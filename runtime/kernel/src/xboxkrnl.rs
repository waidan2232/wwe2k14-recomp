//! `xboxkrnl.exe` exports: native implementations of the kernel API contract.
//!
//! Exports not defined here have no implementation: calling one stops the game
//! and names it (see `ppc_rt::call_import`). Hardware-facing exports (the GPU
//! ring buffer, XMA contexts, I/O space mapping) are deliberately absent: the
//! code that would call them is replaced at a higher level.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ppc_rt::Ctx;

use crate::abi::{arg, exports, out32, ret, ret64};
use crate::file;
use crate::loader::{self, module};
use crate::mem;
use crate::object::{self, Object, Timer};
use crate::printf::{self, Args};
use crate::rtl;
use crate::status::*;
use crate::thread::{self, Apc, Entry};
use crate::{audio, diag, time, video};

static PROCESS_TYPE: AtomicU32 = AtomicU32::new(1);

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Guest object path from an `OBJECT_ATTRIBUTES { HANDLE RootDirectory;
/// ANSI_STRING *ObjectName; u32 Attributes; }`.
pub fn object_path(oa: u32) -> Option<String> {
    if oa == 0 {
        return None;
    }
    let name = mem::ansi_string(mem::r32(oa + 4))?;
    let root = mem::r32(oa);
    if root != 0
        && let Some(Object::File(f)) = object::get(root)
    {
        return Some(format!("{}\\{}", f.guest_path.trim_end_matches('\\'), name.trim_start_matches('\\')));
    }
    Some(name)
}

/// Name of a named kernel object, if the attributes give one.
fn object_name(oa: u32) -> Option<String> {
    object_path(oa).filter(|n| !n.is_empty())
}

fn io_status(p: u32, status: u32, info: u32) {
    if p != 0 {
        mem::w32(p, status);
        mem::w32(p + 4, info);
    }
}

/// Signals an I/O completion: the caller's event (or the file's own), and the
/// APC if one was given.
fn complete_io(f: &file::File, event: u32, apc_routine: u32, apc_context: u32, iosb: u32) {
    if event != 0 {
        if let Some(o) = object::get(event).and_then(|o| o.waitable()) {
            object::set_event(o);
        }
    } else {
        object::set_event(f.event);
    }
    // XAPI sets bit 0 of the routine address as a flag (`ReadFileEx` does
    // `ori r5, r5, 1`); the routine itself is the address with it cleared.
    let routine = apc_routine & !1;
    if routine != 0 {
        thread::current().queue_apc(Apc { routine, context: apc_context, arg1: iosb, arg2: 0 });
    }
}

fn file_of(h: u32) -> Option<Arc<file::File>> {
    match object::get(h)? {
        Object::File(f) => Some(f),
        _ => None,
    }
}

fn waitable(h: u32) -> Option<u32> {
    object::get(h)?.waitable()
}

/// Creates or opens a named object: an existing one with the same name is
/// returned with `STATUS_OBJECT_NAME_EXISTS`.
fn create_named(handle_ptr: u32, oa: u32, make: impl FnOnce() -> Object) -> u32 {
    if let Some(name) = object_name(oa) {
        if let Some(o) = object::lookup_named(&name) {
            out32(handle_ptr, object::insert(o));
            return STATUS_OBJECT_NAME_EXISTS;
        }
        let o = make();
        object::register_name(&name, o.clone());
        out32(handle_ptr, object::insert(o));
        return STATUS_SUCCESS;
    }
    out32(handle_ptr, object::insert(make()));
    STATUS_SUCCESS
}

fn thread_of_kthread(k: u32) -> Arc<thread::Thread> {
    thread::by_kthread(k).unwrap_or_else(|| diag::fatal(&format!("0x{k:08X} is not a KTHREAD this kernel created")))
}

// ---- DPCs ------------------------------------------------------------------------
//
// KDPC (as `KeInitializeDpc` lays it out): u16 Type (19), u8, u8, LIST_ENTRY,
// +0x0C DeferredRoutine, +0x10 DeferredContext, +0x14 Arg1, +0x18 Arg2.
// Queued DPCs run on one kernel worker thread whose PCR says "DPC active".

struct DpcQueue {
    queue: std::collections::VecDeque<u32>,
    started: bool,
}

static DPCS: std::sync::Mutex<DpcQueue> =
    std::sync::Mutex::new(DpcQueue { queue: std::collections::VecDeque::new(), started: false });
static DPC_WAKE: std::sync::Condvar = std::sync::Condvar::new();

fn insert_dpc(dpc: u32, a1: u32, a2: u32) -> bool {
    let mut q = DPCS.lock().unwrap();
    if q.queue.contains(&dpc) {
        return false;
    }
    mem::w32(dpc + 0x14, a1);
    mem::w32(dpc + 0x18, a2);
    q.queue.push_back(dpc);
    if !q.started {
        q.started = true;
        thread::spawn(
            Entry::Native(Box::new(|c: &mut Ctx| {
                let t = thread::current();
                mem::w32(t.pcr + 0x150, 1);
                loop {
                    let dpc = {
                        let mut q = DPCS.lock().unwrap();
                        loop {
                            if let Some(d) = q.queue.pop_front() {
                                break d;
                            }
                            q = DPC_WAKE.wait(q).unwrap();
                        }
                    };
                    let (routine, context) = (mem::r32(dpc + 0xC), mem::r32(dpc + 0x10));
                    let (a1, a2) = (mem::r32(dpc + 0x14), mem::r32(dpc + 0x18));
                    thread::call(c, routine, &[dpc as u64, context as u64, a1 as u64, a2 as u64]);
                }
            })),
            0x1_0000,
            1,
            false,
            "kernel DPC".into(),
        );
    }
    DPC_WAKE.notify_all();
    true
}

// ---- XConfig ---------------------------------------------------------------------

/// `ExGetXConfigSetting` values: `(category, setting)` → bytes.
fn xconfig(category: u16, setting: u16) -> Option<Vec<u8>> {
    let u32b = |v: u32| v.to_be_bytes().to_vec();
    Some(match (category, setting) {
        // XCONFIG_SECURED_CATEGORY
        (0x0002, 0x0002) => u32b(0x0000_1000), // AV region: NTSC-M (North America)
        (0x0002, 0x0003) => u32b(0x0000_00FF), // game region: North America
        // XCONFIG_USER_CATEGORY
        (0x0003, 0x0001) => u32b(0),             // time zone bias
        (0x0003, 0x0002 | 0x0003) => vec![0; 4], // time zone names
        (0x0003, 0x0004 | 0x0005) => vec![0; 4], // standard / daylight dates
        (0x0003, 0x0006 | 0x0007) => u32b(0),    // standard / daylight bias
        (0x0003, 0x0009) => u32b(1),             // language: English
        (0x0003, 0x000A) => u32b(0x0004_0000),   // video flags: widescreen
        (0x0003, 0x000B) => u32b(0),             // audio flags: stereo
        (0x0003, 0x000C) => u32b(0x40),          // retail flags: initial setup done
        (0x0003, 0x000E) => vec![103],           // country: United States
        (0x0003, 0x000F) => vec![0],             // parental control: off
        _ => return None,
    })
}

