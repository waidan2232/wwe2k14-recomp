//! Diagnostics: every failure stops the game with the guest context that
//! explains it. A fatal panic (an unimplemented import, a missing function, a
//! guest trap) and a host access violation inside guest memory both print the
//! guest thread, registers and the guest call stack, then exit.

use std::ffi::c_void;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::mem;
use crate::thread;

/// What to trace (from `WWE14_TRACE`, comma separated: `imports`, `threads`, `files`).
pub static TRACE_IMPORTS: AtomicBool = AtomicBool::new(false);
pub static TRACE_THREADS: AtomicBool = AtomicBool::new(false);
pub static TRACE_FILES: AtomicBool = AtomicBool::new(false);
pub static TRACE_AUDIO: AtomicBool = AtomicBool::new(false);
pub static TRACE_DRAWS: AtomicBool = AtomicBool::new(false);
/// Physical memory allocations and frees (with failures, which this game makes on purpose to probe).
pub static TRACE_MEMORY: AtomicBool = AtomicBool::new(false);

static DYING: AtomicU32 = AtomicU32::new(0);

pub fn configure_trace(spec: &str) {
    for part in spec.split(',') {
        match part.trim() {
            "imports" => TRACE_IMPORTS.store(true, Ordering::Relaxed),
            "threads" => TRACE_THREADS.store(true, Ordering::Relaxed),
            "files" => TRACE_FILES.store(true, Ordering::Relaxed),
            "audio" => TRACE_AUDIO.store(true, Ordering::Relaxed),
            "draws" => TRACE_DRAWS.store(true, Ordering::Relaxed),
            "memory" => TRACE_MEMORY.store(true, Ordering::Relaxed),
            "all" => {
                TRACE_IMPORTS.store(true, Ordering::Relaxed);
                TRACE_THREADS.store(true, Ordering::Relaxed);
                TRACE_FILES.store(true, Ordering::Relaxed);
                TRACE_AUDIO.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub fn log(msg: &str) {
    let tid = thread::try_current().map_or(0, |t| t.id);
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "[t{tid:02}] {msg}");
}

pub fn trace_thread(msg: &str) {
    if TRACE_THREADS.load(Ordering::Relaxed) {
        log(msg);
    }
}

pub fn trace_file(msg: &str) {
    if TRACE_FILES.load(Ordering::Relaxed) {
        log(msg);
    }
}

/// Guest context of the current thread, for fatal reports.
/// Every guest thread's state: registers and call stack, read while the
/// threads run (diagnostics; the values may be a moment stale).
pub fn dump_threads() {
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "
==== guest threads");
    for t in thread::all() {
        if t.finished.load(Ordering::Relaxed) {
            continue;
        }
        let c = t.ctx.load(Ordering::Relaxed);
        if c.is_null() {
            let _ = writeln!(e, "thread {} '{}': not started", t.id, t.name());
            continue;
        }
        let c = unsafe { &*c };
        let _ = writeln!(e, "thread {} '{}': lr 0x{:08X} r1 0x{:08X} r3 0x{:08X}", t.id, t.name(), c.lr as u32, c.r[1] as u32, c.r[3] as u32);
        let stack = walk(&t, c.r[1] as u32);
        let s: Vec<String> = stack.iter().map(|a| format!("{a:08X}")).collect();
        let _ = writeln!(e, "    {}", s.join(" "));
    }
}

fn walk(t: &thread::Thread, r1: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut sp = r1;
    for _ in 0..24 {
        if sp < t.stack_low || sp >= t.stack_high || sp & 3 != 0 {
            break;
        }
        let back = mem::r32(sp);
        if back <= sp || back >= t.stack_high {
            break;
        }
        let ret = mem::r32(back - 8);
        if ret == 0 {
            break;
        }
        out.push(ret);
        sp = back;
    }
    out
}

/// The current guest thread's saved return addresses, innermost first, from
/// its stack pointer `r1` (for trace lines).
pub fn guest_stack(r1: u32) -> String {
    let Some(t) = thread::try_current() else { return String::new() };
    walk(&t, r1).iter().map(|a| format!("{a:08X}")).collect::<Vec<_>>().join(" ")
}

/// Set at boot: the recompiled code keeps registers in host locals
/// (`Program::registers_in_locals`).
pub static REGISTERS_IN_LOCALS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn guest_context() -> String {
    let Some(t) = thread::try_current() else { return "  (not on a guest thread)\n".into() };
    let mut s = format!("  guest thread {} '{}' (KTHREAD 0x{:08X})\n", t.id, t.name(), t.kthread);
    let c = thread::current_ctx();
    if c.is_null() {
        return s;
    }
    let c = unsafe { &*c };
    s += &format!("  lr 0x{:08X}  r1 0x{:08X}  ctr 0x{:08X}\n", c.lr as u32, c.r[1] as u32, c.ctr as u32);
    for row in 0..4 {
        s += "  ";
        for k in 0..8 {
            let r = row * 8 + k;
            s += &format!("r{r:<2} {:08X}  ", c.r[r] as u32);
        }
        s += "\n";
    }
    if REGISTERS_IN_LOCALS.load(std::sync::atomic::Ordering::Relaxed) {
        s += "  (this build keeps r0, r11, r12 and r14-r31 in host locals while a function runs: their values here \
              are from the last time a function handed them on, not current. r1-r10 and r13 are current. \
              A build made with `recomp --locals off` reports all of them.)\n";
    }
    s += "  guest call stack (saved return addresses):\n";
    s += &format!("    0x{:08X} (lr)\n", c.lr as u32);
    let mut sp = c.r[1] as u32;
    for _ in 0..48 {
        if sp < t.stack_low || sp >= t.stack_high || sp & 3 != 0 {
            break;
        }
        let back = mem::r32(sp);
        if back <= sp || back >= t.stack_high {
            break;
        }
        let ret = mem::r32(back - 8);
        if ret == 0 {
            break;
        }
        s += &format!("    0x{ret:08X}\n", );
        sp = back;
    }
    // The current stack frame and the memory the registers point to (32 bytes each), for
    // finding which data was wrong.
    let dump = |addr: u32, len: u32| -> Option<String> {
        if !mem::host_readable(addr, len) {
            return None;
        }
        let bytes = mem::read(addr, len as usize);
        Some(bytes.chunks(4).map(|w| w.iter().map(|b| format!("{b:02X}")).collect::<String>()).collect::<Vec<_>>().join(" "))
    };
    s += "  memory:\n";
    let r1 = c.r[1] as u32;
    for off in (0..0x100).step_by(32) {
        if let Some(d) = dump(r1 + off, 32) {
            s += &format!("    r1+0x{off:02X} {:08X}: {d}\n", r1 + off);
        }
    }
    for r in (0..32).filter(|&r| r != 1) {
        let a = c.r[r] as u32;
        if let Some(d) = dump(a, 32) {
            s += &format!("    r{r:<2} {a:08X}: {d}\n");
        }
    }
    s
}

/// Stops the game with a diagnostic. Never returns.
pub fn fatal(msg: &str) -> ! {
    panic!("FATAL: {msg}");
}

/// Reports a fatal error with the guest context and ends the process, without
/// unwinding (for places outside every `catch_unwind`, like a thread's top level).
pub fn fatal_exit(msg: &str) -> ! {
    if DYING.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut e = std::io::stderr().lock();
        let _ = writeln!(e, "\n==== FATAL: {msg}");
        let _ = write!(e, "{}", guest_context());
    }
    exit_after_fatal()
}

/// Where the log goes when nobody sees stderr (the portable release, started
/// from Explorer): a fatal stop then also says so in a message box, since the
/// game has no console and its window just closes. Set by the host at start.
pub static FATAL_LOG: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

/// Called after a fatal panic has been reported: ends the process.
pub fn exit_after_fatal() -> ! {
    let _ = std::io::stderr().flush();
    if let Some(log) = FATAL_LOG.get() {
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let text = wide(&format!("The game stopped because of an error.\n\nWhat happened is at the end of:\n{log}"));
        // SAFETY: both strings are NUL-terminated and outlive the call. 0x10 | 0x10000: an error icon, in front.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), wide("WWE 2K14").as_ptr(), 0x10 | 0x1_0000) };
    }
    std::process::exit(3);
}

/// Installs the panic hook (adds guest context to every fatal panic) and the
/// access-violation handler for guest memory.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        if DYING.fetch_add(1, Ordering::SeqCst) > 0 {
            // Another thread is already reporting; stay quiet.
            std::thread::sleep(std::time::Duration::from_secs(5));
            return;
        }
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("(non-string panic)");
        let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        let mut e = std::io::stderr().lock();
        let _ = writeln!(e, "\n==== {msg}\n  (raised in the native runtime{at})");
        let _ = write!(e, "{}", guest_context());
        let _ = writeln!(e, "  host stack (recompiled functions are sub_XXXXXXXX):");
        let _ = writeln!(e, "{}", host_backtrace());
        drop(e);
        if thread::try_current().is_none() {
            // A kernel service thread: nothing above it will end the process.
            exit_after_fatal();
        }
    }));
    unsafe { AddVectoredExceptionHandler(1, Some(veh)) };
}

