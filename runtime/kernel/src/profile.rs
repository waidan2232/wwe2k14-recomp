//! A sampling profiler for the running game (`--profile START:SECONDS`).
//!
//! From `START` seconds after boot, for `SECONDS`, a host thread stops each
//! guest thread briefly every 2 ms (`SuspendThread`), reads where its host
//! code is (`GetThreadContext`, the instruction pointer), and lets it go.
//! Afterwards the addresses are resolved to function names with DbgHelp (the
//! binary's own debug information: recompiled functions are `sub_XXXXXXXX`)
//! and the busiest functions are logged, overall and per thread. Host
//! diagnostics only: nothing here runs or inspects guest code.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::{diag, thread};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SuspendThread(thread: isize) -> u32;
    fn ResumeThread(thread: isize) -> u32;
    fn GetThreadContext(thread: isize, context: *mut c_void) -> i32;
    fn GetCurrentProcess() -> isize;
    fn GetCurrentThread() -> *mut c_void;
    fn DuplicateHandle(source_process: isize, source: isize, target_process: isize, target: *mut isize, access: u32, inherit: i32, options: u32) -> i32;
}

/// Host threads sampled besides the guest's (the renderer): an id above the
/// guest's, a name and a real handle.
static HOST_THREADS: std::sync::Mutex<Vec<(u32, String, isize)>> = std::sync::Mutex::new(Vec::new());

/// Adds the calling host thread to what the profiler samples.
pub fn register_host_thread(name: &str) {
    let mut handle = 0isize;
    // DUPLICATE_SAME_ACCESS: a real handle for the pseudo-handle of this thread.
    let ok = unsafe {
        let p = GetCurrentProcess();
        DuplicateHandle(p, GetCurrentThread() as isize, p, &mut handle, 0, 0, 2) != 0
    };
    if ok {
        let mut v = HOST_THREADS.lock().unwrap();
        let id = 100_000 + v.len() as u32;
        v.push((id, name.to_string(), handle));
    }
}

#[link(name = "dbghelp")]
unsafe extern "system" {
    fn SymSetOptions(options: u32) -> u32;
    fn SymInitializeW(process: isize, search_path: *const u16, invade: i32) -> i32;
    fn SymFromAddrW(process: isize, address: u64, displacement: *mut u64, symbol: *mut c_void) -> i32;
    fn SymGetLineFromAddrW64(process: isize, address: u64, displacement: *mut u32, line: *mut c_void) -> i32;
}

/// x64 `CONTEXT` (1232 bytes, 16-aligned): `ContextFlags` at 0x30, `Rip` at 0xF8.
#[repr(C, align(16))]
struct Context([u8; 1232]);

const INTERVAL: Duration = Duration::from_millis(2);

/// The recompiled functions' guest addresses, sorted (set at boot), to name
/// the function a guest return address is in.
static FUNCTIONS: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();

/// Keeps the function table's addresses for `guest_function`.
pub fn set_functions(table: &[(u32, ppc_rt::GuestFn)]) {
    let _ = FUNCTIONS.set(table.iter().map(|e| e.0).collect());
}

/// The recompiled function a guest code address is in.
fn guest_function(addr: u32) -> Option<u32> {
    let f = FUNCTIONS.get()?;
    let i = f.partition_point(|&s| s <= addr);
    (i > 0).then(|| f[i - 1])
}

/// A guest thread's call chain, innermost first, read from its register
/// context and stack without stopping it: the link register (where the
/// innermost call returns), then each frame's saved return address. A frame
/// starts with the caller's stack pointer (`stwu r1`), and a function saves
/// its return address 8 bytes below its caller's stack pointer. The reads race
/// with the thread (a sample can be slightly off); guest memory is always
/// mapped, and every address is checked against the thread's stack and the
/// code range.
fn guest_chain(t: &thread::Thread, out: &mut Vec<u32>) {
    out.clear();
    let ctx = t.ctx.load(Ordering::Relaxed);
    if ctx.is_null() {
        return;
    }
    // SAFETY: the context lives as long as the thread runs (`thread::run`); the
    // fields are plain integers, read racily on purpose.
    let (mut sp, lr) = unsafe { (std::ptr::read_volatile(&(*ctx).r[1]) as u32, std::ptr::read_volatile(&(*ctx).lr) as u32) };
    let code = 0x8200_0000..0x8400_0000u32;
    if code.contains(&lr) {
        out.push(lr);
    }
    let stack = t.stack_low..t.stack_high;
    while out.len() < FRAMES && stack.contains(&sp) && sp % 4 == 0 {
        let back = crate::mem::r32(sp);
        if back <= sp || !stack.contains(&back) {
            break;
        }
        let ret = crate::mem::r32(back - 8);
        if code.contains(&ret) && out.last() != Some(&ret) {
            out.push(ret);
        }
        sp = back;
    }
}

