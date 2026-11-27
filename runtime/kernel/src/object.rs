//! Kernel objects: dispatcher objects (events, semaphores, mutants, timers,
//! threads), waits, and the handle table.
//!
//! Dispatcher objects live in guest memory, as on the console: the game
//! initialises `KEVENT`s itself and passes their addresses, and reads their
//! state. A `DISPATCHER_HEADER` is `{ u8 Type; u8 Absolute; u8 Size; u8 Inserted;
//! i32 SignalState; LIST_ENTRY WaitListHead; }`. Objects created through `Nt*`
//! calls get a header in kernel memory and a handle that refers to it.
//!
//! One global dispatcher lock guards every state change, as in the console
//! kernel. Each blocked thread waits on its own host condition variable; after
//! a change, the threads whose waits it may satisfy are woken, and they
//! re-check their own conditions.

use std::any::Any;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ppc_rt::Ctx;

use crate::mem;
use crate::status::*;
use crate::thread::{self, Apc, Thread};
use crate::time;

pub const NOTIFICATION_EVENT: u8 = 0;
pub const SYNCHRONIZATION_EVENT: u8 = 1;
pub const MUTANT: u8 = 2;
pub const SEMAPHORE: u8 = 5;
pub const THREAD: u8 = 6;
pub const NOTIFICATION_TIMER: u8 = 8;
pub const SYNCHRONIZATION_TIMER: u8 = 9;

static LOCK: Mutex<()> = Mutex::new(());

pub fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A thread blocked in `wait` or an alertable `delay`: what would let it go
/// on, and its own condition variable (used with the dispatcher lock).
struct Waiter {
    cv: Condvar,
    thread: Arc<Thread>,
    /// Its kernel thread, which may own a mutant.
    me: u32,
    objects: Vec<u32>,
    wait_any: bool,
    alertable: bool,
}

impl Waiter {
    /// True when its wait could now be satisfied (or interrupted by an APC).
    /// Call with the dispatcher lock held.
    fn ready(&self) -> bool {
        if self.alertable && self.thread.has_apcs() {
            return true;
        }
        if self.objects.is_empty() {
            return false;
        }
        if self.wait_any { self.objects.iter().any(|&o| signalled(o, self.me)) } else { self.objects.iter().all(|&o| signalled(o, self.me)) }
    }
}

/// The blocked threads. Locked only while the dispatcher lock is held, or on
/// its own to remove a waiter (lock order: dispatcher, then this).
static WAITERS: Mutex<Vec<Arc<Waiter>>> = Mutex::new(Vec::new());

/// Every millisecond, wakes blocked threads whose waits can go on although
/// nothing woke them. That happens when the game changes a dispatcher
/// object's state in guest memory itself instead of through the kernel. When
/// every state change woke every waiter, the next unrelated change rescued
/// such a wait at once; with targeted wakeups it waited for some poller, and
/// the whole game stalled for 100-250 ms (every thread blocked, one on an
/// event already set). Counted and logged every 10 s.
fn start_rescue() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("dispatcher rescue".into()).spawn(|| {
            let (mut rescued, mut since) = (0u32, Instant::now());
            loop {
                std::thread::sleep(Duration::from_millis(1));
                {
                    let _g = lock();
                    for w in WAITERS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                        if w.ready() {
                            w.cv.notify_one();
                            rescued += 1;
                        }
                    }
                }
                if since.elapsed() >= Duration::from_secs(10) {
                    if rescued > 0 {
                        crate::diag::log(&format!("dispatcher: {rescued} waits found ready without a wakeup in the last 10 s"));
                    }
                    (rescued, since) = (0, Instant::now());
                }
            }
        });
    });
}

/// Registers a waiter until dropped.
struct Registered(Arc<Waiter>);

impl Registered {
    /// Call with the dispatcher lock held.
    fn new(w: Waiter) -> Registered {
        start_rescue();
        let w = Arc::new(w);
        WAITERS.lock().unwrap_or_else(|e| e.into_inner()).push(w.clone());
        Registered(w)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        WAITERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| !Arc::ptr_eq(w, &self.0));
    }
}

