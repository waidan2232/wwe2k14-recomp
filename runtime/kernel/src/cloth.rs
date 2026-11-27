//! Cloth settling after camera cuts at the 60 fps lock (John Cena's title
//! belt twitched through his entrance; fine at the game's own 30 fps, user74).
//!
//! At a camera cut or a teleport (`sub_825AC7E8` sets `[0x83767B68]+0x74`),
//! `sub_824F6350` resets the wrestlers' cloth and settles it with a fixed 14
//! steps (2, then 12), each through `sub_824F56E8`, which hands the cloth
//! manager dt = 1 / (frame-rate struct `0x8392DC38` +0x20, the rate as a
//! float). Entrances are made for 30 fps: 14 steps of 1/30 s let a strap hang
//! down. Under the lock the rate stays 60 there, so the settle covered half
//! the time and the strap showed half-fallen after every cut (sticking out,
//! curled). Here those settle steps alone see 30 for the call; ordinary
//! per-frame cloth keeps the real rate. Found statically, 2026-11-26.

use ppc_rt::Ctx;

use crate::mem;

/// The frame-rate struct's rate as a float (60.0 or 30.0).
const RATE_F32: u32 = 0x8392_DC38 + 0x20;

/// Return addresses of the settle loop's two calls in `sub_824F6350`.
const SETTLE_CALLS: [u32; 2] = [0x824F_63E8, 0x824F_6450];

/// `sub_824F56E8`, the per-wrestler cloth time step (see the module).
pub extern "C-unwind" fn cloth_step(c: &mut Ctx, b: *mut u8) {
    let settle = SETTLE_CALLS.contains(&(c.lr as u32));
    let rate = mem::r32(RATE_F32);
    let at_60 = f32::from_bits(rate) > 45.0;
    if settle && at_60 {
        mem::w32(RATE_F32, 30.0f32.to_bits());
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if c.lr as u32 == SETTLE_CALLS[0] && N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            crate::diag::log(&format!("cloth: settling after a cut at 30 fps steps (frame {})", crate::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed)));
        }
    }
    unsafe { ppc_rt::original(0x824F_56E8).expect("the cloth step is replaced")(c, b) };
    if settle && at_60 {
        mem::w32(RATE_F32, rate);
    }
}