/// `WWE14_HITCH_WATCH=MS`: whenever no frame has been presented for `MS`
/// milliseconds, samples every working guest thread's guest call chain every
/// 2 ms until the next frame (5 s at most), then logs, per thread that worked,
/// the game functions it spent the stall in (inclusive, by samples) and the
/// waits that were ready but not woken (the first 2,000 stalls). The game's frame
/// waits on its other threads; this names the one that held it up and the
/// code it ran.
///
/// It never stops a thread. It used to suspend each one and unwind its host
/// stack: a thread stopped while holding a lock the unwinder needs held the
/// sampler too, and a load-time stall got one sample in 5 to 9 seconds and
/// lasted that long (the user's run of 2026-11-25).
pub fn watch_hitches(threshold: Duration) {
    let _ = std::thread::Builder::new().name("hitch watch".into()).spawn(move || {
        let mut chain = Vec::with_capacity(FRAMES);
        let (mut last_frame, mut since) = (0u64, Instant::now());
        // The frame whose stall was already reported (sampled up to the 5 s cap).
        let mut reported = u64::MAX;
        let mut logged = 0;
        while logged < 2000 {
            std::thread::sleep(Duration::from_millis(5));
            let f = crate::video::PRESENTED.load(Ordering::Relaxed);
            if f != last_frame {
                (last_frame, since) = (f, Instant::now());
                continue;
            }
            if f == 0 || f == reported || since.elapsed() < threshold {
                continue;
            }
            let ready: Vec<String> = crate::object::describe_waiters().into_iter().filter(|w| w.contains("READY")).collect();
            // Per thread: (name, samples working, inclusive function counts, innermost counts).
            let mut busy: HashMap<u32, (String, u32, HashMap<u32, u32>, HashMap<u32, u32>)> = HashMap::new();
            let mut rounds = 0u32;
            let sampling = Instant::now();
            while crate::video::PRESENTED.load(Ordering::Relaxed) == f && since.elapsed() < Duration::from_secs(5) {
                rounds += 1;
                let waiting = crate::object::waiting_thread_ids();
                for t in thread::all().iter().filter(|t| !t.finished.load(Ordering::Relaxed) && !t.sleeping.load(Ordering::Relaxed) && !waiting.contains(&t.id)) {
                    guest_chain(t, &mut chain);
                    let Some(&inner) = chain.first() else { continue };
                    let e = busy.entry(t.id).or_insert_with(|| (t.name(), 0, HashMap::new(), HashMap::new()));
                    e.1 += 1;
                    *e.3.entry(guest_function(inner).unwrap_or(inner)).or_default() += 1;
                    let mut seen: Vec<u32> = Vec::with_capacity(chain.len());
                    for &a in &chain {
                        // One count per function per sample (recursion).
                        let func = guest_function(a).unwrap_or(a);
                        if !seen.contains(&func) {
                            *e.2.entry(func).or_default() += 1;
                            seen.push(func);
                        }
                    }
                }
                std::thread::sleep(INTERVAL);
            }
            reported = f;
            let interval = sampling.elapsed().as_secs_f64() * 1000.0 / rounds.max(1) as f64;
            let ms = since.elapsed().as_millis();
            let mut lines = vec![format!("hitch: no frame for {ms} ms after frame {f} ({rounds} samples, one every {interval:.1} ms); working threads:")];
            let mut threads: Vec<_> = busy.into_iter().collect();
            threads.sort_by_key(|(_, b)| std::cmp::Reverse(b.1));
            for (id, (name, samples, incl, inner)) in threads.into_iter().take(4) {
                lines.push(format!("  thread {id} '{name}' worked {samples} of {rounds}:"));
                let mut v: Vec<(u32, u32)> = incl.into_iter().collect();
                v.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
                for (func, c) in v.into_iter().take(16) {
                    lines.push(format!("    {c:5} sub_{func:08X}"));
                }
                let mut l: Vec<(u32, u32)> = inner.into_iter().collect();
                l.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
                let l: Vec<String> = l.into_iter().take(5).map(|(func, c)| format!("sub_{func:08X} {c}")).collect();
                lines.push(format!("    innermost callers: {}", l.join(", ")));
            }
            lines.extend(ready.into_iter().map(|w| format!("  {w}")));
            diag::log(&lines.join("\n"));
            logged += 1;
        }
    });
}