// ---- Exports ------------------------------------------------------------------------

exports! {
    // ---- Debugging and fatal errors

    DbgPrint => |c| {
        let fmt = mem::cstr(arg(c, 0), 4096);
        let s = printf::format(&fmt, &mut Args::Regs { c: &*c, next: 1 });
        diag::log(&format!("DbgPrint: {}", lossy(&s).trim_end()));
        ret(c, STATUS_SUCCESS);
    }
    DbgBreakPoint => |c| { diag::fatal("the game called DbgBreakPoint") }
    KeBugCheck => |c| { diag::fatal(&format!("KeBugCheck(0x{:08X})", arg(c, 0))) }
    KeBugCheckEx => |c| {
        diag::fatal(&format!(
            "KeBugCheckEx(0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:08X}, 0x{:08X})",
            arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4)
        ))
    }
    HalReturnToFirmware => |c| {
        diag::log(&format!("HalReturnToFirmware({}): the game asked to leave", arg(c, 0)));
        std::process::exit(0);
    }

    // ---- Formatting

    sprintf => |c| {
        let (buf, fmt) = (arg(c, 0), mem::cstr(arg(c, 1), 1 << 16));
        let s = printf::format(&fmt, &mut Args::Regs { c: &*c, next: 2 });
        mem::write(buf, &s);
        mem::w8(buf + s.len() as u32, 0);
        ret(c, s.len() as u32);
    }
    _snprintf => |c| {
        let (buf, count, fmt) = (arg(c, 0), arg(c, 1), mem::cstr(arg(c, 2), 1 << 16));
        let s = printf::format(&fmt, &mut Args::Regs { c: &*c, next: 3 });
        let r = snprintf_out(buf, count, &s);
        ret(c, r);
    }
    _vsnprintf => |c| {
        let (buf, count, fmt) = (arg(c, 0), arg(c, 1), mem::cstr(arg(c, 2), 1 << 16));
        let s = printf::format(&fmt, &mut Args::VaList(arg(c, 3)));
        let r = snprintf_out(buf, count, &s);
        ret(c, r);
    }

    // ---- Threads

    ExCreateThread => |c| {
        let (handle_ptr, stack, id_ptr, xapi, start, context, flags) =
            (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6));
        let t = thread::spawn(
            Entry::Guest { start, context, xapi_startup: xapi },
            stack,
            flags >> 24,
            flags & 1 != 0,
            format!("thread@{start:08X}"),
        );
        diag::trace_thread(&format!(
            "ExCreateThread start=0x{start:08X} ctx=0x{context:08X} stack=0x{stack:X} flags=0x{flags:08X} -> thread {}",
            t.id
        ));
        out32(id_ptr, t.id);
        out32(handle_ptr, object::insert(Object::Thread(t)));
        ret(c, STATUS_SUCCESS);
    }
    ExTerminateThread => |c| { thread::exit_current(arg(c, 0)) }
    NtResumeThread => |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        match object::get(h) {
            Some(Object::Thread(t)) => {
                out32(prev, t.resume());
                ret(c, STATUS_SUCCESS);
            }
            _ => ret(c, STATUS_INVALID_HANDLE),
        }
    }
    KeSetAffinityThread => |c| {
        let (k, affinity, prev) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let t = thread_of_kthread(k);
        out32(prev, t.affinity.load(Ordering::Relaxed));
        t.set_affinity(affinity);
        ret(c, STATUS_SUCCESS);
    }
    KeQueryBasePriorityThread => |c| {
        let t = thread_of_kthread(arg(c, 0));
        ret(c, t.priority.load(Ordering::Relaxed) as u32);
    }
    KeSetBasePriorityThread => |c| {
        let t = thread_of_kthread(arg(c, 0));
        let old = t.set_priority(arg(c, 1) as i32);
        ret(c, old as u32);
    }
    KeDelayExecutionThread => |c| {
        let (alertable, interval) = (arg(c, 1) != 0, arg(c, 2));
        let deadline = object::deadline(interval).unwrap_or_else(std::time::Instant::now);
        let r = object::delay(c, alertable, deadline);
        ret(c, r);
    }
    NtYieldExecution => |c| {
        std::thread::yield_now();
        ret(c, STATUS_SUCCESS);
    }
    KeEnterCriticalRegion => |c| { thread::current().critical_region.fetch_add(1, Ordering::Relaxed); }
    KeLeaveCriticalRegion => |c| { thread::current().critical_region.fetch_sub(1, Ordering::Relaxed); }
    KeGetCurrentProcessType => |c| { ret(c, PROCESS_TYPE.load(Ordering::Relaxed)); }
    KeSetCurrentProcessType => |c| { PROCESS_TYPE.store(arg(c, 0), Ordering::Relaxed); }
    NtQueueApcThread => |c| {
        let (h, routine, context, a1, a2) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        match object::get(h) {
            Some(Object::Thread(t)) => {
                t.queue_apc(Apc { routine, context, arg1: a1, arg2: a2 });
                ret(c, STATUS_SUCCESS);
            }
            _ => ret(c, STATUS_INVALID_HANDLE),
        }
    }
    KiApcNormalRoutineNop => |c| {}

    // ---- TLS

    KeTlsAlloc => |c| { ret(c, thread::tls_alloc()); }
    KeTlsFree => |c| { ret(c, thread::tls_free(arg(c, 0)) as u32); }
    KeTlsGetValue => |c| { ret(c, thread::tls_get(arg(c, 0))); }
    KeTlsSetValue => |c| { ret(c, thread::tls_set(arg(c, 0), arg(c, 1)) as u32); }

    // ---- Dispatcher objects and waits

    KeSetEvent => |c| { ret(c, object::set_event(arg(c, 0)) as u32); }
    KeResetEvent => |c| { ret(c, object::reset_event(arg(c, 0)) as u32); }
    KeWaitForSingleObject => |c| {
        let (o, alertable, timeout) = (arg(c, 0), arg(c, 3) != 0, arg(c, 4));
        let r = object::wait(c, &[o], true, alertable, object::deadline(timeout));
        ret(c, r);
    }
    KeWaitForMultipleObjects => |c| {
        let (count, objs, wait_type, alertable, timeout) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 5) != 0, arg(c, 6));
        let list: Vec<u32> = (0..count).map(|i| mem::r32(objs + 4 * i)).collect();
        let r = object::wait(c, &list, wait_type == 1, alertable, object::deadline(timeout));
        ret(c, r);
    }
    NtWaitForSingleObjectEx => |c| {
        let (h, alertable, timeout) = (arg(c, 0), arg(c, 2) != 0, arg(c, 3));
        let Some(o) = waitable(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let r = object::wait(c, &[o], true, alertable, object::deadline(timeout));
        ret(c, r);
    }
    NtWaitForMultipleObjectsEx => |c| {
        let (count, handles, wait_type, alertable, timeout) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 4) != 0, arg(c, 5));
        let mut list = Vec::new();
        for i in 0..count {
            let h = mem::r32(handles + 4 * i);
            match waitable(h) {
                Some(o) => list.push(o),
                None => {
                    static SEEN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
                    let mut seen = SEEN.lock().unwrap();
                    if !seen.contains(&h) {
                        seen.push(h);
                        let all: Vec<String> = (0..count).map(|k| format!("{:08X}", mem::r32(handles + 4 * k))).collect();
                        diag::log(&format!(
                            "NtWaitForMultipleObjectsEx: handle {h:08X} (#{i} of [{}]) is not a waitable object (from 0x{:08X})",
                            all.join(" "),
                            c.lr as u32
                        ));
                    }
                    return ret(c, STATUS_INVALID_HANDLE);
                }
            }
        }
        let r = object::wait(c, &list, wait_type == 1, alertable, object::deadline(timeout));
        ret(c, r);
    }
    NtCreateEvent => |c| {
        let (hp, oa, kind, initial) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
        let r = create_named(hp, oa, || Object::Dispatcher(object::new_event(kind == 0, initial != 0)));
        ret(c, r);
    }
    NtSetEvent => |c| {
        let Some(o) = waitable(arg(c, 0)) else { return ret(c, STATUS_INVALID_HANDLE) };
        out32(arg(c, 1), object::set_event(o) as u32);
        ret(c, STATUS_SUCCESS);
    }
    NtClearEvent => |c| {
        let Some(o) = waitable(arg(c, 0)) else { return ret(c, STATUS_INVALID_HANDLE) };
        object::reset_event(o);
        ret(c, STATUS_SUCCESS);
    }
    NtCreateSemaphore => |c| {
        let (hp, oa, initial, limit) = (arg(c, 0), arg(c, 1), arg(c, 2) as i32, arg(c, 3) as i32);
        let r = create_named(hp, oa, || Object::Dispatcher(object::new_semaphore(initial, limit)));
        ret(c, r);
    }
    NtReleaseSemaphore => |c| {
        let Some(o) = waitable(arg(c, 0)) else { return ret(c, STATUS_INVALID_HANDLE) };
        match object::release_semaphore(o, arg(c, 1) as i32) {
            Ok(prev) => {
                out32(arg(c, 2), prev as u32);
                ret(c, STATUS_SUCCESS);
            }
            Err(s) => ret(c, s),
        }
    }
    NtCreateMutant => |c| {
        let (hp, oa, owned) = (arg(c, 0), arg(c, 1), arg(c, 2) != 0);
        let r = create_named(hp, oa, || Object::Dispatcher(object::new_mutant(owned)));
        ret(c, r);
    }
    NtReleaseMutant => |c| {
        let Some(o) = waitable(arg(c, 0)) else { return ret(c, STATUS_INVALID_HANDLE) };
        match object::release_mutant(o, false) {
            Ok(prev) => {
                out32(arg(c, 1), prev as u32);
                ret(c, STATUS_SUCCESS);
            }
            Err(s) => ret(c, s),
        }
    }
    NtCreateTimer => |c| {
        let (hp, oa, kind) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let r = create_named(hp, oa, || Object::Timer(Timer::new(kind == 1)));
        ret(c, r);
    }
    NtSetTimerEx => |c| {
        let (h, due_ptr, routine, routine_arg, period, prev_ptr) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 4), arg(c, 6), arg(c, 7));
        let Some(Object::Timer(t)) = object::get(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let due = mem::r64(due_ptr) as i64;
        let apc = (routine != 0).then(|| {
            let now = time::system_time();
            Apc { routine, context: routine_arg, arg1: now as u32, arg2: (now >> 32) as u32 }
        });
        let prev = t.set(due, period, apc);
        out32(prev_ptr, prev as u32);
        ret(c, STATUS_SUCCESS);
    }
    NtCancelTimer => |c| {
        let Some(Object::Timer(t)) = object::get(arg(c, 0)) else { return ret(c, STATUS_INVALID_HANDLE) };
        out32(arg(c, 1), t.cancel() as u32);
        ret(c, STATUS_SUCCESS);
    }

    // ---- Spinlocks and IRQL

    KfAcquireSpinLock => |c| {
        rtl::acquire_spinlock(arg(c, 0));
        let old = thread::current().irql.swap(2, Ordering::Relaxed);
        ret(c, old);
    }
    KfReleaseSpinLock => |c| {
        rtl::release_spinlock(arg(c, 0));
        thread::current().irql.store(arg(c, 1) & 0xFF, Ordering::Relaxed);
    }
    KeAcquireSpinLockAtRaisedIrql => |c| { rtl::acquire_spinlock(arg(c, 0)); }
    KeReleaseSpinLockFromRaisedIrql => |c| { rtl::release_spinlock(arg(c, 0)); }

    // ---- DPCs

    KeInitializeDpc => |c| {
        let (dpc, routine, context) = (arg(c, 0), arg(c, 1), arg(c, 2));
        mem::w16(dpc, 19);
        mem::w8(dpc + 2, 0);
        mem::w8(dpc + 3, 0);
        object::init_list_head(dpc + 4);
        mem::w32(dpc + 0xC, routine);
        mem::w32(dpc + 0x10, context);
        mem::w32(dpc + 0x14, 0);
        mem::w32(dpc + 0x18, 0);
    }
    KeInsertQueueDpc => |c| { ret(c, insert_dpc(arg(c, 0), arg(c, 1), arg(c, 2)) as u32); }

    // ---- Critical sections

    RtlInitializeCriticalSection => |c| { rtl::init_critical_section(arg(c, 0), 0); }
    RtlInitializeCriticalSectionAndSpinCount => |c| {
        rtl::init_critical_section(arg(c, 0), arg(c, 1));
        ret(c, STATUS_SUCCESS);
    }
    RtlEnterCriticalSection => |c| { let cs = arg(c, 0); rtl::enter_critical_section(c, cs); }
    RtlLeaveCriticalSection => |c| { rtl::leave_critical_section(arg(c, 0)); }
    RtlTryEnterCriticalSection => |c| { ret(c, rtl::try_enter_critical_section(arg(c, 0)) as u32); }

    // ---- Memory

    NtAllocateVirtualMemory => |c| {
        let (bp, sp, kind, protect) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
        match mem::allocate_virtual(mem::r32(bp), mem::r32(sp), kind, protect) {
            Ok((base, size)) => {
                mem::w32(bp, base);
                mem::w32(sp, size);
                ret(c, STATUS_SUCCESS);
            }
            Err(s) => {
                // A title treats this as fatal or degrades, so it is always worth knowing.
                diag::log(&format!(
                    "NtAllocateVirtualMemory(base 0x{:08X}, size 0x{:X}, type 0x{kind:X}, protect 0x{protect:X}) failed: 0x{s:08X} [from 0x{:08X}]",
                    mem::r32(bp),
                    mem::r32(sp),
                    c.lr as u32
                ));
                ret(c, s)
            }
        }
    }
    NtFreeVirtualMemory => |c| {
        let (bp, sp, kind) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let size = if sp != 0 { mem::r32(sp) } else { 0 };
        match mem::free_virtual(mem::r32(bp), size, kind) {
            Ok((base, n)) => {
                mem::w32(bp, base);
                out32(sp, n);
                ret(c, STATUS_SUCCESS);
            }
            Err(s) => ret(c, s),
        }
    }
    NtQueryVirtualMemory => |c| {
        let (addr, out) = (arg(c, 0), arg(c, 1));
        let Some(r) = mem::query(addr) else { return ret(c, STATUS_INVALID_PARAMETER) };
        for (i, v) in [r.base, r.allocation_base, r.allocation_protect, r.size, r.state, r.protect, r.kind].iter().enumerate() {
            mem::w32(out + 4 * i as u32, *v);
        }
        ret(c, STATUS_SUCCESS);
    }
    MmQueryAddressProtect => |c| { ret(c, mem::protect_of(arg(c, 0))); }
    MmQueryAllocationSize => |c| { ret(c, mem::allocation_size(arg(c, 0))); }
    MmAllocatePhysicalMemoryEx => |c| {
        let (size, protect, min, max, align) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5));
        let a = mem::allocate_physical(size, protect, min, max, align);
        // The game probes for space with requests it expects to fail, so
        // failures are only reported when tracing memory.
        if diag::TRACE_MEMORY.load(std::sync::atomic::Ordering::Relaxed) {
            let phys = mem::physical_address(a).unwrap_or(0);
            diag::log(&format!(
                "MmAllocatePhysicalMemoryEx(size 0x{size:X}, protect 0x{protect:X}, 0x{min:X}..0x{max:X}, align 0x{align:X}) -> \
                 {} ({} MB in use) [from 0x{:08X} {}]",
                if a == 0 { "failed".to_string() } else { format!("0x{a:08X} (physical 0x{phys:08X}..0x{:08X})", phys + size) },
                mem::physical_in_use() >> 20,
                c.lr as u32,
                diag::guest_stack(c.r[1] as u32)
            ));
        }
        ret(c, a);
    }
    MmFreePhysicalMemory => |c| {
        let a = arg(c, 1);
        if diag::TRACE_MEMORY.load(std::sync::atomic::Ordering::Relaxed) {
            let size = mem::allocation_size(a);
            let phys = mem::physical_address(a).unwrap_or(0);
            diag::log(&format!(
                "MmFreePhysicalMemory(0x{a:08X}): physical 0x{phys:08X}, 0x{size:X} bytes [from 0x{:08X} {}]",
                c.lr as u32,
                diag::guest_stack(c.r[1] as u32)
            ));
        }
        if !mem::free_physical(a) {
            diag::fatal(&format!("MmFreePhysicalMemory(0x{a:08X}): not a physical allocation"));
        }
    }
    MmGetPhysicalAddress => |c| {
        let a = arg(c, 0);
        match mem::physical_address(a) {
            Some(p) => ret(c, p),
            None => diag::fatal(&format!("MmGetPhysicalAddress(0x{a:08X}): not in a physical memory view")),
        }
    }
    ExAllocatePoolTypeWithTag => |c| {
        let size = arg(c, 0);
        ret(c, mem::kalloc(size, 16));
    }
    ExFreePool => |c| {
        let a = arg(c, 0);
        if !mem::kfree(a) {
            diag::fatal(&format!("ExFreePool(0x{a:08X}): not a pool allocation"));
        }
    }

    // ---- Handles and objects

    NtClose => |c| {
        let h = arg(c, 0);
        if let Some(Object::File(f)) = object::get(h) {
            if Arc::strong_count(&f) <= 2 {
                f.close();
            }
        }
        ret(c, if object::close(h) { STATUS_SUCCESS } else { STATUS_INVALID_HANDLE });
    }
    NtDuplicateObject => |c| {
        let (h, new_ptr, options) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let Some(o) = object::get(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        out32(new_ptr, object::insert(o));
        if options & 1 != 0 {
            object::close(h);
        }
        ret(c, STATUS_SUCCESS);
    }
    ObReferenceObjectByHandle => |c| {
        let (h, kind, out) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let Some(o) = object::get(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        if kind != 0 && kind == module().thread_object_type && !matches!(o, Object::Thread(_)) {
            return ret(c, STATUS_OBJECT_TYPE_MISMATCH);
        }
        let body = match &o {
            Object::Other(..) => diag::fatal(&format!("ObReferenceObjectByHandle on a {} handle", o.kind())),
            _ => o.waitable().unwrap(),
        };
        out32(out, body);
        ret(c, STATUS_SUCCESS);
    }
    ObReferenceObject => |c| {}
    ObDereferenceObject => |c| {}
    ObIsTitleObject => |c| { ret(c, 1); }
    ObCreateSymbolicLink => |c| {
        let (link, target) = (mem::ansi_string(arg(c, 0)), mem::ansi_string(arg(c, 1)));
        let (Some(link), Some(target)) = (link, target) else { return ret(c, STATUS_INVALID_PARAMETER) };
        diag::trace_file(&format!("ObCreateSymbolicLink {link} -> {target}"));
        file::create_link(&link, &target);
        ret(c, STATUS_SUCCESS);
    }
    ObDeleteSymbolicLink => |c| {
        let Some(link) = mem::ansi_string(arg(c, 0)) else { return ret(c, STATUS_INVALID_PARAMETER) };
        ret(c, if file::delete_link(&link) { STATUS_SUCCESS } else { STATUS_OBJECT_NAME_NOT_FOUND });
    }

    // ---- Files

    NtCreateFile => |c| {
        let (hp, access, oa, iosb, disposition, options) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 7), arg(c, 8));
        let r = create_file(hp, access, oa, iosb, disposition, options);
        ret(c, r);
    }
    NtOpenFile => |c| {
        let (hp, access, oa, iosb, options) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 5));
        let r = create_file(hp, access, oa, iosb, file::FILE_OPEN, options);
        ret(c, r);
    }
    NtReadFile => |c| {
        let (h, event, apc, apc_ctx, iosb, buf, len, offp) =
            (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7));
        let Some(f) = file_of(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let offset = file_offset(offp);
        let r = f.read(buf, len, offset);
        diag::trace_file(&format!("NtReadFile {} @{:?} len 0x{len:X} -> {:?}", f.guest_path, offset, r));
        let (status, n) = match r { Ok(n) => (STATUS_SUCCESS, n), Err(s) => (s, 0) };
        io_status(iosb, status, n);
        complete_io(&f, event, apc, apc_ctx, iosb);
        ret(c, status);
    }
    NtWriteFile => |c| {
        let (h, event, apc, apc_ctx, iosb, buf, len, offp) =
            (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7));
        let Some(f) = file_of(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let r = f.write(buf, len, file_offset(offp));
        diag::trace_file(&format!("NtWriteFile {} len 0x{len:X} -> {r:?}", f.guest_path));
        let (status, n) = match r { Ok(n) => (STATUS_SUCCESS, n), Err(s) => (s, 0) };
        io_status(iosb, status, n);
        complete_io(&f, event, apc, apc_ctx, iosb);
        ret(c, status);
    }
    NtFlushBuffersFile => |c| {
        io_status(arg(c, 1), STATUS_SUCCESS, 0);
        ret(c, STATUS_SUCCESS);
    }
    NtCancelIoFile => |c| {
        io_status(arg(c, 1), STATUS_SUCCESS, 0);
        ret(c, STATUS_SUCCESS);
    }
    NtQueryFullAttributesFile => |c| {
        let (oa, out) = (arg(c, 0), arg(c, 1));
        let Some(path) = object_path(oa) else { return ret(c, STATUS_OBJECT_NAME_INVALID) };
        let r = file::stat(&path);
        diag::trace_file(&format!("NtQueryFullAttributesFile {path} -> {}", if r.is_ok() { "found" } else { "missing" }));
        match r {
            Ok(info) => {
                info.write_network_open(out);
                ret(c, STATUS_SUCCESS);
            }
            Err(s) => ret(c, s),
        }
    }
    NtQueryInformationFile => |c| {
        let (h, iosb, out, len, class) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let Some(f) = file_of(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let r = query_file(&f, out, len, class);
        io_status(iosb, r.0, r.1);
        ret(c, r.0);
    }
    NtSetInformationFile => |c| {
        let (h, iosb, info, len, class) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let Some(f) = file_of(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let status = match class {
            14 => {
                f.state.lock().unwrap().position = mem::r64(info);
                STATUS_SUCCESS
            }
            19 | 20 => f.set_size(mem::r64(info)).err().unwrap_or(STATUS_SUCCESS),
            13 => {
                f.state.lock().unwrap().delete_on_close = mem::r8(info) != 0;
                STATUS_SUCCESS
            }
            4 => STATUS_SUCCESS, // basic information (times, attributes): accepted, not stored
            _ => diag::fatal(&format!("NtSetInformationFile class {class} on {}", f.guest_path)),
        };
        io_status(iosb, status, if status == STATUS_SUCCESS { len } else { 0 });
        ret(c, status);
    }
    NtQueryVolumeInformationFile => |c| {
        let (h, iosb, out, len, class) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let Some(f) = file_of(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let r = query_volume(&f, out, len, class);
        io_status(iosb, r.0, r.1);
        ret(c, r.0);
    }
    NtQueryDirectoryFile => |c| {
        let (h, event, apc, apc_ctx, iosb, out, len, pattern, restart) =
            (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7), arg(c, 8));
        let Some(f) = file_of(h) else { return ret(c, STATUS_INVALID_HANDLE) };
        let first = f.state.lock().unwrap().listing.is_none();
        let pat = mem::ansi_string(pattern);
        let (status, n) = match f.next_entry(pat.as_deref(), restart != 0) {
            Some(e) => {
                let name = e.name.as_bytes();
                if (len as usize) < 0x40 + name.len() {
                    (STATUS_BUFFER_OVERFLOW, 0)
                } else {
                    mem::fill(out, 0x40, 0);
                    mem::w64(out + 0x8, e.info.created);
                    mem::w64(out + 0x10, e.info.accessed);
                    mem::w64(out + 0x18, e.info.written);
                    mem::w64(out + 0x20, e.info.written);
                    mem::w64(out + 0x28, e.info.size);
                    mem::w64(out + 0x30, e.info.allocation());
                    mem::w32(out + 0x38, e.info.attributes());
                    mem::w32(out + 0x3C, name.len() as u32);
                    mem::write(out + 0x40, name);
                    (STATUS_SUCCESS, 0x40 + name.len() as u32)
                }
            }
            None => (if first || restart != 0 { STATUS_NO_SUCH_FILE } else { STATUS_NO_MORE_FILES }, 0),
        };
        diag::trace_file(&format!("NtQueryDirectoryFile {} {:?} -> 0x{status:08X}", f.guest_path, pat));
        io_status(iosb, status, n);
        complete_io(&f, event, apc, apc_ctx, iosb);
        ret(c, status);
    }
    FscSetCacheElementCount => |c| { ret(c, STATUS_SUCCESS); }

    // ---- Strings and time

    RtlInitAnsiString => |c| {
        let (dst, src) = (arg(c, 0), arg(c, 1));
        let len = if src != 0 { mem::cstr(src, 0xFFFE).len() as u16 } else { 0 };
        mem::w16(dst, len);
        mem::w16(dst + 2, if src != 0 { len + 1 } else { 0 });
        mem::w32(dst + 4, src);
    }
    RtlInitUnicodeString => |c| {
        let (dst, src) = (arg(c, 0), arg(c, 1));
        let len = if src != 0 { (mem::wstr(src, 0x7FFE).len() * 2) as u16 } else { 0 };
        mem::w16(dst, len);
        mem::w16(dst + 2, if src != 0 { len + 2 } else { 0 });
        mem::w32(dst + 4, src);
    }
    RtlFreeAnsiString => |c| {
        let s = arg(c, 0);
        let buf = mem::r32(s + 4);
        if buf != 0 {
            mem::kfree(buf);
        }
        mem::w16(s, 0);
        mem::w16(s + 2, 0);
        mem::w32(s + 4, 0);
    }
    RtlUnicodeStringToAnsiString => |c| {
        let (dst, src, alloc) = (arg(c, 0), arg(c, 1), arg(c, 2) != 0);
        let s = mem::unicode_string(src).unwrap_or_default();
        let bytes: Vec<u8> = s.chars().map(|ch| if (ch as u32) < 0x100 { ch as u8 } else { b'?' }).collect();
        let buf = if alloc {
            let b = mem::kalloc(bytes.len() as u32 + 1, 4);
            mem::w16(dst + 2, bytes.len() as u16 + 1);
            mem::w32(dst + 4, b);
            b
        } else {
            if (mem::r16(dst + 2) as usize) < bytes.len() + 1 {
                return ret(c, STATUS_BUFFER_OVERFLOW);
            }
            mem::r32(dst + 4)
        };
        mem::write(buf, &bytes);
        mem::w8(buf + bytes.len() as u32, 0);
        mem::w16(dst, bytes.len() as u16);
        ret(c, STATUS_SUCCESS);
    }
    RtlMultiByteToUnicodeN => |c| {
        let (dst, dst_bytes, out_len, src, src_bytes) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let n = (dst_bytes / 2).min(src_bytes);
        for i in 0..n {
            mem::w16(dst + 2 * i, mem::r8(src + i) as u16);
        }
        out32(out_len, n * 2);
        ret(c, STATUS_SUCCESS);
    }
    RtlUnicodeToMultiByteN => |c| {
        let (dst, dst_bytes, out_len, src, src_bytes) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let n = dst_bytes.min(src_bytes / 2);
        for i in 0..n {
            let u = mem::r16(src + 2 * i);
            mem::w8(dst + i, if u < 0x100 { u as u8 } else { b'?' });
        }
        out32(out_len, n);
        ret(c, STATUS_SUCCESS);
    }
    RtlUpcaseUnicodeChar => |c| {
        let u = arg(c, 0) as u16;
        let up = char::from_u32(u as u32).map(|ch| ch.to_uppercase().next().unwrap_or(ch) as u32).unwrap_or(u as u32);
        ret(c, if up < 0x1_0000 { up } else { u as u32 });
    }
    RtlCompareStringN => |c| {
        let (s1, l1, s2, l2, ci) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4) != 0);
        let len = |s: u32, l: u32| if l == 0xFFFF { mem::cstr(s, 0xFFFF).len() as u32 } else { l };
        let (l1, l2) = (len(s1, l1), len(s2, l2));
        let n = l1.min(l2);
        let mut r = 0i32;
        for i in 0..n {
            let (mut a, mut b) = (mem::r8(s1 + i), mem::r8(s2 + i));
            if ci {
                a = a.to_ascii_uppercase();
                b = b.to_ascii_uppercase();
            }
            if a != b {
                r = a as i32 - b as i32;
                break;
            }
        }
        if r == 0 {
            r = l1 as i32 - l2 as i32;
        }
        ret(c, r as u32);
    }
    RtlCompareMemoryUlong => |c| {
        let (src, len, pattern) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let mut n = 0;
        while n + 4 <= len && mem::r32(src + n) == pattern {
            n += 4;
        }
        ret(c, n);
    }
    RtlFillMemoryUlong => |c| {
        let (dst, len, pattern) = (arg(c, 0), arg(c, 1), arg(c, 2));
        for i in 0..len / 4 {
            mem::w32(dst + 4 * i, pattern);
        }
    }
    RtlNtStatusToDosError => |c| { ret(c, to_dos_error(arg(c, 0))); }
    KeQuerySystemTime => |c| { mem::w64(arg(c, 0), time::system_time()); }
    KeQueryPerformanceFrequency => |c| { ret64(c, time::PERFORMANCE_FREQUENCY); }
    RtlTimeToTimeFields => |c| {
        let (t, out) = (arg(c, 0), arg(c, 1));
        let f = time::to_fields(mem::r64(t));
        for (i, v) in [f.year, f.month, f.day, f.hour, f.minute, f.second, f.millis, f.weekday].iter().enumerate() {
            mem::w16(out + 2 * i as u32, *v);
        }
    }
    RtlTimeFieldsToTime => |c| {
        let (fp, out) = (arg(c, 0), arg(c, 1));
        let g = |i: u32| mem::r16(fp + 2 * i);
        let f = time::TimeFields {
            year: g(0), month: g(1), day: g(2), hour: g(3), minute: g(4), second: g(5), millis: g(6), weekday: g(7),
        };
        match time::from_fields(&f) {
            Some(t) => {
                mem::w64(out, t);
                ret(c, 1);
            }
            None => ret(c, 0),
        }
    }

    // ---- Exceptions

    RtlRaiseException => |c| {
        let rec = arg(c, 0);
        let code = mem::r32(rec);
        let nparams = mem::r32(rec + 0x10);
        let param = |i: u32| mem::r32(rec + 0x14 + 4 * i);
        if code == 0x406D_1388 && nparams >= 3 && param(0) == 0x1000 {
            // MSVC "set thread name": a debugger would record it and continue.
            let name = mem::latin1(&mem::cstr(param(1), 256));
            let tid = param(2);
            let t = if tid == u32::MAX { thread::try_current() } else { thread::all().into_iter().find(|t| t.id == tid && !t.finished.load(Ordering::Relaxed)) };
            if let Some(t) = t {
                diag::trace_thread(&format!("thread {} is '{name}'", t.id));
                if name == "Bink Snd" {
                    object::BINK_SOUND_STARTED.store(true, Ordering::Relaxed);
                }
                t.set_name(&name);
                if t.id == thread::current().id {
                    diag::name_host_thread(&name);
                }
            }
            return;
        }
        let params: Vec<String> = (0..nparams.min(15)).map(|i| format!("0x{:08X}", param(i))).collect();
        diag::fatal(&format!(
            "RtlRaiseException code 0x{code:08X} flags 0x{:08X} address 0x{:08X} params [{}]: \
             structured exception dispatch is not implemented yet",
            mem::r32(rec + 4),
            mem::r32(rec + 0xC),
            params.join(", ")
        ));
    }

    // ---- Executable and module queries

    XexCheckExecutablePrivilege => |c| {
        let p = arg(c, 0);
        ret(c, if p < 32 { (module().system_flags >> p) & 1 } else { 0 });
    }
    XexGetModuleHandle => |c| {
        let (name, out) = (arg(c, 0), arg(c, 1));
        let m = module();
        let h = if name == 0 {
            m.ldr
        } else {
            match mem::latin1(&mem::cstr(name, 260)).to_ascii_lowercase().as_str() {
                "xam.xex" => m.xam_ldr,
                "xboxkrnl.exe" => m.kernel_ldr,
                "default.xex" => m.ldr,
                other => {
                    diag::log(&format!("XexGetModuleHandle(\"{other}\"): no such module"));
                    out32(out, 0);
                    return ret(c, 0xC000_0135); // STATUS_DLL_NOT_FOUND
                }
            }
        };
        out32(out, h);
        ret(c, STATUS_SUCCESS);
    }
    XexGetProcedureAddress => |c| {
        let (h, ordinal, out) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let m = module();
        let lib = if h == m.xam_ldr { "xam.xex" } else if h == m.kernel_ldr { "xboxkrnl.exe" } else {
            out32(out, 0);
            return ret(c, 0xC000_007A); // STATUS_PROCEDURE_NOT_FOUND
        };
        let Some(name) = xex::exports::lookup(lib, ordinal) else {
            out32(out, 0);
            return ret(c, 0xC000_007A);
        };
        let addr = crate::procedure_address(lib, ordinal);
        let f = crate::resolve(lib, name).unwrap_or_else(|| {
            // The export exists on the console, so the lookup succeeds; calling
            // it stops the game and names it.
            MISSING_PROCEDURES.lock().unwrap().insert(addr, format!("{lib}!{name} (ordinal 0x{ordinal:X})"));
            missing_procedure as ppc_rt::GuestFn
        });
        ppc_rt::register_function(addr, f);
        out32(out, addr);
        ret(c, STATUS_SUCCESS);
    }
    XexLoadImage => |c| {
        // Only the system modules can be "loaded": they are this kernel. Code
        // from any other module was never recompiled and is never loaded.
        let (name, flags, handle_out) = (arg(c, 0), arg(c, 1), arg(c, 3));
        let m = module();
        let n = mem::latin1(&mem::cstr(name, 260));
        let h = match n.to_ascii_lowercase().rsplit(['\\', ':']).next().unwrap_or("") {
            "xam.xex" => m.xam_ldr,
            "xboxkrnl.exe" => m.kernel_ldr,
            _ => diag::fatal(&format!("XexLoadImage(\"{n}\", 0x{flags:X}): loading another executable module is not supported")),
        };
        out32(handle_out, h);
        ret(c, STATUS_SUCCESS);
    }
    XexUnloadImage => |c| {
        let h = arg(c, 0);
        let m = module();
        if h != m.xam_ldr && h != m.kernel_ldr {
            diag::fatal(&format!("XexUnloadImage(0x{h:08X}): not a module this kernel loaded"));
        }
        ret(c, STATUS_SUCCESS);
    }
    RtlImageXexHeaderField => |c| { ret(c, loader::header_field(arg(c, 0), arg(c, 1))); }
    ExGetXConfigSetting => |c| {
        let (cat, setting, buf, size, req) = (arg(c, 0) as u16, arg(c, 1) as u16, arg(c, 2), arg(c, 3), arg(c, 4));
        let Some(v) = xconfig(cat, setting) else {
            diag::fatal(&format!("ExGetXConfigSetting(category 0x{cat:04X}, setting 0x{setting:04X}) has no value defined"))
        };
        if req != 0 {
            mem::w16(req, v.len() as u16);
        }
        if buf != 0 {
            if (size as usize) < v.len() {
                return ret(c, STATUS_BUFFER_TOO_SMALL);
            }
            mem::write(buf, &v);
        }
        ret(c, STATUS_SUCCESS);
    }
    ExRegisterTitleTerminateNotification => |c| {}

    // ---- Crypto

    XeCryptSha => |c| {
        let mut h = crate::sha1::Sha1::default();
        for i in 0..3 {
            let (p, n) = (arg(c, 2 * i), arg(c, 2 * i + 1));
            if p != 0 && n != 0 {
                h.update(&mem::read(p, n as usize));
            }
        }
        let d = h.finish();
        let (out, out_len) = (arg(c, 6), arg(c, 7));
        mem::write(out, &d[..(out_len as usize).min(20)]);
    }
    XeKeysConsolePrivateKeySign => |c| {
        // No console keys exist here. Saves are signed with a fixed signature,
        // which XeKeysConsoleSignatureVerification accepts.
        mem::fill(arg(c, 1), 0x228, 0);
        ret(c, 1);
    }
    XeKeysConsoleSignatureVerification => |c| {
        out32(arg(c, 2), 0);
        ret(c, 1);
    }

    // ---- Video: display queries (the GPU itself is reached only through D3D)

    VdQueryVideoMode => |c| { video::write_video_mode(arg(c, 0)); }
    VdQueryVideoFlags => |c| { ret(c, video::video_flags()); }
    VdGetCurrentDisplayGamma => |c| {
        out32(arg(c, 0), 2);
        out32(arg(c, 1), 2.222_222_3f32.to_bits());
    }
    VdInitializeEngines => |c| { video::gpu_boundary("VdInitializeEngines") }
    VdShutdownEngines => |c| { video::gpu_boundary("VdShutdownEngines") }
    VdInitializeRingBuffer => |c| { video::gpu_boundary("VdInitializeRingBuffer") }
    VdEnableRingBufferRPtrWriteBack => |c| { video::gpu_boundary("VdEnableRingBufferRPtrWriteBack") }
    VdGetSystemCommandBuffer => |c| { video::gpu_boundary("VdGetSystemCommandBuffer") }
    VdSetSystemCommandBufferGpuIdentifierAddress => |c| { video::gpu_boundary("VdSetSystemCommandBufferGpuIdentifierAddress") }
    VdInitializeScalerCommandBuffer => |c| { video::gpu_boundary("VdInitializeScalerCommandBuffer") }
    VdSetGraphicsInterruptCallback => |c| { video::gpu_boundary("VdSetGraphicsInterruptCallback") }
    VdSwap => |c| { video::gpu_boundary("VdSwap") }
    VdRetrainEDRAM => |c| { video::gpu_boundary("VdRetrainEDRAM") }
    VdRetrainEDRAMWorker => |c| { video::gpu_boundary("VdRetrainEDRAMWorker") }
    // Calls the kernel's registered display-notification routines (XAM's,
    // registered through an export the game doesn't use). None are registered
    // here, so there is nothing to call; this is not a GPU operation.
    VdCallGraphicsNotificationRoutines => |c| { ret(c, 0); }
    // The CPU-GPU link training at power-on: it succeeded.
    VdIsHSIOTrainingSucceeded => |c| { ret(c, 1); }
    VdEnableDisableClockGating => |c| { video::gpu_boundary("VdEnableDisableClockGating") }

    // ---- Audio: queries (the render driver and XMA come with Phase 5)

    XAudioGetSpeakerConfig => |c| {
        out32(arg(c, 0), 0x0001_0001);
        ret(c, STATUS_SUCCESS);
    }
    XAudioGetVoiceCategoryVolume => |c| {
        out32(arg(c, 1), 1.0f32.to_bits());
        ret(c, STATUS_SUCCESS);
    }
    // A hardware XMA decoder context. The XMA hardware is never modelled: the
    // code asking for one (an XMA voice) must be replaced with software decoding.
    XMACreateContext => |c| {
        diag::fatal(
            "XMACreateContext: a hardware XMA decoder context was requested. This is the audio boundary: \
             the XMA voice asking for it must be replaced with software XMA2 decoding (Phase 5), never the decoder emulated",
        )
    }
    XMAReleaseContext => |c| { diag::fatal("XMAReleaseContext: no hardware XMA context can exist") }
    XAudioRegisterRenderDriverClient => |c| {
        let (cb, driver_out) = (arg(c, 0), arg(c, 1));
        let (callback, argument) = (mem::r32(cb), mem::r32(cb + 4));
        let d = audio::register(callback, argument);
        diag::trace_thread(&format!("XAudioRegisterRenderDriverClient callback 0x{callback:08X} arg 0x{argument:08X} -> 0x{d:08X}"));
        out32(driver_out, d);
        ret(c, STATUS_SUCCESS);
    }
    XAudioUnregisterRenderDriverClient => |c| {
        ret(c, if audio::unregister(arg(c, 0)) { STATUS_SUCCESS } else { STATUS_INVALID_PARAMETER });
    }
    XAudioSubmitRenderDriverFrame => |c| {
        audio::submit(arg(c, 0), arg(c, 1));
        ret(c, STATUS_SUCCESS);
    }
}