/// The blocked threads (diagnostics, `profile::watch_hitches`): each one's
/// thread id, whether any object will do, and its objects with their type and
/// signal state, and whether its wait could go on now (`ready`): a blocked
/// waiter that is ready missed its wakeup.
pub fn describe_waiters() -> Vec<String> {
    let _g = lock();
    WAITERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|w| {
            let objs: Vec<String> = w.objects.iter().map(|&o| format!("{o:08X} type {} state {}", mem::r8(o), state(o))).collect();
            format!(
                "thread {} waits for {} of [{}]{}{}",
                w.thread.id,
                if w.wait_any { "any" } else { "all" },
                objs.join(", "),
                if w.alertable { ", alertable" } else { "" },
                if w.ready() { " -- READY BUT NOT WOKEN" } else { "" }
            )
        })
        .collect()
}

/// The ids of the threads blocked in a wait now (diagnostics: a sampler
/// counts only threads that work). Takes only the waiter list's own lock.
pub fn waiting_thread_ids() -> Vec<u32> {
    WAITERS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|w| w.thread.id).collect()
}

/// Call with the dispatcher lock held, after changing any object's state:
/// wakes the blocked threads whose waits that may satisfy. (Waking every
/// blocked thread on every change, as this did, sent some 35 threads through
/// the dispatcher lock at each event, delaying the one that could run.)
pub fn wake_all() {
    for w in WAITERS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if w.ready() {
            w.cv.notify_one();
        }
    }
}

pub fn init_list_head(a: u32) {
    mem::w32(a, a);
    mem::w32(a + 4, a);
}

/// Initialises a dispatcher header in guest memory.
pub fn init_header(a: u32, kind: u8, size_bytes: u32, signal: i32) {
    mem::w8(a, kind);
    mem::w8(a + 1, 0);
    mem::w8(a + 2, (size_bytes / 4) as u8);
    mem::w8(a + 3, 0);
    mem::w32(a + 4, signal as u32);
    init_list_head(a + 8);
}

fn state(o: u32) -> i32 {
    mem::r32(o + 4) as i32
}

fn signalled(o: u32, me: u32) -> bool {
    match mem::r8(o) {
        NOTIFICATION_EVENT | SYNCHRONIZATION_EVENT | THREAD | NOTIFICATION_TIMER | SYNCHRONIZATION_TIMER => state(o) != 0,
        SEMAPHORE => state(o) > 0,
        MUTANT => state(o) > 0 || mem::r32(o + 0x18) == me,
        t => crate::diag::fatal(&format!(
            "wait on a dispatcher object of type {t} at 0x{o:08X}, which this kernel doesn't implement"
        )),
    }
}

/// Consumes a satisfied wait. Returns true for an abandoned mutant.
fn satisfy(o: u32, me: u32) -> bool {
    match mem::r8(o) {
        SYNCHRONIZATION_EVENT | SYNCHRONIZATION_TIMER => mem::w32(o + 4, 0),
        SEMAPHORE => mem::w32(o + 4, (state(o) - 1) as u32),
        MUTANT => {
            mem::w32(o + 4, (state(o) - 1) as u32);
            mem::w32(o + 0x18, me);
            if mem::r8(o + 0x1C) != 0 {
                mem::w8(o + 0x1C, 0);
                return true;
            }
        }
        _ => {}
    }
    false
}

/// `WWE14_WAIT_STATS=1`: time the game's threads spend in waits and sleeps,
/// by thread, caller and kind, logged every 5 s (the 15 largest).
mod wait_stats {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    type Key = (u32, u32, &'static str);
    static DATA: Mutex<Option<(Instant, HashMap<Key, (u32, u64)>)>> = Mutex::new(None);

    pub fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("WWE14_WAIT_STATS").is_some())
    }

    pub fn record(thread: u32, caller: u32, kind: &'static str, since: Instant) {
        let ns = since.elapsed().as_nanos() as u64;
        let mut d = DATA.lock().unwrap();
        let (start, map) = d.get_or_insert_with(|| (Instant::now(), HashMap::new()));
        let e = map.entry((thread, caller, kind)).or_default();
        e.0 += 1;
        e.1 += ns;
        let secs = start.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        // Only threads that also work: a thread blocked nearly all the time
        // (an idle worker) says nothing about what slows a frame down.
        let mut per_thread: HashMap<u32, u64> = HashMap::new();
        for ((t, _, _), (_, ns)) in map.iter() {
            *per_thread.entry(*t).or_default() += ns;
        }
        let busy = |t: &u32| (per_thread[t] as f64 / 1e9) < 0.9 * secs;
        let mut rows: Vec<_> = map.drain().filter(|((t, _, _), _)| busy(t)).collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.1));
        let lines: Vec<String> = rows
            .iter()
            .take(20)
            .map(|((t, lr, kind), (n, ns))| format!("  thread {t} from 0x{lr:08X} {kind}: {:.1} ms/s in {:.0} waits/s", *ns as f64 / 1e6 / secs, *n as f64 / secs))
            .collect();
        *start = Instant::now();
        drop(d);
        crate::diag::log(&format!("waits over {secs:.1} s:\n{}", lines.join("\n")));
    }
}