/// A sample is under way, or waiting for its start time.
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Parses `START:SECONDS` and starts the sampler thread.
pub fn start(spec: &str) -> Result<(), String> {
    let (a, b) = spec.split_once(':').ok_or("expected START:SECONDS")?;
    let start: f64 = a.parse().map_err(|_| format!("bad start {a:?}"))?;
    let secs: f64 = b.parse().map_err(|_| format!("bad duration {b:?}"))?;
    BUSY.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("profiler".into())
        .spawn(move || run(Duration::from_secs_f64(start), Duration::from_secs_f64(secs)))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Samples for `secs` seconds from now, unless a sample is already under way
/// or waiting. `why` goes in the log. Returns whether it started. The window
/// starts one on F10, and in heavy frames with `WWE14_PROFILE_DRAWS`.
pub fn start_now(secs: f64, why: &str) -> bool {
    if BUSY.swap(true, Ordering::AcqRel) {
        return false;
    }
    diag::log(&format!("profile: {why}"));
    let started = std::thread::Builder::new().name("profiler".into()).spawn(move || run(Duration::ZERO, Duration::from_secs_f64(secs))).is_ok();
    if !started {
        BUSY.store(false, Ordering::Release);
    }
    started
}

/// `WWE14_PROFILE_AUTO=SECONDS`: samples of a played session with nothing to
/// press. `auto` (at boot) takes a 12 s sample every `SECONDS` (30 at least),
/// which lands in menus, loads and matches alike; `scene` adds one at each
/// scene the game marks.
static AUTO: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();

fn auto_period() -> Option<u64> {
    *AUTO.get_or_init(|| std::env::var("WWE14_PROFILE_AUTO").ok().and_then(|v| v.parse::<u64>().ok()).map(|s| s.max(30)))
}

/// Starts the periodic samples if `WWE14_PROFILE_AUTO` asks for them.
pub fn auto() {
    let Some(period) = auto_period() else { return };
    diag::log(&format!("profile: WWE14_PROFILE_AUTO: a 12 s sample every {period} s, and 15 s at each scene the game marks (six at most)"));
    let _ = std::thread::Builder::new().name("profile timer".into()).spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(period));
            start_now(12.0, &format!("{period} s have passed: sampling for 12 s"));
        }
    });
}

/// The game marked a scene (it switches to half rate for entrances, cutscenes
/// and loading screens): with `WWE14_PROFILE_AUTO`, a 15 s sample from now,
/// six a run at most.
pub fn scene(what: &str) {
    static TAKEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if auto_period().is_none() || TAKEN.load(Ordering::Relaxed) >= 6 {
        return;
    }
    if start_now(15.0, &format!("{what}: sampling for 15 s")) {
        TAKEN.fetch_add(1, Ordering::Relaxed);
    }
}

#[repr(C)]
struct RuntimeFunction {
    begin: u32,
    end: u32,
    unwind: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn RtlLookupFunctionEntry(pc: u64, image_base: *mut u64, history: *mut c_void) -> *mut RuntimeFunction;
    fn RtlVirtualUnwind(
        handler_type: u32,
        image_base: u64,
        pc: u64,
        function: *mut RuntimeFunction,
        context: *mut c_void,
        handler_data: *mut *mut c_void,
        establisher_frame: *mut u64,
        context_pointers: *mut c_void,
    ) -> *mut c_void;
}

/// `CONTEXT_FULL`: control, integer and floating-point registers (unwinding
/// needs the non-volatile ones). `Rsp` is at 0x98.
const CONTEXT_FULL: u32 = 0x0010_000B;
const FRAMES: usize = 32;