/// Exports handed out by `XexGetProcedureAddress` without a native implementation.
static MISSING_PROCEDURES: std::sync::Mutex<std::collections::BTreeMap<u32, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Called through a procedure address (`mtctr; bctrl`, so CTR holds the address).
extern "C-unwind" fn missing_procedure(c: &mut Ctx, _: *mut u8) {
    let addr = c.ctr as u32;
    let name = MISSING_PROCEDURES.lock().unwrap().get(&addr).cloned().unwrap_or_else(|| format!("0x{addr:08X}"));
    diag::fatal(&format!("{name}, obtained through XexGetProcedureAddress, has no native implementation yet"))
}

fn snprintf_out(buf: u32, count: u32, s: &[u8]) -> u32 {
    let count = count as usize;
    if s.len() < count {
        mem::write(buf, s);
        mem::w8(buf + s.len() as u32, 0);
        s.len() as u32
    } else {
        mem::write(buf, &s[..count]);
        if s.len() == count { s.len() as u32 } else { u32::MAX }
    }
}

/// `PLARGE_INTEGER ByteOffset`: null or -2 (`FILE_USE_FILE_POINTER_POSITION`) = current position.
fn file_offset(p: u32) -> Option<u64> {
    if p == 0 {
        return None;
    }
    let v = mem::r64(p);
    if v as i64 == -2 || v as i64 == -1 { None } else { Some(v) }
}

