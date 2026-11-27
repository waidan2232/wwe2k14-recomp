//! Guest threads. Each guest thread is a host thread with its own register
//! context, a stack in guest memory (the game passes pointers to locals), and
//! the per-thread blocks the game reads through `r13`:
//!
//! - **PCR** (`r13`): `+0x000` static TLS data, `+0x030` self, `+0x070`/`+0x074`
//!   stack base/limit, `+0x100` current `KTHREAD`, `+0x10C` processor number,
//!   `+0x150` DPC-active flag. These are the only PCR fields the game reads
//!   (`analyse r13`).
//! - **KTHREAD**: a dispatcher header (type 6, signalled when the thread ends),
//!   stack base/limit at `+0x5C`/`+0x60`, TLS at `+0x68`, thread id at `+0x14C`,
//!   start address at `+0x150`, last error at `+0x160` (XAPI's
//!   `GetCurrentThreadId` and `SetLastError` read these directly).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use ppc_rt::{Ctx, GuestFn};

use crate::mem;
use crate::object;

/// A user-mode APC: `routine(context, arg1, arg2)`.
#[derive(Clone, Copy, Debug)]
pub struct Apc {
    pub routine: u32,
    pub context: u32,
    pub arg1: u32,
    pub arg2: u32,
}

pub struct Thread {
    pub id: u32,
    pub kthread: u32,
    pub pcr: u32,
    pub tls_slots: u32,
    pub stack_low: u32,
    pub stack_high: u32,
    pub name: Mutex<String>,
    /// Pushed and inspected under the dispatcher lock.
    pub apcs: Mutex<VecDeque<Apc>>,
    suspend: Mutex<u32>,
    resumed: Condvar,
    pub priority: AtomicI32,
    pub affinity: AtomicU32,
    pub exit_code: AtomicU32,
    pub finished: AtomicBool,
    /// `KeEnterCriticalRegion` nesting.
    pub critical_region: AtomicI32,
    /// Interrupt request level for the spinlock API.
    pub irql: AtomicU32,
    /// Guest stack pointer when an import was entered (for diagnostics).
    pub host_thread: Mutex<Option<std::thread::Thread>>,
    /// The thread's register context while it runs (diagnostics only).
    pub ctx: std::sync::atomic::AtomicPtr<Ctx>,
    /// In `KeDelayExecutionThread` (diagnostics: a sampler skips sleepers).
    pub sleeping: AtomicBool,
    /// Host thread handle (0 until the thread starts), for applying the
    /// guest's affinity and priority.
    host_handle: std::sync::atomic::AtomicUsize,
}

// ---- Scheduling ----------------------------------------------------------------------
//
// The console runs each thread only on the hardware threads its affinity
// names (six: three cores, two threads each), and on each hardware thread
// the highest-priority ready thread runs until it blocks. Games lay out their
// threads around this. The kernel keeps the same contract with the host's
// scheduler: guest hardware thread n is host logical processor
// `FIRST_HOST_CPU + n`, and the guest priority maps to the host priority.

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> *mut std::ffi::c_void;
    fn GetCurrentProcess() -> isize;
    fn DuplicateHandle(sp: isize, s: isize, tp: isize, t: *mut isize, access: u32, inherit: i32, options: u32) -> i32;
    fn SetThreadAffinityMask(thread: isize, mask: usize) -> usize;
    fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    fn SetThreadPriorityBoost(thread: isize, disable: i32) -> i32;
    fn CloseHandle(h: isize) -> i32;
}

const DUPLICATE_SAME_ACCESS: u32 = 2;

/// Host logical processors for the six guest hardware threads, when the host
/// has enough of them (else threads float).
///
/// With 12 or more, each guest hardware thread gets a physical core of its
/// own: every other logical processor from `cpus - 12` up (Windows numbers a
/// core's SMT threads next to each other), leaving their siblings to the
/// host's threads. Packing the six into three cores' SMT pairs, as the console
/// does, let a loading thread on one slow the render thread on its sibling
/// (the main menu dropped to 21 fps in one run while the game loaded).
///
/// By default guest threads are not pinned at all (`WWE14_AFFINITY=pinned`
/// pins them as described). Pinned, a thread woken while another guest thread
/// of the same hardware thread was busy waited out the host's scheduling
/// quantum: 15 ms sleeps took 50-80 ms, and the game stalled for 100-300 ms
/// at camera cuts with every thread blocked behind one that could not run.
/// The guest's priorities still apply.
fn host_cpu_mask(guest: u32) -> Option<usize> {
    static PINNED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*PINNED.get_or_init(|| std::env::var("WWE14_AFFINITY").is_ok_and(|v| v == "pinned")) {
        return None;
    }
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let guest = (guest & 0x3F) as usize;
    if cpus < 8 || guest == 0 {
        return None;
    }
    if cpus >= 12 {
        let first = cpus - 12;
        return Some((0..6).filter(|n| guest & (1 << n) != 0).fold(0, |m, n| m | 1 << (first + 2 * n)));
    }
    Some(guest << (cpus - 6)) // the last six: the host's own threads tend to start low
}