/// Unwinds a stopped thread's stack from `ctx`: up to 16 return addresses,
/// innermost first. Allocates nothing (the thread is suspended).
unsafe fn unwind(ctx: &mut Context, out: &mut [u64; FRAMES]) -> usize {
    let get = |c: &Context, at: usize| u64::from_le_bytes(c.0[at..at + 8].try_into().unwrap());
    let mut n = 0;
    while n < FRAMES {
        let rip = get(ctx, 0xF8);
        if rip == 0 {
            break;
        }
        out[n] = rip;
        n += 1;
        let mut base = 0u64;
        let f = unsafe { RtlLookupFunctionEntry(rip, &mut base, std::ptr::null_mut()) };
        if f.is_null() {
            // A leaf without unwind data: the return address is on top of the stack.
            let rsp = get(ctx, 0x98);
            if rsp == 0 || rsp % 8 != 0 {
                break;
            }
            let ret = unsafe { *(rsp as *const u64) };
            ctx.0[0xF8..0x100].copy_from_slice(&ret.to_le_bytes());
            ctx.0[0x98..0xA0].copy_from_slice(&(rsp + 8).to_le_bytes());
        } else {
            let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
            unsafe { RtlVirtualUnwind(0, base, rip, f, ctx.0.as_mut_ptr().cast(), &mut data, &mut frame, std::ptr::null_mut()) };
        }
    }
    n
}

fn run(start: Duration, length: Duration) {
    std::thread::sleep(start);
    diag::log(&format!("profile: sampling every {} ms for {:.0} s", INTERVAL.as_millis(), length.as_secs_f64()));
    // (thread id, thread name) -> stack (innermost first) -> samples
    let mut samples: HashMap<(u32, String), HashMap<Vec<u64>, u32>> = HashMap::new();
    let mut rounds = 0u32;
    let end = Instant::now() + length;
    let mut ctx = Box::new(Context([0; 1232]));
    let mut frames = [0u64; FRAMES];
    while Instant::now() < end {
        let threads: Vec<Arc<thread::Thread>> = thread::all();
        let hosts = HOST_THREADS.lock().unwrap().clone();
        let mut got: Vec<(u32, Vec<u64>)> = Vec::with_capacity(threads.len() + hosts.len());
        let handles: Vec<(u32, isize)> = threads
            .iter()
            .filter(|t| !t.finished.load(Ordering::Relaxed))
            .map(|t| (t.id, t.host_handle() as isize))
            .chain(hosts.iter().map(|(id, _, h)| (*id, *h)))
            .collect();
        for &(id, h) in &handles {
            if h == 0 {
                continue;
            }
            // Nothing may allocate between suspend and resume: the stopped
            // thread could hold the heap lock.
            let n = unsafe {
                if SuspendThread(h) == u32::MAX {
                    continue;
                }
                ctx.0[0x30..0x34].copy_from_slice(&CONTEXT_FULL.to_le_bytes());
                let n = if GetThreadContext(h, ctx.0.as_mut_ptr().cast()) != 0 { unwind(&mut ctx, &mut frames) } else { 0 };
                ResumeThread(h);
                n
            };
            if n > 0 {
                got.push((id, frames[..n].to_vec()));
            }
        }
        for (id, stack) in got {
            let name = match threads.iter().find(|t| t.id == id) {
                Some(t) => t.name(),
                None => hosts.iter().find(|h| h.0 == id).map(|h| format!("host {}", h.1)).unwrap_or_default(),
            };
            *samples.entry((id, name)).or_default().entry(stack).or_default() += 1;
        }
        rounds += 1;
        std::thread::sleep(INTERVAL);
    }
    report(&samples, rounds);
    BUSY.store(false, Ordering::Release);
}

/// Function name of a host address (`module!function` without the hash
/// suffix), cached.
struct Symbols {
    process: isize,
    cache: HashMap<u64, String>,
    lines: HashMap<u64, Option<String>>,
    buf: Vec<u64>,
}

