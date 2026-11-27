//! Presenting frames.

use std::sync::atomic::{AtomicU64, Ordering};

use kernel::abi::arg;
use kernel::{diag, mem};
use ppc_rt::Ctx;

pub static FRAMES: AtomicU64 = AtomicU64::new(0);

/// `sub_82BB6BB0(fps)`: the game's frame-rate mode. It takes 60, 50, 30 or 25
/// (0 means 60) and derives from it everything the game times by, in the
/// struct at `0x8392DC38`:
/// - `+0x08` the rate, which also picks D3D's presentation interval (two
///   blanks for 30 and 25, `sub_82BA84E8`);
/// - `+0x20` the rate as a float;
/// - `+0x24` the step per frame;
/// - `+0x28` milliseconds per frame.
///
/// Its logic advances one step per frame and sizes its timers as rate times
/// seconds, so every rate runs at the right speed. The game asks for 30 on its
/// loading screens. The port runs at 60 throughout (the user's choice,
/// 2026-11-24): each request becomes 60, through the game's own setter, so
/// the derived values stay consistent. `WWE14_FRAME_RATE=native` keeps the
/// game's own choices.
pub extern "C-unwind" fn set_frame_rate(c: &mut Ctx, b: *mut u8) {
    let asked = arg(c, 0);
    static NATIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let native = *NATIVE.get_or_init(|| std::env::var("WWE14_FRAME_RATE").is_ok_and(|v| v == "native"));
    if !native && asked != 60 && asked != 0 {
        static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if LOGGED.fetch_add(1, Ordering::Relaxed) < 20 {
            diag::log(&format!("present: the game asked for {asked} frames a second [from 0x{:08X}]; running at 60", c.lr as u32));
        }
        c.r[3] = 60;
    }
    unsafe { ppc_rt::original(0x82BB_6BB0).expect("the frame-rate setter is replaced")(c, b) };
}

/// `sub_82BB6D50()`: the game's switch to half rate. From 60 it sets every
/// field `set_frame_rate` derives to their 30 values (50: 25). Loading screens
/// use it, and `sub_82BB6DF0` switches back. Under the 60 fps lock it does
/// nothing, so the game stays at 60 with consistent steps and timers;
/// `WWE14_FRAME_RATE=native` runs the original.
pub extern "C-unwind" fn halve_frame_rate(c: &mut Ctx, b: *mut u8) {
    static NATIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *NATIVE.get_or_init(|| std::env::var("WWE14_FRAME_RATE").is_ok_and(|v| v == "native")) {
        unsafe { ppc_rt::original(0x82BB_6D50).expect("the half-rate switch is replaced")(c, b) };
        return;
    }
    static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 20 {
        diag::log(&format!("present: the game switched to half frame rate [from 0x{:08X}]; staying at 60", c.lr as u32));
    }
    HALF_RATE_WANTED.store(true, Ordering::Relaxed);
    // Entrances, cutscenes and loading screens start here: a sample of each
    // when `WWE14_PROFILE_AUTO` is set.
    kernel::profile::scene(&format!("a half-rate scene starts at frame {} [from 0x{:08X}]", kernel::video::PRESENTED.load(Ordering::Relaxed), c.lr as u32));
}

/// D3D's presentation interval, in vertical blanks per frame (0: immediate).
///
/// `SynchronizeToPresentationInterval` (`sub_82CB3300`, `docs/d3d.md`) reads
/// the device's interval at `+0x36B0` and maps it the way this does before
/// making the GPU wait: 0 (default) and 1 are one blank, 2 is two, 4 is three,
/// anything else (immediate) none.
fn presentation_interval(dev: u32, frame: u64) -> u32 {
    if let Some(from) = uncapped_from()
        && frame >= from
    {
        return 0;
    }
    let v = mem::r32(dev + 0x36B0);
    let n = match v {
        0 | 1 => 1,
        2 => 2,
        4 => 3,
        _ => 0,
    };
    static LAST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);
    if LAST.swap(v, Ordering::Relaxed) != v {
        diag::log(&format!("present: presentation interval {v:#x} ({n} vertical blanks per frame)"));
    }
    n
}