/// Host priority for a guest base priority increment (XAPI's
/// `SetThreadPriority` values: -15 idle .. 2 highest, 15 time critical).
fn host_priority(guest: i32) -> i32 {
    match guest {
        i32::MIN..=-15 => -15,
        -14..=-2 => -2,
        -1 => -1,
        0 => 0,
        1 => 1,
        2..=14 => 2,
        _ => 15,
    }
}

/// Unwind payload for `ExTerminateThread`.
pub struct ThreadExit(pub u32);

thread_local! {
    static CURRENT: RefCell<Option<Arc<Thread>>> = const { RefCell::new(None) };
    static CTX: Cell<*mut Ctx> = const { Cell::new(core::ptr::null_mut()) };
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// Ids of threads that have ended, reused most recently freed first, as the
/// console kernel reuses client ids (thread ids are handle-table entries).
/// Games key per-thread tables by id and rely on this: WWE 2K14 registers
/// threads in a 23-slot heap table that a thread ending with
/// `ExTerminateThread` never leaves, and the next thread created takes over
/// that slot through the reused id.
static FREE_IDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static THREADS: Mutex<Vec<Arc<Thread>>> = Mutex::new(Vec::new());
static TLS_BITMAP: Mutex<u64> = Mutex::new(0);

/// Guest image facts the thread code needs.
pub struct ThreadSetup {
    pub tls_slots: u32,
    pub tls_template: u32,
    pub tls_raw_size: u32,
    pub tls_data_size: u32,
    pub default_stack: u32,
}

static SETUP: std::sync::OnceLock<ThreadSetup> = std::sync::OnceLock::new();

pub fn configure(s: ThreadSetup) {
    SETUP.set(s).ok().expect("thread setup configured twice");
}

fn setup() -> &'static ThreadSetup {
    SETUP.get().expect("thread setup")
}

/// Lowers the calling host thread below every guest thread. For background
/// work (texture hashing and conversion, shader loading, freeing frames):
/// each guest thread may run on one logical processor only, and a pool of
/// normal-priority host workers busy at a camera cut kept guest threads from
/// running for whole scheduler quanta, stalling the game for 100-250 ms with
/// every thread blocked behind one that was ready but not running.
pub fn make_background() {
    const THREAD_PRIORITY_LOWEST: i32 = -2;
    unsafe { SetThreadPriority(GetCurrentThread() as isize, THREAD_PRIORITY_LOWEST) };
}

/// Raises the calling host thread above the game's own threads: for the
/// renderer, which every frame waits on and which works only when a frame
/// arrives. At normal priority, busy game threads (a load) kept it off the CPU
/// while the game waited on it for 280-310 ms (2026-11-25).
pub fn make_urgent() {
    const THREAD_PRIORITY_HIGHEST: i32 = 2;
    unsafe { SetThreadPriority(GetCurrentThread() as isize, THREAD_PRIORITY_HIGHEST) };
}

pub fn current() -> Arc<Thread> {
    CURRENT.with(|c| c.borrow().clone()).expect("FATAL: kernel call from a host thread that isn't a guest thread")
}

pub fn try_current() -> Option<Arc<Thread>> {
    CURRENT.try_with(|c| c.borrow().clone()).ok().flatten()
}

/// The current guest thread's register context (diagnostics only).
pub fn current_ctx() -> *mut Ctx {
    CTX.try_with(|c| c.get()).unwrap_or(core::ptr::null_mut())
}