impl Symbols {
    fn new() -> Symbols {
        let process = unsafe { GetCurrentProcess() };
        // The exe names its PDB without a directory (`wwe14.pdb`), and the game
        // runs from the project root: search the exe's own directory.
        let dir: Vec<u16> = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.as_os_str().to_owned()))
            .map(|d| std::os::windows::ffi::OsStrExt::encode_wide(d.as_os_str()).chain([0]).collect())
            .unwrap_or_else(|| vec![0]);
        unsafe {
            SymSetOptions(0x2 | 0x4 | 0x10); // SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES
            SymInitializeW(process, dir.as_ptr(), 1);
        }
        Symbols { process, cache: HashMap::new(), lines: HashMap::new(), buf: vec![0u64; (88 + 2 * 512) / 8 + 1] }
    }

    /// The source line of a host address (`crate\src\file.rs:123`), for code
    /// built with line tables: the runtime's crates, not the recompiled game.
    /// Code inlined there counts as the line that called it.
    fn line(&mut self, addr: u64) -> Option<String> {
        if let Some(l) = self.lines.get(&addr) {
            return l.clone();
        }
        // IMAGEHLP_LINEW64 (40 bytes): SizeOfStruct, Key at 8, LineNumber at 16,
        // FileName (a wide string's address) at 24, Address at 32.
        let mut info = [0u64; 5];
        info[0] = 40;
        let mut displacement = 0u32;
        let found = unsafe { SymGetLineFromAddrW64(self.process, addr, &mut displacement, info.as_mut_ptr().cast()) != 0 };
        let line = found.then(|| {
            let mut name = Vec::new();
            let mut p = info[3] as *const u16;
            // SAFETY: dbghelp returned a NUL-terminated wide string it owns.
            unsafe {
                while !p.is_null() && *p != 0 && name.len() < 512 {
                    name.push(*p);
                    p = p.add(1);
                }
            }
            let path = String::from_utf16_lossy(&name);
            let parts: Vec<&str> = path.split(['\\', '/']).collect();
            format!("{}:{}", parts[parts.len().saturating_sub(3)..].join("\\"), info[2] as u32)
        });
        self.lines.insert(addr, line.clone());
        line
    }

    fn name(&mut self, addr: u64) -> String {
        if let Some(n) = self.cache.get(&addr) {
            return n.clone();
        }
        // SYMBOL_INFOW: SizeOfStruct 88, MaxNameLen at 80, NameLen at 76, Name at 84.
        let b = self.buf.as_mut_ptr().cast::<u8>();
        let name = unsafe {
            std::ptr::write_bytes(b, 0, 88);
            b.cast::<u32>().write(88);
            b.add(80).cast::<u32>().write(512);
            let mut displacement = 0u64;
            if SymFromAddrW(self.process, addr, &mut displacement, b.cast()) != 0 {
                let len = (b.add(76).cast::<u32>().read() as usize).min(512);
                let w = std::slice::from_raw_parts(b.add(84).cast::<u16>(), len);
                String::from_utf16_lossy(w)
            } else {
                format!("?{addr:#x}")
            }
        };
        // Rust symbols end in a hash (`::h0123…`); drop it.
        let name = match name.rfind("::h") {
            Some(i) if name.len() - i == 19 => name[..i].to_string(),
            _ => name,
        };
        self.cache.insert(addr, name.clone());
        name
    }
}

/// Waits, in the host's own terms: a thread here is blocked, not working.
/// Without ntdll's PDB, dbghelp names a syscall stub by the nearest export: a
/// thread parked in `NtWaitForAlertByThreadId` (the wait under Rust's and the
/// CRT's locks) comes out as the neighbouring `NtAlertThreadByThreadId(Ex)`.
fn is_wait(f: &str) -> bool {
    ["NtWaitFor", "ZwWaitFor", "NtDelayExecution", "ZwDelayExecution", "NtRemoveIoCompletion", "SleepConditionVariable", "WaitOnAddress", "NtWaitForAlertByThreadId", "ZwWaitForAlertByThreadId", "NtAlertThreadByThreadId"]
        .iter()
        .any(|w| f.contains(w))
}

/// Our own code (the recompiled game and the runtime's crates), as opposed to
/// Windows, the drivers and the standard library.
fn is_own(f: &str) -> bool {
    ["sub_", "gpu", "render", "kernel", "ppc_rt", "host", "audio", "xenos"].iter().any(|p| f.contains(p))
}