/// `WWE14_FPS_CAP=off`: frames are not paced to the vertical blank (the
/// renderer still bounds how far ahead the game runs); `off@N` from frame N on.
fn uncapped_from() -> Option<u64> {
    static FROM: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *FROM.get_or_init(|| {
        let v = std::env::var("WWE14_FPS_CAP").ok()?;
        let from = match v.split_once('@') {
            Some(("off", n)) => n.parse().ok()?,
            None if v == "off" => 0,
            _ => return None,
        };
        diag::log(&format!("present: frame rate not capped from frame {from} (WWE14_FPS_CAP={v})"));
        Some(from)
    })
}

/// Paces frames to the presentation interval: `interval` 60 Hz periods per
/// frame (0: no pacing), kept on average rather than snapped to vertical blanks.
///
/// The console flips on a vertical blank, so a frame that takes 17 ms there
/// waits for the next blank and costs 33 ms. Here the window presents on its
/// own, and the game's logic is frame-locked (one step per frame), so that
/// snapping turned any frame a little over budget into half speed for that
/// frame: slow motion. Each frame instead has a slot on a fixed 60 Hz
/// schedule. An early frame sleeps until its slot. A late one goes on at
/// once, and the following frames make up the time while they are early. So
/// the game keeps 60 steps a second whenever its frames fit on average. After
/// a stall longer than two periods (a load), the schedule restarts from now
/// rather than rushing.
fn wait_for_vblank(interval: u32) {
    use std::time::{Duration, Instant};
    static NEXT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let mut next = NEXT.lock().unwrap();
    let now = Instant::now();
    if interval == 0 {
        *next = None;
        return;
    }
    let period = Duration::from_nanos(1_000_000_000 / 60) * interval;
    let slot = next.unwrap_or(now);
    if slot > now {
        std::thread::sleep(slot - now);
        *next = Some(slot + period);
    } else if now - slot > 2 * period {
        *next = Some(now + period);
    } else {
        *next = Some(slot + period);
    }
}

/// Callbacks waiting for the GPU to reach them: (function, context).
static CALLBACKS: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());

/// `D3DDevice_InsertCallback` (`sub_82CB71D0(device, type, callback, context)`).
///
/// The original writes a packet that makes the GPU interrupt the CPU when it
/// reaches that point, and D3D's interrupt handler then calls
/// `callback(context)` (the game uses it to timestamp GPU progress). Here the
/// GPU's work is complete once issued, but the callback must still run after
/// the call returns, as it would on the console: it is queued and called at
/// the end of the frame (`Swap`), on the presenting thread.
pub extern "C-unwind" fn insert_callback(c: &mut Ctx, _: *mut u8) {
    let (callback, context) = (arg(c, 2), arg(c, 3));
    {
        static SEEN: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());
        let thread = kernel::thread::current().id;
        let mut seen = SEEN.lock().unwrap();
        if !seen.contains(&(thread, callback)) {
            seen.push((thread, callback));
            diag::log(&format!(
                "InsertCallback: thread {thread} queues 0x{callback:08X} (type {}, context 0x{context:08X}) [from 0x{:08X}]",
                arg(c, 1),
                c.lr as u32
            ));
        }
    }
    if callback != 0 {
        CALLBACKS.lock().unwrap().push((callback, context));
    }
}

/// D3D's fences: `+0x2B1C` is the last fence issued, and the word at
/// `*(+0x2B10)` the last one the GPU completed (the GPU writes it back). A
/// fence is pending while it is newer than the completed one (`BlockOnFence`,
/// `sub_82CB5A20`; resources carry the fence of their last use). Work here is
/// complete once issued, so each frame marks every issued fence completed.
fn complete_fences(dev: u32) {
    let block = mem::r32(dev + 0x2B10);
    if block == 0 {
        return;
    }
    let (issued, completed) = (mem::r32(dev + 0x2B1C), mem::r32(block));
    static REPORTED: std::sync::Once = std::sync::Once::new();
    if issued != completed {
        REPORTED.call_once(|| diag::log(&format!("fences: issued {issued:#x}, completed {completed:#x} (now marked completed)")));
    }
    mem::w32(block, issued);
}

