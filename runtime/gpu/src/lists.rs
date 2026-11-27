//! The game's render lists, and the one ordering its threads rely on.
//!
//! The main thread records display lists into an arena owned by the renderer
//! (`sub_82BB4AE0`); a cached list is re-recorded when invalidated, at a new
//! place in the arena that may overlap the old one. The render thread walks a
//! frame's lists (`sub_82BB5BE8`), calling the cached ones by address. It lets
//! the main thread start its next frame (event `F8000028`) *before* that walk,
//! so recording and walking overlap. On the console the walk is short, over
//! long before the main thread reaches its list recording. Here every draw of
//! the walk is captured, the walk takes far longer, and a re-recorded list
//! overwrote one the walk had not reached yet: the render thread then ran
//! garbage (the `0xDB170B70` crash in `STATUS.md`).
//!
//! So recording a list waits while another thread is walking. This restores
//! the order the console's timing gave; it is a hook on the game's own
//! threading, not a model of any hardware.

use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use kernel::diag;
use ppc_rt::Ctx;

/// The thread walking the render lists, and how deeply (the walker could nest).
static WALK: Mutex<(u32, u32)> = Mutex::new((0, 0));
static DONE: Condvar = Condvar::new();

fn original(address: u32) -> ppc_rt::GuestFn {
    ppc_rt::original(address).expect("render list function is replaced")
}

/// `sub_82BB5BE8`: the render thread's walk over a frame's render lists.
pub extern "C-unwind" fn walk(c: &mut Ctx, b: *mut u8) {
    let me = kernel::thread::current().id;
    {
        let mut w = WALK.lock().unwrap();
        *w = (me, w.1 + 1);
    }
    let start = Instant::now();
    unsafe { original(0x82BB_5BE8)(c, b) };
    crate::ir::perf::add(&crate::ir::perf::WALK_NS, &crate::ir::perf::WALK_TOTAL, start.elapsed().as_nanos() as u64);
    let mut w = WALK.lock().unwrap();
    w.1 -= 1;
    if w.1 == 0 {
        w.0 = 0;
        DONE.notify_all();
    }
}

/// `sub_82BB4AE0(renderer, slot, buffer)`: records a display list into the
/// arena. Waits for a walk in progress on another thread to finish first.
pub extern "C-unwind" fn record(c: &mut Ctx, b: *mut u8) {
    let me = kernel::thread::current().id;
    let start = Instant::now();
    let mut w = WALK.lock().unwrap();
    while w.1 > 0 && w.0 != me {
        let left = Duration::from_secs(2).saturating_sub(start.elapsed());
        if left.is_zero() {
            // Never deadlock silently: a walk that waits on this thread would.
            diag::log(&format!("render lists: thread {me} recorded a list while thread {} was still walking after 2 s", w.0));
            break;
        }
        w = DONE.wait_timeout(w, left).unwrap().0;
    }
    drop(w);
    crate::ir::perf::LIST_WAIT_NS.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    unsafe { original(0x82BB_4AE0)(c, b) };
}