fn create_file(hp: u32, access: u32, oa: u32, iosb: u32, disposition: u32, options: u32) -> u32 {
    let Some(path) = object_path(oa) else { return STATUS_OBJECT_NAME_INVALID };
    let r = file::open(&path, access, disposition, options);
    diag::trace_file(&format!(
        "open {path} access 0x{access:08X} disp {disposition} opts 0x{options:X} -> {}",
        match &r { Ok((f, _)) => format!("{}", f.host.display()), Err(s) => format!("0x{s:08X}") }
    ));
    match r {
        Ok((f, info)) => {
            out32(hp, object::insert(Object::File(Arc::new(f))));
            io_status(iosb, STATUS_SUCCESS, info);
            STATUS_SUCCESS
        }
        Err(s) => {
            io_status(iosb, s, if s == STATUS_OBJECT_NAME_NOT_FOUND { file::FILE_DOES_NOT_EXIST } else { 0 });
            out32(hp, 0);
            s
        }
    }
}

/// `NtQueryInformationFile` classes. Returns `(status, bytes written)`.
fn query_file(f: &file::File, out: u32, len: u32, class: u32) -> (u32, u32) {
    let info = f.info();
    let need = |n: u32| len >= n;
    match class {
        4 if need(0x28) => {
            mem::w64(out, info.created);
            mem::w64(out + 8, info.accessed);
            mem::w64(out + 0x10, info.written);
            mem::w64(out + 0x18, info.written);
            mem::w32(out + 0x20, info.attributes());
            mem::w32(out + 0x24, 0);
            (STATUS_SUCCESS, 0x28)
        }
        5 if need(0x16) => {
            mem::w64(out, info.allocation());
            mem::w64(out + 8, info.size);
            mem::w32(out + 0x10, 1);
            mem::w8(out + 0x14, 0);
            mem::w8(out + 0x15, info.is_dir as u8);
            (STATUS_SUCCESS, 0x18.min(len))
        }
        6 if need(8) => {
            let h = f.host.to_string_lossy().bytes().fold(0xCBF2_9CE4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3));
            mem::w64(out, h);
            (STATUS_SUCCESS, 8)
        }
        14 if need(8) => {
            mem::w64(out, f.state.lock().unwrap().position);
            (STATUS_SUCCESS, 8)
        }
        17 if need(4) => {
            mem::w32(out, 0);
            (STATUS_SUCCESS, 4)
        }
        20 if need(8) => {
            mem::w64(out, info.size);
            (STATUS_SUCCESS, 8)
        }
        34 if need(0x38) => {
            info.write_network_open(out);
            (STATUS_SUCCESS, 0x38)
        }
        // XCTD ("compressed title data") information: bit 0 = the file is an
        // XCTD container (magic 0x0FF512ED). XAPI decompresses such files itself.
        27 if need(4) => {
            let mut magic = [0u8; 4];
            let is_xctd = std::fs::File::open(&f.host)
                .and_then(|mut h| std::io::Read::read_exact(&mut h, &mut magic))
                .is_ok()
                && u32::from_be_bytes(magic) == 0x0FF5_12ED;
            mem::w32(out, is_xctd as u32);
            (STATUS_SUCCESS, 4)
        }
        4 | 5 | 6 | 14 | 17 | 20 | 27 | 34 => (STATUS_INFO_LENGTH_MISMATCH, 0),
        _ => diag::fatal(&format!("NtQueryInformationFile class {class} on {}", f.guest_path)),
    }
}