/// Waits for one (`wait_any`) or all of the dispatcher objects. Returns the
/// NTSTATUS: `STATUS_WAIT_0 + i`, `STATUS_ABANDONED_WAIT_0 + i`,
/// `STATUS_TIMEOUT` or `STATUS_USER_APC`.
/// With `WWE14_HITCH_WATCH`: logs timed waits and sleeps that lasted over
/// 50 ms when they end (the first 300), with how they ended. A stall in
/// which every thread is blocked ends when one of these does.
fn log_long(c: &Ctx, kind: &str, since: Instant, ended: &str) {
    trace_thread_wait(c, kind, since, ended);
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static N: AtomicU32 = AtomicU32::new(0);
    let ms = since.elapsed().as_millis();
    if ms > 50 && *ON.get_or_init(|| std::env::var_os("WWE14_HITCH_WATCH").is_some()) && N.fetch_add(1, Ordering::Relaxed) < 300 {
        crate::diag::log(&format!(
            "long {kind}: thread {} from 0x{:08X}, {ms} ms, {ended} (frame {})",
            thread::current().id,
            c.lr as u32,
            crate::video::PRESENTED.load(Ordering::Relaxed)
        ));
    }
}

/// `WWE14_TRACE_WAITS=3,38,39`: every sleep and wait of those guest threads,
/// with what was asked, how long it took, the caller and the frame (5,000
/// lines at most). For the 30 Years of WrestleMania intro, whose video
/// thread sleeps a quarter of the time while its sound runs dry (user41).
/// Set when a video with sound starts (a thread names itself "Bink Snd"):
/// the wait trace begins there.
pub static BINK_SOUND_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn trace_thread_wait(c: &Ctx, kind: &str, since: Instant, detail: &str) {
    // Entries are guest thread ids or names ("Bink Snd").
    static WANT: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    static N: AtomicU32 = AtomicU32::new(0);
    let want = WANT.get_or_init(|| {
        std::env::var("WWE14_TRACE_WAITS").map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
    });
    if want.is_empty() || !BINK_SOUND_STARTED.load(Ordering::Relaxed) {
        return;
    }
    let me = thread::current();
    let (id, name) = (me.id, me.name());
    if !want.iter().any(|w| *w == id.to_string() || *w == name) || N.fetch_add(1, Ordering::Relaxed) >= 8000 {
        return;
    }
    crate::diag::log(&format!(
        "wait trace: thread {id} {kind} from 0x{:08X}: {detail}, took {:.2} ms (frame {})",
        c.lr as u32,
        since.elapsed().as_secs_f64() * 1000.0,
        crate::video::PRESENTED.load(Ordering::Relaxed)
    ));
}

pub fn wait(c: &mut Ctx, objects: &[u32], wait_any: bool, alertable: bool, deadline: Option<Instant>) -> u32 {
    if deadline.is_none() {
        let since = Instant::now();
        let r = if wait_stats::on() { wait_with_stats(c, objects, wait_any, alertable, deadline) } else { wait_inner(c, objects, wait_any, alertable, deadline) };
        trace_thread_wait(c, "wait", since, &format!("{} object(s) {:08X?}, no timeout", objects.len(), objects));
        return r;
    }
    if deadline.is_some() {
        let since = Instant::now();
        let r = if wait_stats::on() { wait_with_stats(c, objects, wait_any, alertable, deadline) } else { wait_inner(c, objects, wait_any, alertable, deadline) };
        log_long(c, "timed wait", since, if r == STATUS_TIMEOUT { "timed out" } else { "signalled" });
        return r;
    }
    if !wait_stats::on() {
        return wait_inner(c, objects, wait_any, alertable, deadline);
    }
    wait_with_stats(c, objects, wait_any, alertable, deadline)
}

fn wait_with_stats(c: &mut Ctx, objects: &[u32], wait_any: bool, alertable: bool, deadline: Option<Instant>) -> u32 {
    let since = Instant::now();
    let (caller, me) = (c.lr as u32, thread::current().id);
    let r = wait_inner(c, objects, wait_any, alertable, deadline);
    let kind = match (deadline, r) {
        (None, _) => "wait",
        (Some(_), STATUS_TIMEOUT) => "timed wait, timed out",
        (Some(_), _) => "timed wait",
    };
    wait_stats::record(me, caller, kind, since);
    r
}

