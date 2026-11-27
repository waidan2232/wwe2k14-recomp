//! Display queries. The GPU itself is never reached from here: the game's D3D
//! library talks to the GPU's ring buffer and registers, and that library is
//! replaced at its API boundary by the native renderer (Phase 4). A `Vd*`
//! call that would drive the GPU therefore marks the point where the boot
//! has reached graphics initialisation.

use crate::mem;

pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;

/// `XVIDEO_MODE` (0x30 bytes): width, height, interlaced, widescreen, HD,
/// refresh rate (float), video standard (1 = NTSC-M), then two fields the
/// console sets to 0x4A and 0x01.
pub fn write_video_mode(p: u32) {
    mem::fill(p, 0x30, 0);
    mem::w32(p, WIDTH);
    mem::w32(p + 4, HEIGHT);
    mem::w32(p + 8, 0);
    mem::w32(p + 0xC, 1);
    mem::w32(p + 0x10, 1);
    mem::w32(p + 0x14, 60.0f32.to_bits());
    mem::w32(p + 0x18, 1);
    mem::w32(p + 0x1C, 0x4A);
    mem::w32(p + 0x20, 0x01);
}

/// `VdQueryVideoFlags`: bit 0 widescreen, bit 1 width >= 1024, bit 2 width >= 1920.
pub fn video_flags() -> u32 {
    1 | ((WIDTH >= 1024) as u32) << 1 | ((WIDTH >= 1920) as u32) << 2
}

/// `--gpu-boundary park`: instead of stopping the game, block the thread that
/// reached the GPU so the other threads keep running (to find what else they
/// need). Diagnostics only: that thread never continues.
pub static PARK_AT_BOUNDARY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A GPU-hardware entry point was called.
pub fn gpu_boundary(name: &str) -> ! {
    if PARK_AT_BOUNDARY.load(std::sync::atomic::Ordering::Relaxed) {
        let t = crate::thread::current();
        crate::diag::log(&format!(
            "{name}: thread {} '{}' reached the GPU hardware interface (graphics boundary) and is parked for good",
            t.id,
            t.name()
        ));
        loop {
            std::thread::park();
        }
    }
    crate::diag::fatal(&format!(
        "{name}: the game's D3D library reached the GPU hardware interface. \
         This is the graphics boundary: D3D must be replaced natively (Phase 4), never the GPU emulated"
    ))
}

/// Frames the game has presented (`VdSwap`, counted by the GPU layer). The
/// input script (`--input BUTTON@fFRAME`) can be timed by it.
pub static PRESENTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
