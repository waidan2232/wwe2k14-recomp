//! Runtime-library pieces of the kernel: critical sections and guest atomics.
//!
//! `RTL_CRITICAL_SECTION` (0x1C bytes) = a synchronization-event dispatcher
//! header (its `Absolute` byte holds the spin count / 256), then
//! `i32 LockCount` (-1 = free), `i32 RecursionCount`, `KTHREAD *OwningThread`.
//! The algorithm is the classic NT one: contended entries wait on the header,
//! and the last leave with waiters sets it.

use std::sync::atomic::{AtomicU32, Ordering};

use ppc_rt::Ctx;

use crate::mem;
use crate::object;
use crate::thread;

fn atomic(a: u32) -> &'static AtomicU32 {
    assert!(a & 3 == 0, "FATAL: misaligned guest atomic at 0x{a:08X}");
    unsafe { &*(mem::base().add(a as usize) as *const AtomicU32) }
}

/// Atomic compare-and-swap on a big-endian guest word.
pub fn cas(a: u32, expected: u32, new: u32) -> bool {
    atomic(a).compare_exchange(expected.to_be(), new.to_be(), Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Atomic add on a big-endian guest word; returns the new value.
pub fn add(a: u32, delta: i32) -> i32 {
    let p = atomic(a);
    let mut cur = p.load(Ordering::SeqCst);
    loop {
        let new = (u32::from_be(cur) as i32).wrapping_add(delta);
        match p.compare_exchange_weak(cur, (new as u32).to_be(), Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return new,
            Err(c) => cur = c,
        }
    }
}

pub fn init_critical_section(cs: u32, spin: u32) {
    object::init_header(cs, object::SYNCHRONIZATION_EVENT, 0x1C, 0);
    mem::w8(cs + 1, (spin.saturating_add(255) >> 8).min(255) as u8);
    mem::w32(cs + 0x10, u32::MAX);
    mem::w32(cs + 0x14, 0);
    mem::w32(cs + 0x18, 0);
}

pub fn enter_critical_section(c: &mut Ctx, cs: u32) {
    let me = thread::current().kthread;
    if mem::r32(cs + 0x18) == me {
        add(cs + 0x10, 1);
        mem::w32(cs + 0x14, mem::r32(cs + 0x14).wrapping_add(1));
        return;
    }
    let mut spin = mem::r8(cs + 1) as u32 * 256;
    while spin > 0 {
        if cas(cs + 0x10, u32::MAX, 0) {
            mem::w32(cs + 0x18, me);
            mem::w32(cs + 0x14, 1);
            return;
        }
        std::hint::spin_loop();
        spin -= 1;
    }
    if add(cs + 0x10, 1) != 0 {
        object::wait(c, &[cs], true, false, None);
    }
    mem::w32(cs + 0x18, me);
    mem::w32(cs + 0x14, 1);
}

pub fn try_enter_critical_section(cs: u32) -> bool {
    let me = thread::current().kthread;
    if cas(cs + 0x10, u32::MAX, 0) {
        mem::w32(cs + 0x18, me);
        mem::w32(cs + 0x14, 1);
        true
    } else if mem::r32(cs + 0x18) == me {
        add(cs + 0x10, 1);
        mem::w32(cs + 0x14, mem::r32(cs + 0x14).wrapping_add(1));
        true
    } else {
        false
    }
}

pub fn leave_critical_section(cs: u32) {
    let rec = (mem::r32(cs + 0x14) as i32).wrapping_sub(1);
    mem::w32(cs + 0x14, rec as u32);
    if rec != 0 {
        add(cs + 0x10, -1);
        return;
    }
    mem::w32(cs + 0x18, 0);
    if add(cs + 0x10, -1) != -1 {
        object::set_event(cs);
    }
}

/// Spinlock acquire on a guest word (`KfAcquireSpinLock`): owner = KTHREAD.
pub fn acquire_spinlock(lock: u32) {
    let me = thread::current().kthread;
    let mut n = 0u32;
    while !cas(lock, 0, me) {
        n += 1;
        if n % 64 == 0 {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

pub fn release_spinlock(lock: u32) {
    atomic(lock).store(0, Ordering::SeqCst);
}