fn host_backtrace() -> String {
    let bt = std::backtrace::Backtrace::force_capture().to_string();
    // Keep the interesting frames: recompiled functions and our kernel.
    bt.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("at ") && (t.contains("sub_") || t.contains("kernel::") || t.contains("gpu::") || t.contains("ppc_rt::"))
        })
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
}

#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    count: u32,
    info: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}

type Handler = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(first: u32, handler: Option<Handler>) -> *mut c_void;
    fn SetThreadDescription(thread: *mut c_void, name: *const u16) -> i32;
    fn GetCurrentThread() -> *mut c_void;
}

const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

unsafe extern "system" fn veh(p: *mut ExceptionPointers) -> i32 {
    let rec = unsafe { &*(*p).record };
    if rec.code != EXCEPTION_ACCESS_VIOLATION || rec.count < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let Some(addr) = mem::guest_address_of(rec.info[1]) else { return EXCEPTION_CONTINUE_SEARCH };
    if (mem::MMIO.0..mem::MMIO.1).contains(&addr)
        && crate::video::PARK_AT_BOUNDARY.load(Ordering::Relaxed)
        && let Some(t) = thread::try_current()
    {
        // Diagnostics (`--gpu-boundary park`): this thread reached hardware and
        // never continues; the others keep running. The handler doesn't return.
        log(&format!(
            "hardware access at 0x{addr:08X} ({}): thread {} '{}' is parked for good",
            mem::describe(addr),
            t.id,
            t.name()
        ));
        loop {
            std::thread::park();
        }
    }
    // A watched range (`mem::watch`): report who touched it first, and go on.
    if let Some((start, len, what, reader)) = mem::end_watch(addr) {
        if let Some(reader) = reader {
            // Memory whose contents are held elsewhere until the game looks:
            // a read gets them first; a write replaces them anyway.
            if rec.info[0] == 0 {
                reader(start, len);
            }
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        let kind = if rec.info[0] == 1 { "write" } else { "read" };
        let mut e = std::io::stderr().lock();
        let _ = writeln!(
            e,
            "\n==== watch: first {kind} at 0x{addr:08X}, in {what} (0x{start:08X}+0x{len:X}), frame {}\n  (host instruction at {:p})",
            crate::video::PRESENTED.load(Ordering::Relaxed),
            rec.address
        );
        let _ = write!(e, "{}", guest_context());
        let _ = writeln!(e, "  host stack (recompiled functions are sub_XXXXXXXX):");
        let _ = writeln!(e, "{}", host_backtrace());
        let _ = e.flush();
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if DYING.fetch_add(1, Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_secs(5));
        std::process::exit(3);
    }
    let kind = match rec.info[0] {
        0 => "read",
        1 => "write",
        _ => "execute",
    };
    let mut e = std::io::stderr().lock();
    let _ = writeln!(
        e,
        "\n==== FATAL: guest memory {kind} at 0x{addr:08X}: {}\n  (host instruction at {:p})",
        mem::describe(addr),
        rec.address
    );
    let _ = write!(e, "{}", guest_context());
    let _ = writeln!(e, "  host stack (recompiled functions are sub_XXXXXXXX):");
    let _ = writeln!(e, "{}", host_backtrace());
    let _ = e.flush();
    std::process::exit(3);
}

pub fn name_host_thread(name: &str) {
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { SetThreadDescription(GetCurrentThread(), w.as_ptr()) };
}