/// `NtQueryVolumeInformationFile` classes.
fn query_volume(f: &file::File, out: u32, len: u32, class: u32) -> (u32, u32) {
    match class {
        1 => {
            // FILE_FS_VOLUME_INFORMATION: time, serial, label length, supports objects, label.
            let label = f.label.as_bytes();
            let n = 0x12 + label.len() as u32;
            if len < 0x12 {
                return (STATUS_INFO_LENGTH_MISMATCH, 0);
            }
            mem::w64(out, 0);
            mem::w32(out + 8, 0x1234_5678);
            mem::w32(out + 0xC, label.len() as u32);
            mem::w8(out + 0x10, 0);
            mem::w8(out + 0x11, 0);
            let k = (len - 0x12).min(label.len() as u32);
            mem::write(out + 0x12, &label[..k as usize]);
            (if k < label.len() as u32 { STATUS_BUFFER_OVERFLOW } else { STATUS_SUCCESS }, n.min(len))
        }
        3 => {
            // FILE_FS_SIZE_INFORMATION: total units, free units, sectors per unit, bytes per sector.
            if len < 0x18 {
                return (STATUS_INFO_LENGTH_MISMATCH, 0);
            }
            let (total, free) = if f.read_only { (0x20_0000u64, 0u64) } else { (0x400_0000u64, 0x200_0000u64) };
            mem::w64(out, total);
            mem::w64(out + 8, free);
            mem::w32(out + 0x10, 8);
            mem::w32(out + 0x14, 0x200);
            (STATUS_SUCCESS, 0x18)
        }
        4 => {
            // FILE_FS_DEVICE_INFORMATION: device type, characteristics.
            if len < 8 {
                return (STATUS_INFO_LENGTH_MISMATCH, 0);
            }
            mem::w32(out, if f.read_only { 0x02 } else { 0x07 }); // CD-ROM / disk
            mem::w32(out + 4, if f.read_only { 0x01 } else { 0 }); // removable
            (STATUS_SUCCESS, 8)
        }
        5 => {
            // FILE_FS_ATTRIBUTE_INFORMATION: attributes, max name length, name length, name.
            let name: &[u8] = if f.read_only { b"GDFX" } else { b"FATX" };
            if len < 0xC {
                return (STATUS_INFO_LENGTH_MISMATCH, 0);
            }
            mem::w32(out, 0);
            mem::w32(out + 4, 255);
            mem::w32(out + 8, name.len() as u32);
            let k = (len - 0xC).min(name.len() as u32);
            mem::write(out + 0xC, &name[..k as usize]);
            (STATUS_SUCCESS, 0xC + k)
        }
        _ => diag::fatal(&format!("NtQueryVolumeInformationFile class {class} on {}", f.guest_path)),
    }
}