pub fn all() -> Vec<Arc<Thread>> {
    THREADS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn by_kthread(kthread: u32) -> Option<Arc<Thread>> {
    all().into_iter().find(|t| t.kthread == kthread)
}

impl Thread {
    pub fn name(&self) -> String {
        self.name.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The host thread handle (0 before the thread starts or after it ends).
    pub fn host_handle(&self) -> usize {
        self.host_handle.load(Ordering::Relaxed)
    }

    pub fn set_name(&self, name: &str) {
        *self.name.lock().unwrap() = name.to_string();
    }

    /// `KeSetAffinityThread`: the processor number the game sees in
    /// `PCR+0x10C`, and the host processor the thread may run on.
    pub fn set_affinity(&self, mask: u32) {
        self.affinity.store(mask, Ordering::Relaxed);
        if mask != 0 {
            mem::w8(self.pcr + 0x10C, mask.trailing_zeros().min(5) as u8);
        }
        self.apply_scheduling();
    }

    /// `KeSetBasePriorityThread`: returns the previous priority.
    pub fn set_priority(&self, priority: i32) -> i32 {
        let old = self.priority.swap(priority, Ordering::Relaxed);
        self.apply_scheduling();
        old
    }

    fn apply_scheduling(&self) {
        let h = self.host_handle.load(Ordering::Relaxed) as isize;
        if h == 0 {
            return; // applied when the thread starts
        }
        if let Some(mask) = host_cpu_mask(self.affinity.load(Ordering::Relaxed)) {
            unsafe { SetThreadAffinityMask(h, mask) };
        }
        unsafe { SetThreadPriority(h, host_priority(self.priority.load(Ordering::Relaxed))) };
        // The console's scheduler has fixed priorities: a thread woken by an
        // event doesn't get the temporary boost Windows gives it, which would
        // let it preempt the thread that woke it on the same processor. WWE
        // 2K14's job kick (`sub_8251D1F0`: set start, clear done) deadlocks if
        // its worker runs the whole job between those two calls.
        unsafe { SetThreadPriorityBoost(h, 1) };
        if crate::diag::TRACE_THREADS.load(Ordering::Relaxed) {
            crate::diag::log(&format!(
                "thread {} '{}': affinity {:#04X}, priority {}",
                self.id,
                self.name(),
                self.affinity.load(Ordering::Relaxed),
                self.priority.load(Ordering::Relaxed)
            ));
        }
    }

    pub fn resume(&self) -> u32 {
        let mut s = self.suspend.lock().unwrap();
        let prev = *s;
        *s = s.saturating_sub(1);
        self.resumed.notify_all();
        prev
    }

    pub fn queue_apc(&self, apc: Apc) {
        let _g = object::lock();
        self.apcs.lock().unwrap().push_back(apc);
        object::wake_all();
    }

    pub fn has_apcs(&self) -> bool {
        !self.apcs.lock().unwrap().is_empty()
    }
}

/// Builds a thread's guest-visible blocks. `stack_size` 0 = the image default.
fn build(start_address: u32, stack_size: u32, affinity: u32, name: String) -> Arc<Thread> {
    let s = setup();
    let id = FREE_IDS.lock().unwrap().pop().unwrap_or_else(|| NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let stack_size = if stack_size == 0 { s.default_stack } else { stack_size }.max(0x1_0000);
    let stack_low = mem::alloc_stack(stack_size);
    let stack_high = stack_low + stack_size.next_multiple_of(0x1_0000);

    // TLS: dynamic slots, then a copy of the image's static TLS template.
    let slots_size = s.tls_slots * 4;
    let tls = mem::kalloc(slots_size + s.tls_data_size.max(s.tls_raw_size) + 16, 16);
    let tls_data = tls + slots_size.next_multiple_of(16);
    if s.tls_raw_size > 0 {
        let template = mem::read(s.tls_template, s.tls_raw_size as usize);
        mem::write(tls_data, &template);
    }

    let pcr = mem::kalloc(0x1000, 0x1000);
    let kthread = mem::kalloc(0x400, 16);
    // KTHREAD dispatcher header: type 6 (thread), size in words, not signalled.
    mem::w8(kthread, object::THREAD);
    mem::w8(kthread + 2, (0x400 / 4) as u8);
    mem::w32(kthread + 4, 0);
    object::init_list_head(kthread + 8);
    object::init_list_head(kthread + 0x10);
    mem::w32(kthread + 0x5C, stack_high);
    mem::w32(kthread + 0x60, stack_low);
    mem::w32(kthread + 0x64, stack_high);
    mem::w32(kthread + 0x68, tls_data);
    mem::w32(kthread + 0x14C, id);
    mem::w32(kthread + 0x150, start_address);

    mem::w32(pcr, tls_data);
    mem::w32(pcr + 0x30, pcr);
    mem::w32(pcr + 0x70, stack_high);
    mem::w32(pcr + 0x74, stack_low);
    mem::w32(pcr + 0x100, kthread);
    mem::w8(pcr + 0x10C, 0);
    mem::w32(pcr + 0x150, 0);

    let t = Arc::new(Thread {
        id,
        kthread,
        pcr,
        tls_slots: tls,
        stack_low,
        stack_high,
        name: Mutex::new(name),
        apcs: Mutex::new(VecDeque::new()),
        suspend: Mutex::new(0),
        resumed: Condvar::new(),
        priority: AtomicI32::new(0),
        affinity: AtomicU32::new(0),
        exit_code: AtomicU32::new(0),
        finished: AtomicBool::new(false),
        critical_region: AtomicI32::new(0),
        irql: AtomicU32::new(0),
        host_thread: Mutex::new(None),
        ctx: std::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        sleeping: AtomicBool::new(false),
        host_handle: std::sync::atomic::AtomicUsize::new(0),
    });
    t.set_affinity(affinity);
    THREADS.lock().unwrap().push(t.clone());
    t
}

/// What a new guest thread runs.
pub enum Entry {
    /// `start(context)`, or `xapi_startup(start, context)` when the XAPI
    /// startup routine is given (as `ExCreateThread` does).
    Guest { start: u32, context: u32, xapi_startup: u32 },
    /// Native code that gets the thread's context (kernel worker threads that
    /// call into the game: DPCs, timers, audio).
    Native(Box<dyn FnOnce(&mut Ctx) + Send>),
}

/// Host stack for guest threads: recompiled functions keep their locals on
/// the host stack, so deep guest recursion needs room.
const HOST_STACK: usize = 64 << 20;

/// Creates a guest thread. It starts running unless `suspended`.
pub fn spawn(entry: Entry, stack_size: u32, affinity: u32, suspended: bool, name: String) -> Arc<Thread> {
    let start = match &entry {
        Entry::Guest { start, .. } => *start,
        Entry::Native(_) => 0,
    };
    let t = build(start, stack_size, affinity, name);
    *t.suspend.lock().unwrap() = suspended as u32;
    let t2 = t.clone();
    let handle = std::thread::Builder::new()
        .name(format!("guest-{}", t.id))
        .stack_size(HOST_STACK)
        .spawn(move || run(t2, entry))
        .expect("spawn host thread");
    *t.host_thread.lock().unwrap() = Some(handle.thread().clone());
    t
}

fn run(t: Arc<Thread>, entry: Entry) {
    let mut ctx = Ctx::new();
    CURRENT.with(|c| *c.borrow_mut() = Some(t.clone()));
    CTX.with(|c| c.set(&mut *ctx));
    t.ctx.store(&mut *ctx, Ordering::Relaxed);
    crate::diag::name_host_thread(&t.name());
    // A real handle to this host thread (the pseudo-handle only works from
    // inside it), so other threads can apply affinity and priority changes.
    let mut h: isize = 0;
    let ok = unsafe {
        DuplicateHandle(GetCurrentProcess(), GetCurrentThread() as isize, GetCurrentProcess(), &mut h, 0, 0, DUPLICATE_SAME_ACCESS)
    };
    if ok != 0 {
        t.host_handle.store(h as usize, Ordering::Relaxed);
        t.apply_scheduling();
    }
    {
        let mut s = t.suspend.lock().unwrap();
        while *s > 0 {
            s = t.resumed.wait(s).unwrap();
        }
    }
    // Initial stack: room above r1 for the callee's parameter save area, and a
    // zero back chain so stack walks stop here.
    let sp = t.stack_high - 0x100;
    mem::w32(sp, 0);
    ctx.r[1] = sp as u64;
    ctx.r[13] = t.pcr as u64;
    let c: &mut Ctx = &mut ctx;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match entry {
        Entry::Guest { start, context, xapi_startup } => {
            if xapi_startup != 0 {
                call(c, xapi_startup, &[start as u64, context as u64]);
            } else {
                call(c, start, &[context as u64]);
            }
            c.r[3] as u32
        }
        Entry::Native(f) => {
            f(c);
            0
        }
    }));
    let code = match result {
        Ok(code) => code,
        Err(payload) => match payload.downcast::<ThreadExit>() {
            Ok(e) => e.0,
            Err(payload) => {
                if let Some(j) = payload.downcast_ref::<ppc_rt::LongJmp>() {
                    crate::diag::fatal_exit(&format!(
                        "guest longjmp to 0x{:08X} (r1 0x{:08X}) found no matching setjmp on thread {} '{}'",
                        j.target,
                        j.r1,
                        t.id,
                        t.name()
                    ));
                }
                // A fatal diagnostic already printed by the panic hook.
                crate::diag::exit_after_fatal();
            }
        },
    };
    finish(&t, code);
}

fn finish(t: &Arc<Thread>, code: u32) {
    mem::free_stack(t.stack_low);
    let h = t.host_handle.swap(0, Ordering::Relaxed);
    if h != 0 {
        unsafe { CloseHandle(h as isize) };
    }
    t.exit_code.store(code, Ordering::Relaxed);
    t.finished.store(true, Ordering::Relaxed);
    {
        let _g = object::lock();
        mem::w32(t.kthread + 4, 1);
        object::wake_all();
    }
    crate::diag::trace_thread(&format!("thread {} '{}' exited with 0x{code:08X}", t.id, t.name()));
    FREE_IDS.lock().unwrap().push(t.id);
    CTX.with(|c| c.set(core::ptr::null_mut()));
}

/// Calls the guest function at `addr` on the current thread with up to eight
/// integer arguments, and returns r3. Native code (the kernel) calls into the
/// game this way: thread entry points, APCs, DPCs, timer and XAM callbacks.
pub fn call(c: &mut Ctx, addr: u32, args: &[u64]) -> u64 {
    assert!(args.len() <= 8);
    let f: GuestFn = ppc_rt::lookup(addr).unwrap_or_else(|| {
        crate::diag::fatal(&format!("kernel callback to 0x{addr:08X}, which is not a recompiled function"))
    });
    let saved_r1 = c.r[1];
    let saved_lr = c.lr;
    // A frame of our own below the caller's, with a back chain.
    let sp = ((saved_r1 as u32).wrapping_sub(0x100)) & !0xF;
    mem::w32(sp, saved_r1 as u32);
    c.r[1] = sp as u64;
    for (i, &a) in args.iter().enumerate() {
        c.r[3 + i] = a;
    }
    c.lr = 0;
    unsafe { f(c, mem::base()) };
    let ret = c.r[3];
    c.r[1] = saved_r1;
    c.lr = saved_lr;
    ret
}

/// Runs the APCs queued for the current thread (alertable waits).
pub fn deliver_apcs(c: &mut Ctx) -> bool {
    let t = current();
    let mut any = false;
    loop {
        let next = {
            let _g = object::lock();
            t.apcs.lock().unwrap().pop_front()
        };
        let Some(apc) = next else { break };
        any = true;
        if apc.routine != 0 {
            call(c, apc.routine, &[apc.context as u64, apc.arg1 as u64, apc.arg2 as u64]);
        }
    }
    any
}

/// `ExTerminateThread`: unwinds the current host thread to its entry.
pub fn exit_current(code: u32) -> ! {
    std::panic::resume_unwind(Box::new(ThreadExit(code)))
}

// ---- TLS slots (KeTlsAlloc) ----------------------------------------------------------

pub fn tls_alloc() -> u32 {
    let n = setup().tls_slots.min(64);
    let mut bits = TLS_BITMAP.lock().unwrap();
    for i in 0..n {
        if *bits & (1 << i) == 0 {
            *bits |= 1 << i;
            // A fresh slot reads as 0 on every thread.
            for t in all() {
                mem::w32(t.tls_slots + 4 * i, 0);
            }
            return i;
        }
    }
    u32::MAX
}

pub fn tls_free(i: u32) -> bool {
    let mut bits = TLS_BITMAP.lock().unwrap();
    if i >= 64 || *bits & (1 << i) == 0 {
        return false;
    }
    *bits &= !(1 << i);
    true
}

pub fn tls_get(i: u32) -> u32 {
    if i >= setup().tls_slots {
        return 0;
    }
    mem::r32(current().tls_slots + 4 * i)
}

pub fn tls_set(i: u32, v: u32) -> bool {
    if i >= setup().tls_slots {
        return false;
    }
    mem::w32(current().tls_slots + 4 * i, v);
    true
}