fn wait_inner(c: &mut Ctx, objects: &[u32], wait_any: bool, alertable: bool, deadline: Option<Instant>) -> u32 {
    let t = thread::current();
    let me = t.kthread;
    let mut g = lock();
    // Registered at the first block, under the same hold of the lock as the
    // failed check, so no wake can fall between them.
    let mut waiter: Option<Registered> = None;
    loop {
        if alertable && t.has_apcs() {
            drop(g);
            drop(waiter);
            thread::deliver_apcs(c);
            return STATUS_USER_APC;
        }
        if wait_any {
            for (i, &o) in objects.iter().enumerate() {
                if signalled(o, me) {
                    let abandoned = satisfy(o, me);
                    wake_all();
                    return i as u32 + if abandoned { STATUS_ABANDONED_WAIT_0 } else { STATUS_WAIT_0 };
                }
            }
        } else if objects.iter().all(|&o| signalled(o, me)) {
            for &o in objects {
                satisfy(o, me);
            }
            wake_all();
            return STATUS_WAIT_0;
        }
        let w = waiter
            .get_or_insert_with(|| Registered::new(Waiter { cv: Condvar::new(), thread: t.clone(), me, objects: objects.to_vec(), wait_any, alertable }))
            .0
            .clone();
        match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return STATUS_TIMEOUT;
                }
                g = w.cv.wait_timeout(g, d - now).unwrap_or_else(|e| e.into_inner()).0;
            }
            None => g = w.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
        }
    }
}

/// Sleeps until `deadline` (`KeDelayExecutionThread`), delivering APCs if alertable.
pub fn delay(c: &mut Ctx, alertable: bool, deadline: Instant) -> u32 {
    let me = thread::current();
    me.sleeping.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = delay_timed(c, alertable, deadline);
    me.sleeping.store(false, std::sync::atomic::Ordering::Relaxed);
    r
}

fn delay_timed(c: &mut Ctx, alertable: bool, deadline: Instant) -> u32 {
    if !wait_stats::on() {
        let since = Instant::now();
        let asked = deadline.saturating_duration_since(since).as_secs_f64() * 1000.0;
        let r = delay_inner(c, alertable, deadline);
        log_long(c, "sleep", since, &format!("asked {asked:.2} ms"));
        return r;
    }
    let since = Instant::now();
    let (caller, me) = (c.lr as u32, thread::current().id);
    let asked = deadline.saturating_duration_since(since);
    let r = delay_inner(c, alertable, deadline);
    let kind = match asked.as_micros() {
        0 => "sleep 0",
        1..=1999 => "sleep under 2 ms",
        _ => "sleep",
    };
    wait_stats::record(me, caller, kind, since);
    r
}