/// Calls the callbacks queued this frame.
fn run_callbacks(c: &mut Ctx) {
    let pending = std::mem::take(&mut *CALLBACKS.lock().unwrap());
    for (callback, context) in pending {
        kernel::thread::call(c, callback, &[context as u64]);
    }
}

/// `D3DDevice_SynchronizeToPresentationInterval` (worker `sub_82CB3300(device,
/// flag)`; the API entry `sub_82CB37B8` passes 0). The game calls it before
/// `Resolve` and `Swap`.
///
/// The original makes the GPU wait for the vertical blank (wait-on-memory
/// packets against the write-back at `*(+0x2B14) + 4`), queues a GPU callback
/// (`sub_82CB2A28`: presentation statistics at `+0x4230..+0x424C`, calling a
/// game-registered callback at `+0x4230`), and refreshes D3D's Hi-Z state
/// through `SetPredication(0)` (`sub_82CB7770`, more packets). Here `Swap` paces to the vertical
/// blank, so the native version keeps only D3D's CPU-side bookkeeping: the
/// predication and Hi-Z state as `SetPredication(device, 0)` leaves it (`predication_state`), and the
/// synchronize flag, `+0x2B3D` bit 4.
pub extern "C-unwind" fn synchronize_to_presentation_interval(c: &mut Ctx, _: *mut u8) {
    let dev = arg(c, 0);
    crate::commands::predication_state(dev, 0);
    mem::w8(dev + 0x2B3D, mem::r8(dev + 0x2B3D) | 0x10);
    static REPORTED: std::sync::Once = std::sync::Once::new();
    let callback = mem::r32(dev + 0x4230);
    if callback != 0 {
        REPORTED.call_once(|| {
            diag::log(&format!(
                "note: the game registered a presentation callback (0x{callback:08X}); \
                 it is not called yet (the GPU would call it through sub_82CB2A28)"
            ))
        });
    }
}

/// `D3DDevice_Swap` (`sub_82CB37C0(device, front_buffer, present_params)`).
///
/// The original queues the front buffer for scan-out: it writes swap packets
/// into the kernel's system command buffer (`VdSwap`), paces the CPU against
/// the GPU with fences, and applies display-mode and gamma changes. Here frames
/// complete as soon as they are presented, so the native version keeps D3D's
/// bookkeeping and closes the frame for the host's renderer (`ir::end_frame`):
/// - `+0x4248` swap counter;
/// - `+0x3B90` copy of the front buffer's texture header (0x34 bytes);
/// - `+0x5614`/`+0x5610` frames issued/completed: both advance together, so
///   nothing ever waits for a frame to finish;
/// - `+0x42DC` pending display changes: consumed (the host owns the display);
/// - `+0x36DC` present parameters (0x1C bytes): kept when given.
pub extern "C-unwind" fn swap(c: &mut Ctx, _: *mut u8) {
    let (dev, front, params) = (arg(c, 0), arg(c, 1), arg(c, 2));
    mem::w32(dev + 0x4248, mem::r32(dev + 0x4248).wrapping_add(1));
    let header = mem::read(front, 0x34);
    mem::write(dev + 0x3B90, &header);
    if mem::r32(dev + 0x5618) != 0 {
        let issued = mem::r32(dev + 0x5614).wrapping_add(2);
        mem::w32(dev + 0x5614, issued);
        mem::w32(dev + 0x5610, issued);
    }
    mem::w32(dev + 0x42DC, 0);
    if params != 0 {
        let p = mem::read(params, 0x1C);
        mem::write(dev + 0x36DC, &p);
    }

    // Front buffer: texture fetch constant at +0x1C (6 words). Word 1 holds the
    // base address (4 KB units), word 2 the size: width-1 in bits 0-12,
    // height-1 in bits 13-25.
    let fetch1 = mem::r32(front + 0x1C + 4);
    let fetch2 = mem::r32(front + 0x1C + 8);
    let (w, h) = ((fetch2 & 0x1FFF) + 1, ((fetch2 >> 13) & 0x1FFF) + 1);
    let n = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    kernel::video::PRESENTED.store(n, Ordering::Relaxed);
    let front_fetch: [u32; 6] = core::array::from_fn(|i| mem::r32(front + 0x1C + 4 * i as u32));
    crate::ir::end_frame(n, Some(front_fetch));
    kernel::trace::frame(n);
    complete_fences(dev);
    run_callbacks(c);

    // The display scans out at 60 Hz and D3D presents on a vertical blank, as
    // many blanks after the previous frame as the presentation interval asks.
    // The frame's own closing work above (reading its textures, handing it to
    // the renderer) comes first, so the wait absorbs it.
    let t = std::time::Instant::now();
    wait_for_vblank(presentation_interval(dev, n));
    crate::ir::perf::add(&crate::ir::perf::VBLANK_NS, &crate::ir::perf::VBLANK_TOTAL, t.elapsed().as_nanos() as u64);
    crate::preview::present();
    // Also every 1,800 frames (30 s at 60 Hz): a heartbeat that shows whether the game still presents.
    if n <= 3 || n.is_power_of_two() || n % 1800 == 0 {
        diag::log(&format!(
            "present #{n}: front buffer {w}x{h} at physical 0x{:08X}",
            fetch1 & 0xFFFF_F000
        ));
    }
}