fn report(samples: &HashMap<(u32, String), HashMap<Vec<u64>, u32>>, rounds: u32) {
    let mut sym = Symbols::new();
    let mut total: HashMap<String, u32> = HashMap::new();
    let mut lines = vec![format!("profile: {rounds} rounds")];
    let mut threads: Vec<_> = samples.iter().collect();
    threads.sort_by_key(|(k, _)| k.0);
    let mut busy_rows = Vec::new();
    for ((id, name), stacks) in threads {
        let mut funcs: HashMap<String, u32> = HashMap::new();
        // Self time charged to the innermost frame of our own code (time in
        // Windows or std on its behalf included; waits marked), and inclusive
        // time per function of ours.
        let mut own_self: HashMap<String, u32> = HashMap::new();
        let mut inclusive: HashMap<String, u32> = HashMap::new();
        // Working samples by the source line of the innermost frame of our
        // own code (where the binary has line tables): what it ran or called.
        let mut own_lines: HashMap<String, u32> = HashMap::new();
        for (stack, &n) in stacks {
            let names: Vec<String> = stack.iter().map(|&a| sym.name(a)).collect();
            *funcs.entry(names[0].clone()).or_default() += n;
            let waiting = is_wait(&names[0]);
            if !waiting && let Some(i) = names.iter().position(|f| is_own(f)) {
                // A caller's address is where its call returns to: one byte
                // back is inside the call.
                let at = if i == 0 { stack[i] } else { stack[i] - 1 };
                if let Some(line) = sym.line(at) {
                    *own_lines.entry(format!("{line}  ({})", names[i])).or_default() += n;
                }
            }
            if let Some(own) = names.iter().find(|f| is_own(f)) {
                let key = if waiting { format!("{own} (waiting)") } else { own.clone() };
                *own_self.entry(key).or_default() += n;
            }
            if !waiting {
                let mut seen: Vec<&String> = Vec::new();
                for f in names.iter().filter(|f| is_own(f)) {
                    if !seen.contains(&f) {
                        seen.push(f);
                        *inclusive.entry(f.clone()).or_default() += n;
                    }
                }
            }
        }
        let all: u32 = funcs.values().sum();
        let waiting: u32 = funcs.iter().filter(|(f, _)| is_wait(f)).map(|(_, n)| n).sum();
        let busy = all - waiting;
        busy_rows.push((busy, *id, name.clone(), all));
        for (f, n) in &funcs {
            if !is_wait(f) {
                *total.entry(f.clone()).or_default() += n;
            }
        }
        if busy * 20 < all.max(1) {
            continue; // under 5% busy: not worth listing
        }
        let mut top: Vec<_> = funcs.into_iter().filter(|(f, _)| !is_wait(f)).collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        lines.push(format!("  thread {id} '{name}': busy {:.0}%", 100.0 * busy as f64 / all.max(1) as f64));
        for (f, n) in top.into_iter().take(12) {
            lines.push(format!("      {:5.1}%  {f}", 100.0 * n as f64 / all.max(1) as f64));
        }
        let pct = |n: u32| 100.0 * n as f64 / all.max(1) as f64;
        let mut own: Vec<_> = own_self.into_iter().collect();
        own.sort_by(|a, b| b.1.cmp(&a.1));
        lines.push("    by our innermost function (all samples):".into());
        for (f, n) in own.into_iter().take(15) {
            lines.push(format!("      {:5.1}%  {f}", pct(n)));
        }
        let mut inc: Vec<_> = inclusive.into_iter().collect();
        inc.sort_by(|a, b| b.1.cmp(&a.1));
        lines.push("    inclusive, working samples:".into());
        for (f, n) in inc.into_iter().take(20) {
            lines.push(format!("      {:5.1}%  {f}", pct(n)));
        }
        if !own_lines.is_empty() {
            let mut by_line: Vec<_> = own_lines.into_iter().collect();
            by_line.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            lines.push("    by our innermost source line, working samples:".into());
            for (l, n) in by_line.into_iter().take(40) {
                lines.push(format!("      {:5.1}%  {l}", pct(n)));
            }
        }
    }
    busy_rows.sort_by(|a, b| b.0.cmp(&a.0));
    lines.push("  busiest threads (share of the run they were working):".into());
    for (busy, id, name, all) in busy_rows.iter().take(12) {
        lines.push(format!("      {:5.1}%  thread {id} '{name}'", 100.0 * *busy as f64 / (*all).max(1) as f64));
    }
    let all_busy: u32 = total.values().sum();
    let mut top: Vec<_> = total.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    lines.push(format!("  hottest functions over all threads ({all_busy} working samples):"));
    for (f, n) in top.into_iter().take(40) {
        lines.push(format!("      {:5.1}%  {f}", 100.0 * n as f64 / all_busy.max(1) as f64));
    }
    diag::log(&lines.join("\n"));
}