fn delay_inner(c: &mut Ctx, alertable: bool, deadline: Instant) -> u32 {
    let t = thread::current();
    let mut g = lock();
    let mut waiter: Option<Registered> = None;
    loop {
        if alertable && t.has_apcs() {
            drop(g);
            drop(waiter);
            thread::deliver_apcs(c);
            return STATUS_USER_APC;
        }
        let now = Instant::now();
        if now >= deadline {
            return STATUS_SUCCESS;
        }
        if !alertable {
            drop(g);
            std::thread::sleep(deadline - now);
            return STATUS_SUCCESS;
        }
        // Alertable: only a queued APC ends the sleep early.
        let w = waiter
            .get_or_insert_with(|| Registered::new(Waiter { cv: Condvar::new(), thread: t.clone(), me: t.kthread, objects: Vec::new(), wait_any: true, alertable }))
            .0
            .clone();
        g = w.cv.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// Converts a guest timeout pointer (`LARGE_INTEGER*`, 100 ns units; negative
/// = relative, positive = absolute system time, null = forever).
pub fn deadline(timeout_ptr: u32) -> Option<Instant> {
    if timeout_ptr == 0 {
        return None;
    }
    Some(deadline_of(mem::r64(timeout_ptr) as i64))
}

pub fn deadline_of(t: i64) -> Instant {
    let now = Instant::now();
    if t < 0 {
        now + Duration::from_nanos((t.unsigned_abs()).saturating_mul(100))
    } else {
        let rel = t - time::system_time() as i64;
        if rel <= 0 { now } else { now + Duration::from_nanos(rel as u64 * 100) }
    }
}

// ---- Object operations ------------------------------------------------------------

pub fn set_event(o: u32) -> i32 {
    let _g = lock();
    let prev = state(o);
    mem::w32(o + 4, 1);
    wake_all();
    prev
}

pub fn reset_event(o: u32) -> i32 {
    let _g = lock();
    let prev = state(o);
    mem::w32(o + 4, 0);
    prev
}

pub fn release_semaphore(o: u32, count: i32) -> Result<i32, u32> {
    let _g = lock();
    let prev = state(o);
    let limit = mem::r32(o + 0x10) as i32;
    if count <= 0 || prev.checked_add(count).is_none_or(|n| n > limit) {
        return Err(STATUS_SEMAPHORE_LIMIT_EXCEEDED);
    }
    mem::w32(o + 4, (prev + count) as u32);
    wake_all();
    Ok(prev)
}

pub fn release_mutant(o: u32, abandoned: bool) -> Result<i32, u32> {
    let me = thread::current().kthread;
    let _g = lock();
    let prev = state(o);
    if mem::r32(o + 0x18) != me {
        return Err(STATUS_MUTANT_NOT_OWNED);
    }
    let now = prev + 1;
    mem::w32(o + 4, now as u32);
    if now > 0 {
        mem::w32(o + 0x18, 0);
        mem::w8(o + 0x1C, abandoned as u8);
    }
    wake_all();
    Ok(prev)
}

/// Allocates a dispatcher object in kernel memory.
pub fn new_event(manual_reset: bool, initial: bool) -> u32 {
    let o = mem::kalloc(0x10, 16);
    init_header(o, if manual_reset { NOTIFICATION_EVENT } else { SYNCHRONIZATION_EVENT }, 0x10, initial as i32);
    o
}

pub fn new_semaphore(initial: i32, limit: i32) -> u32 {
    let o = mem::kalloc(0x14, 16);
    init_header(o, SEMAPHORE, 0x14, initial);
    mem::w32(o + 0x10, limit as u32);
    o
}

pub fn new_mutant(owned: bool) -> u32 {
    let o = mem::kalloc(0x20, 16);
    init_header(o, MUTANT, 0x20, if owned { 0 } else { 1 });
    init_list_head(o + 0x10);
    mem::w32(o + 0x18, if owned { thread::current().kthread } else { 0 });
    o
}

// ---- Timers ------------------------------------------------------------------------

pub struct Timer {
    pub header: u32,
    state: Mutex<TimerState>,
}

#[derive(Default)]
struct TimerState {
    generation: u64,
    period_ms: u32,
    apc: Option<(Arc<Thread>, Apc)>,
}

struct Due {
    at: Instant,
    timer: Arc<Timer>,
    generation: u64,
}

impl PartialEq for Due {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at
    }
}
impl Eq for Due {}
impl PartialOrd for Due {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Due {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.at.cmp(&self.at) // min-heap
    }
}

static TIMERS: Mutex<BinaryHeap<Due>> = Mutex::new(BinaryHeap::new());
static TIMER_WAKE: Condvar = Condvar::new();

impl Timer {
    pub fn new(synchronization: bool) -> Arc<Timer> {
        let h = mem::kalloc(0x28, 16);
        init_header(h, if synchronization { SYNCHRONIZATION_TIMER } else { NOTIFICATION_TIMER }, 0x28, 0);
        Arc::new(Timer { header: h, state: Mutex::new(TimerState::default()) })
    }

    /// `NtSetTimerEx`. Returns the previous signal state.
    pub fn set(self: &Arc<Timer>, due: i64, period_ms: u32, apc: Option<Apc>) -> bool {
        let prev = {
            let _g = lock();
            let p = state(self.header) != 0;
            mem::w32(self.header + 4, 0);
            p
        };
        let generation = {
            let mut s = self.state.lock().unwrap();
            s.generation += 1;
            s.period_ms = period_ms;
            s.apc = apc.map(|a| (thread::current(), a));
            s.generation
        };
        TIMERS.lock().unwrap().push(Due { at: deadline_of(due), timer: self.clone(), generation });
        TIMER_WAKE.notify_all();
        prev
    }

    pub fn cancel(&self) -> bool {
        self.state.lock().unwrap().generation += 1;
        let _g = lock();
        state(self.header) != 0
    }
}

/// The kernel's timer service: signals timers when they come due.
pub fn start_timer_service() {
    std::thread::Builder::new()
        .name("kernel-timers".into())
        .spawn(|| {
            let mut heap = TIMERS.lock().unwrap();
            loop {
                let now = Instant::now();
                match heap.peek() {
                    Some(d) if d.at <= now => {
                        let d = heap.pop().unwrap();
                        let mut s = d.timer.state.lock().unwrap();
                        if s.generation != d.generation {
                            continue;
                        }
                        {
                            let _g = lock();
                            mem::w32(d.timer.header + 4, 1);
                            wake_all();
                        }
                        if let Some((t, apc)) = &s.apc {
                            t.queue_apc(*apc);
                        }
                        if s.period_ms > 0 {
                            let at = d.at + Duration::from_millis(s.period_ms as u64);
                            let generation = s.generation;
                            drop(s);
                            heap.push(Due { at, timer: d.timer.clone(), generation });
                        } else {
                            s.apc = None;
                        }
                    }
                    Some(d) => {
                        let wait = d.at - now;
                        heap = TIMER_WAKE.wait_timeout(heap, wait).unwrap().0;
                    }
                    None => heap = TIMER_WAKE.wait(heap).unwrap(),
                }
            }
        })
        .expect("timer thread");
}

// ---- Handles ---------------------------------------------------------------------

#[derive(Clone)]
pub enum Object {
    /// A dispatcher object in guest memory (event, semaphore, mutant).
    Dispatcher(u32),
    Thread(Arc<Thread>),
    Timer(Arc<Timer>),
    File(Arc<crate::file::File>),
    /// XAM and other kernel-private objects (enumerators, listeners, ...),
    /// with the dispatcher header to wait on if they are waitable (a
    /// notification listener is signalled while notifications are queued).
    Other(Arc<dyn Any + Send + Sync>, Option<u32>),
}

impl Object {
    /// The dispatcher header to wait on, if the object is waitable.
    pub fn waitable(&self) -> Option<u32> {
        match self {
            Object::Dispatcher(a) => Some(*a),
            Object::Thread(t) => Some(t.kthread),
            Object::Timer(t) => Some(t.header),
            Object::File(f) => Some(f.event),
            Object::Other(_, w) => *w,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Object::Dispatcher(a) => match mem::r8(*a) {
                NOTIFICATION_EVENT | SYNCHRONIZATION_EVENT => "event",
                SEMAPHORE => "semaphore",
                MUTANT => "mutant",
                _ => "dispatcher object",
            },
            Object::Thread(_) => "thread",
            Object::Timer(_) => "timer",
            Object::File(_) => "file",
            Object::Other(..) => "kernel object",
        }
    }
}

pub const CURRENT_PROCESS: u32 = 0xFFFF_FFFF;
pub const CURRENT_THREAD: u32 = 0xFFFF_FFFE;

static HANDLES: Mutex<Option<HashMap<u32, Object>>> = Mutex::new(None);
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(0xF800_0004);
static NAMES: Mutex<Option<HashMap<String, Object>>> = Mutex::new(None);

pub fn insert(o: Object) -> u32 {
    let h = NEXT_HANDLE.fetch_add(4, Ordering::Relaxed);
    HANDLES.lock().unwrap().get_or_insert_with(HashMap::new).insert(h, o);
    h
}

pub fn get(h: u32) -> Option<Object> {
    if h == CURRENT_THREAD {
        return Some(Object::Thread(thread::current()));
    }
    HANDLES.lock().unwrap().as_ref()?.get(&h).cloned()
}

pub fn close(h: u32) -> bool {
    if h == CURRENT_THREAD || h == CURRENT_PROCESS {
        return true;
    }
    HANDLES.lock().unwrap().as_mut().and_then(|m| m.remove(&h)).is_some()
}

/// Typed lookup for XAM's private objects.
pub fn get_other<T: Any + Send + Sync>(h: u32) -> Option<Arc<T>> {
    match get(h)? {
        Object::Other(o, _) => o.downcast::<T>().ok(),
        _ => None,
    }
}

/// Finds a handle referring to the dispatcher object at `addr` (for
/// `ObReferenceObjectByHandle` round trips), or creates one.
pub fn lookup_named(name: &str) -> Option<Object> {
    NAMES.lock().unwrap().as_ref()?.get(&name.to_ascii_lowercase()).cloned()
}

pub fn register_name(name: &str, o: Object) {
    NAMES.lock().unwrap().get_or_insert_with(HashMap::new).insert(name.to_ascii_lowercase(), o);
}