// ---- Camera cuts in half-rate scenes --------------------------------------------
//
// Entrance and victory cameras are animation tracks whose cut events are kept
// in 30 Hz frames: the event matcher (`sub_82259A18`) takes only even 60 Hz
// frames and compares frame / 2. At the game's own 30 fps the camera clock
// steps by 2 and never stops on the odd frame before a cut; under the 60 lock
// it does, and the pose there is a 50/50 blend of the old shot's last key and
// the new shot's first: one frame of a camera matching neither shot (the
// user, 2026-11-25). On that frame the camera keeps the old shot's last pose,
// as at 30 fps; every other frame still plays at 60. Found statically,
// 2026-11-26.

/// The game asked for half rate (an entrance, a cutscene) and hasn't restored
/// full rate since.
pub static HALF_RATE_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `sub_82BB6DF0`, the game's switch back to full rate.
pub extern "C-unwind" fn restore_frame_rate(c: &mut Ctx, b: *mut u8) {
    HALF_RATE_WANTED.store(false, Ordering::Relaxed);
    unsafe { ppc_rt::original(0x82BB_6DF0).expect("the full-rate switch is replaced")(c, b) };
}

/// The animated camera's class (its players sit at +0x5D0 and +0x720).
const CAMERA_VTABLE: u32 = 0x8201_EF40;

/// Whether the event track at `track` (a player's +0x11C) has an event at the
/// 30 Hz frame `key`: `sub_8225B530`'s tables (the one at manager +0x18, then
/// the five pointers from +0x14 down to +0x04) read as `sub_82259A18` does,
/// without dispatching anything.
fn event_at(c: &mut Ctx, track: u32, key: u32) -> bool {
    let mgr = mem::r32(0x836E_E3B0);
    // Diagnostics (2026-11-26): what the lookup sees, a few times.
    static DUMPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // Partway into a scene (frames 41 and 141), when its tables are loaded.
    let dump = (key == 21 || key == 71) && DUMPS.fetch_add(1, Ordering::Relaxed) < 8;
    if dump {
        diag::log(&format!(
            "camera events: manager 0x{mgr:08X}, track 0x{track:08X} (id {:#x}, index {}, count {}), key {key}",
            mem::r16(track),
            mem::r32(track + 4),
            mem::r16(track + 8)
        ));
    }
    if mgr == 0 {
        return false;
    }
    let tables = std::iter::once(mgr + 0x18).chain((1..=5).rev().map(|k| mem::r32(mgr + 4 * k)));
    for t in tables {
        if t == 0 || [0xC, 8, 0x30, 0x34].iter().any(|&o| mem::r32(t + o) == 0) {
            if dump && t != 0 {
                diag::log(&format!("camera events: table 0x{t:08X} skipped (+8 {:08X}, +C {:08X}, +30 {:08X}, +34 {:08X})", mem::r32(t + 8), mem::r32(t + 0xC), mem::r32(t + 0x30), mem::r32(t + 0x34)));
            }
            continue;
        }
        // `sub_82C39860(id, list, 6)`: is the track's id in this table (-1: no)?
        let found = kernel::thread::call(c, 0x82C3_9860, &[mem::r16(track) as u64, mem::r32(t + 8) as u64, 6]) as u32;
        if dump {
            let base = mem::r32(t + 0xC) + 4 * mem::r32(track + 4);
            let keys: Vec<u16> = (0..(mem::r16(track + 8) as u32).min(24)).map(|i| mem::r16(base + 4 * i + 2)).collect();
            diag::log(&format!("camera events: table 0x{t:08X}: id lookup {found:#x}, keys {keys:?}"));
        }
        if found == u32::MAX {
            continue;
        }
        let base = mem::r32(t + 0xC) + 4 * mem::r32(track + 4);
        let n = mem::r16(track + 8) as u32;
        if (0..n.min(4096)).any(|i| mem::r16(base + 4 * i + 2) as u32 == key) {
            return true;
        }
    }
    false
}

/// `sub_825B6848(player)`, the animated camera's per-frame clock and pose
/// (see above).
pub extern "C-unwind" fn camera_clock(c: &mut Ctx, b: *mut u8) {
    let player = c.r[3] as u32;
    unsafe { ppc_rt::original(0x825B_6848).expect("the camera clock is replaced")(c, b) };
    {
        // Diagnostics (2026-11-26): the first calls, and each odd frame's checks.
        static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let k = CALLS.fetch_add(1, Ordering::Relaxed);
        if k < 40 || k % 600 == 0 {
            diag::log(&format!(
                "camera clock: call {k}, player 0x{player:08X}, [-0x5D0] 0x{:08X}, [-0x720] 0x{:08X}, t {}, half rate {}",
                mem::r32(player.wrapping_sub(0x5D0)),
                mem::r32(player.wrapping_sub(0x720)),
                f32::from_bits(mem::r32(player + 0x38)),
                HALF_RATE_WANTED.load(Ordering::Relaxed)
            ));
        }
    }
    if !HALF_RATE_WANTED.load(Ordering::Relaxed) {
        return;
    }
    let cam = if mem::r32(player.wrapping_sub(0x5D0)) == CAMERA_VTABLE {
        player - 0x5D0
    } else if mem::r32(player.wrapping_sub(0x720)) == CAMERA_VTABLE {
        player - 0x720
    } else {
        return;
    };
    let t = f32::from_bits(mem::r32(player + 0x38));
    if !(t.is_finite() && t >= 1.0) {
        return;
    }
    let n = t.trunc() as u32;
    let result = c.r[3];
    // The cut events may be on either player's track (field of view or position).
    let cut = n % 2 == 1 && (event_at(c, cam + 0x720 + 0x11C, (n + 1) / 2) || event_at(c, cam + 0x5D0 + 0x11C, (n + 1) / 2));
    {
        static ODD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if n % 2 == 1 && player == cam + 0x5D0 && ODD.fetch_add(1, Ordering::Relaxed) < 300 {
            diag::log(&format!("camera clock: odd scene frame {n}, event at the next frame: {cut} (game frame {})", kernel::video::PRESENTED.load(Ordering::Relaxed)));
        }
    }
    if cut {
        // The old shot's last pose: evaluate at n - 1, then put the clock back.
        mem::w32(player + 0x38, ((n - 1) as f32).to_bits());
        kernel::thread::call(c, 0x823E_3860, &[player as u64]);
        mem::w32(player + 0x38, t.to_bits());
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if player == cam + 0x5D0 && N.fetch_add(1, Ordering::Relaxed) < 200 {
            diag::log(&format!("camera: held the old shot on the frame before a cut (scene frame {n}, game frame {})", kernel::video::PRESENTED.load(Ordering::Relaxed)));
        }
    }
    c.r[3] = result;
}

/// `sub_825B69E0(camera, ...)`, the animated camera's per-frame update
/// (vtable `0x8201EF40` slot at `0x8201EFA4`): it advances both players'
/// clocks and writes the frame's pose into the camera (position and target
/// at +0x210..+0x268, field of view at +0x130..+0x13C). On the odd frame
/// before a cut in a half-rate scene that pose is a 50/50 blend of two shots
/// (see above): a jump far larger than the camera's motion the frame before.
/// That frame keeps the previous frame's pose. The event lookup in
/// `camera_clock` found no cut events (the tables at `[0x836EE3B0]` are empty
/// during entrances, user82), so this looks at the result instead.
pub extern "C-unwind" fn camera_update(c: &mut Ctx, b: *mut u8) {
    let cam = c.r[3] as u32;
    unsafe { ppc_rt::original(0x825B_69E0).expect("the camera update is replaced")(c, b) };
    if mem::r32(cam) != CAMERA_VTABLE {
        return;
    }
    // The camera's output state: everything before its two animation players
    // (+0x100..+0x5D0): pose, the matrices and lens values derived from it.
    // Restoring only position, target and field of view still showed the
    // blend (user87). (camera, previous state, previous jump)
    const OUT: (u32, usize) = (0x100, (0x5D0 - 0x100) / 4);
    static LAST: std::sync::Mutex<Option<(u32, Vec<u32>, f32)>> = std::sync::Mutex::new(None);
    let read = || -> Vec<u32> { (0..OUT.1).map(|i| mem::r32(cam + OUT.0 + 4 * i as u32)).collect() };
    let pos = |p: &Vec<u32>| -> [f32; 3] { core::array::from_fn(|k| f32::from_bits(p[(0x210 - 0x100) / 4 + k])) };
    let now = read();
    let t = f32::from_bits(mem::r32(cam + 0x5D0 + 0x38));
    let mut last = LAST.lock().unwrap();
    let Some((prev_cam, prev, prev_jump)) = last.clone() else {
        *last = Some((cam, now, 0.0));
        return;
    };
    if prev_cam != cam {
        *last = Some((cam, now, 0.0));
        return;
    }
    let (a, b2) = (pos(&prev), pos(&now));
    let jump = ((a[0] - b2[0]).powi(2) + (a[1] - b2[1]).powi(2) + (a[2] - b2[2]).powi(2)).sqrt();
    let odd = t.is_finite() && t >= 1.0 && (t.trunc() as u32) % 2 == 1;
    let blend = HALF_RATE_WANTED.load(Ordering::Relaxed) && odd && jump.is_finite() && jump > 8.0 * prev_jump.max(0.5);
    if blend {
        // Not shown either: the picture is decided before this point. Two
        // frames, so the blended one is covered whether the renderer draws
        // this logic frame now or one frame later.
        crate::ir::HOLD_FRAMES.store(2, Ordering::Relaxed);
        // Keep the previous frame's pose; its jump stays the reference.
        for (i, &w) in prev.iter().enumerate() {
            mem::w32(cam + OUT.0 + 4 * i as u32, w);
        }
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, Ordering::Relaxed) < 200 {
            diag::log(&format!(
                "camera: held the previous view on a cut's in-between frame (jump {jump:.1}, before {prev_jump:.1}, scene frame {}, game frame {})",
                t.trunc(),
                kernel::video::PRESENTED.load(Ordering::Relaxed)
            ));
        }
        return;
    }
    *last = Some((cam, now, jump));
}
