//! Resolves: copying a render target into a texture (and optionally clearing
//! it), which the console does from EDRAM. Here render targets are the
//! renderer's own textures, so a resolve is recorded in the render IR and the
//! renderer copies between textures (plan §2.4 item 5).

use kernel::abi::arg;
use kernel::mem;
use ppc_rt::Ctx;

/// `D3DDevice_Clear` (`sub_82CB52B8(device, count, rects, flags, color, f1 z,
/// stencil, edram_clear)`).
///
/// The original converts the ARGB colour to a float vector and clears the
/// bound targets through command packets. The native version records the
/// clear (rectangles, flags, colour, depth, stencil, and the bound surfaces)
/// for the renderer.
pub extern "C-unwind" fn clear(c: &mut Ctx, _: *mut u8) {
    let (dev, count, rects, flags, color) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    let rects = if rects == 0 {
        Vec::new()
    } else {
        (0..count.min(64)).map(|i| core::array::from_fn(|k| mem::r32(rects + 16 * i + 4 * k as u32))).collect()
    };
    let channel = |s: u32| ((color >> s) & 0xFF) as f32 / 255.0;
    crate::ir::clear(crate::ir::Clear {
        flags,
        rects,
        color: [channel(16), channel(8), channel(0), channel(24)],
        z: c.f[1] as f32,
        stencil: arg(c, 6),
        surface: crate::ir::surface_regs(dev),
    });
}

/// `D3DDevice_Resolve` (`sub_82CAFB88(device, flags, source_rect, dest_texture,
/// dest_point, level, slice, clear_color, f1 clear_z, clear_stencil,
/// parameters)`).
///
/// The original emits packets that copy EDRAM to memory and clear it. Nothing
/// of D3D's state changes for the caller, so the native version only records
/// the resolve: the source surface (from the shadowed `RB_SURFACE_INFO`,
/// `RB_COLOR_INFO`, `RB_DEPTH_INFO`), the rectangle, the destination texture's
/// fetch constant, and the clear values.
pub extern "C-unwind" fn resolve(c: &mut Ctx, _: *mut u8) {
    let (dev, flags, rect, dest, point) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    let (level, slice, color) = (arg(c, 5), arg(c, 6), arg(c, 7));
    record_resolve(dev, flags, rect, dest, point, level, slice, color, c.f[1] as f32, arg(c, 9));
    hold_for_reader(dest);
    watch_destination(c, dest, flags, rect);
}

// ---- Resolved images the game reads on the CPU ---------------------------------------
//
// On the console a resolve copies the render target into the destination
// texture's memory, where the game can read it. Here the image stays with the
// renderer (plan §2.4 item 5: "a guest-memory readback only where the CPU
// actually reads the result"). The game does read one: a created superstar's
// picture. It draws him into a 736x1280 target, resolves that (at EndTiling),
// and four frames later a worker thread (`sub_82CF9F40`, under
// `sub_8341B6A8`) reads the texture's memory, scales it to 324x860 and
// compresses it into the superstar's save. With nothing in that memory the
// saved picture was black, and the game rejected the superstar at the next
// start ("damaged or missing").
//
// So the memory of such a destination is held back (`kernel::mem::watch_with`):
// the first thread that reads it stops, the image is fetched from the
// renderer, converted to the texture's own layout and written there, and the
// read goes on. Nothing is copied for a destination nobody reads.

/// A destination being held: where its memory is, and when it was resolved.
struct Held {
    fetch: [u32; 6],
    phys: u32,
    virt: u32,
    len: u32,
    frame: u64,
}

static HELD: std::sync::Mutex<Vec<Held>> = std::sync::Mutex::new(Vec::new());

/// Is this a destination the game reads on the CPU? Two rules so far, both
/// 8-bit RGBA:
/// - taller than the screen: no scene pass has one (they are at most
///   1280x720, or 736 high as tiles); the superstar picture is 736x1280;
/// - 64x64: the game draws a created item's icon
///   there and compresses it in place to DXT1 on the CPU (a custom
///   championship's picture, user59: the destination at 0xF696A000 was then
///   read as a 64x128 DXT1 texture, blank).
/// - 256x128: a custom championship's picture, drawn when the belt is
///   accepted (user62: the one resolve after Accept, at 0xFE833000).
/// - 512x512: tried for a custom arena's thumbnail (user83: the arena editor
///   resolves two 512x512 RGBA images every frame; which one the thumbnail
///   comes from is not known yet, so both are held; a read is logged).
/// A resolve the game reads that this misses leaves its reader with stale
/// memory, as before; `WWE14_WATCH_RESOLVE=all` shows every destination.
fn read_on_cpu(d: &crate::texture::Fetch) -> bool {
    d.format == 6
        && (d.height > 736
            || (d.width == 64 && d.height == 64)
            || (d.width == 256 && d.height == 128)
            || (d.width == 512 && d.height == 512)
            || (d.width == 1280 && d.height == 720))
}

/// Destinations read again and again (scene targets whose memory something
/// reads every frame) are not held any more: each read costs a readback from
/// the GPU. A picture the game reads back is read once after each resolve.
static NEVER_HOLD: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
/// (address, frame) of recent fills.
static RECENT_FILLS: std::sync::Mutex<Vec<(u32, u64)>> = std::sync::Mutex::new(Vec::new());

/// After a resolve into `dest` (a texture header): holds its memory for a
/// reader if the game reads such images.
fn hold_for_reader(dest: u32) {
    if dest == 0 {
        return;
    }
    let fetch: [u32; 6] = core::array::from_fn(|i| mem::r32(dest + 0x1C + 4 * i as u32));
    let Some(d) = crate::texture::Fetch::decode(fetch) else { return };
    if !read_on_cpu(&d) {
        return;
    }
    let Some((virt, len)) = crate::texture::guest_range(&d) else { return };
    if NEVER_HOLD.lock().unwrap().contains(&virt) {
        return;
    }
    {
        // Each held destination once in the log (200 at most).
        static LOGGED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        let mut logged = LOGGED.lock().unwrap();
        if !logged.contains(&virt) && logged.len() < 200 {
            logged.push(virt);
            kernel::diag::log(&format!("resolve: holding the {}x{} image at 0x{virt:08X}+0x{len:X} for a CPU reader", d.width, d.height));
        }
    }
    let frame = kernel::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed);
    let mut held = HELD.lock().unwrap();
    held.retain(|h| h.virt != virt);
    held.push(Held { fetch, phys: crate::texture::physical(d.base), virt, len, frame });
    drop(held);
    mem::watch_with(virt, len, "a resolved image the game reads", Some(fill_from_renderer));
}

/// The first read of a held destination (`kernel::mem::WatchReader`): puts the
/// resolved image in its memory.
fn fill_from_renderer(start: u32, size: u32) {
    let Some(h) = ({
        let mut held = HELD.lock().unwrap();
        held.iter().position(|h| h.virt >= start && h.virt < start + size).map(|i| held.remove(i))
    }) else {
        return;
    };
    // A read by the port's own threads (the renderer's texture hashing), not
    // the game: the renderer has this image already, and asking it for a
    // readback from here waits on itself (a 3 s freeze, user86). Never held
    // again.
    if kernel::thread::try_current().is_none() {
        NEVER_HOLD.lock().unwrap().push(h.virt);
        kernel::diag::log(&format!("resolve: 0x{:08X} is read by the renderer itself; not held for readers any more", h.virt));
        return;
    }
    let t = std::time::Instant::now();
    let Some(d) = crate::texture::Fetch::decode(h.fetch) else { return };
    {
        // Read a third time within ten seconds: a scene target something reads
        // all the time, not a picture. Filled this once more, then never held.
        let now = kernel::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed);
        let mut recent = RECENT_FILLS.lock().unwrap();
        recent.retain(|&(_, f)| now.saturating_sub(f) < 600);
        recent.push((h.virt, now));
        if recent.iter().filter(|(v, _)| *v == h.virt).count() >= 3 {
            NEVER_HOLD.lock().unwrap().push(h.virt);
            kernel::diag::log(&format!("resolve: 0x{:08X} is read every few frames; not held for readers any more", h.virt));
        }
    }
    let filled = crate::ir::request_readback(h.phys, h.frame).and_then(|(w, hgt, rgba)| guest_bytes(&d, h.len as usize, w, hgt, &rgba));
    match filled {
        Ok(bytes) => {
            mem::write(h.virt, &bytes);
            crate::ir::resolved_memory_written(h.phys, h.virt, h.len);
            kernel::diag::log(&format!(
                "resolve read back: the {}x{} image at 0x{:08X} is in guest memory for its reader ({} ms)",
                d.width,
                d.height,
                h.virt,
                t.elapsed().as_millis()
            ));
        }
        Err(why) => kernel::diag::log(&format!(
            "resolve read back FAILED: the game is reading the {}x{} image resolved at 0x{:08X} (physical 0x{:08X}, fetch {:08X?}): {why}; \
             the reader gets what the memory held ({} ms)",
            d.width,
            d.height,
            h.virt,
            h.phys,
            h.fetch,
            t.elapsed().as_millis()
        )),
    }
}

/// An RGBA8 image as the bytes of texture `d` (tiled, in its byte order and
/// channel order): the inverse of `texture::to_rgba_from`, found from that
/// function itself. For an 8-bit format each output byte is a copy of one
/// texture byte, so converting textures whose bytes spell out their own
/// offsets shows which. The result is checked by converting it back.
fn guest_bytes(d: &crate::texture::Fetch, len: usize, w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if (w, h) != (d.width, d.height) || rgba.len() != (w * h * 4) as usize {
        return Err(format!("the renderer's image is {w}x{h} ({} bytes), the texture {}x{}", rgba.len(), d.width, d.height));
    }
    if len >= 1 << 24 {
        return Err(format!("the texture's memory is {len:#X} bytes, more than the layout probe covers"));
    }
    let forward = |f: &dyn Fn(usize) -> u8| {
        crate::texture::to_rgba_from(d, &(0..len).map(f).collect::<Vec<u8>>()).ok_or_else(|| format!("texture format {} has no conversion", d.format))
    };
    let (zeros, ones) = (forward(&|_| 0)?, forward(&|_| 0xFF)?);
    let (b0, b1, b2) = (forward(&|i| i as u8)?, forward(&|i| (i >> 8) as u8)?, forward(&|i| (i >> 16) as u8)?);
    if zeros.len() != rgba.len() {
        return Err(format!("the texture converts to {} bytes, the image has {}", zeros.len(), rgba.len()));
    }
    let mut out = vec![0u8; len];
    for j in 0..rgba.len() {
        // A byte that follows the texture (not a constant the fetch supplies).
        if zeros[j] == 0 && ones[j] == 0xFF {
            let source = b0[j] as usize | (b1[j] as usize) << 8 | (b2[j] as usize) << 16;
            if source < len {
                out[source] = rgba[j];
            }
        }
    }
    let back = crate::texture::to_rgba_from(d, &out).ok_or("the texture's bytes do not convert back")?;
    let wrong = (0..rgba.len()).filter(|&j| zeros[j] == 0 && ones[j] == 0xFF && back[j] != rgba[j]).count();
    if wrong != 0 {
        return Err(format!("texture format {} is not a plain byte layout: {wrong} of {} bytes differ when converted back", d.format, rgba.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture::Fetch;

    /// The superstar picture's texture: 736x1280, 8-bit RGBA, tiled, bytes
    /// swapped in fours, read as A8R8G8B8 (R, G, B, A from Z, Y, X, W).
    fn picture() -> Fetch {
        Fetch {
            tiled: true,
            pitch: 736,
            format: 6,
            endian: 2,
            base: 0x1E6C_3000,
            width: 736,
            height: 1280,
            dimension: 1,
            swizzle: 2 | 1 << 3 | 0 << 6 | 3 << 9,
            signs: [0; 4],
            exp_adjust: 0,
            integer: false,
            mip_max: 0,
            packed_mips: false,
            mip_address: 0,
        }
    }

    #[test]
    fn an_image_becomes_the_texture_bytes_that_read_as_it() {
        let d = picture();
        let len = 736 * 1280 * 4;
        // Every pixel different from its neighbours in every channel.
        let rgba: Vec<u8> = (0..len as u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let bytes = guest_bytes(&d, len, 736, 1280, &rgba).expect("an 8-bit RGBA texture");
        assert_eq!(bytes.len(), len);
        assert_eq!(crate::texture::to_rgba_from(&d, &bytes).unwrap(), rgba);
        // A swizzle that supplies a constant: those bytes of the image are not stored.
        let opaque = Fetch { swizzle: 2 | 1 << 3 | 0 << 6 | 5 << 9, ..d };
        let bytes = guest_bytes(&opaque, len, 736, 1280, &rgba).expect("alpha supplied by the fetch");
        let back = crate::texture::to_rgba_from(&opaque, &bytes).unwrap();
        assert!(back.chunks_exact(4).zip(rgba.chunks_exact(4)).all(|(b, r)| b[..3] == r[..3] && b[3] == 255));
    }

    #[test]
    fn a_wrong_size_is_refused_with_the_sizes() {
        let why = guest_bytes(&picture(), 736 * 1280 * 4, 1280, 720, &[0; 1280 * 720 * 4]).unwrap_err();
        assert!(why.contains("1280x720") && why.contains("736x1280"), "{why}");
    }
}

/// `WWE14_WATCH_RESOLVE` (diagnostics), for finding where the game reads back
/// an image it rendered, such as a created superstar's picture. Every texture
/// that becomes a resolve destination for the first time is logged, with its
/// size and the caller. The guest memory of some is then watched, so the first
/// code to read or write it afterwards is reported (`kernel::mem::watch`):
/// - `WxH`: destinations of exactly that size;
/// - `all`: destinations taller than the screen, which no scene pass has;
/// - `every`: every destination, from the first time it is seen (400 at most).
/// F6 in the window: trace resolves (`WWE14_TRACE_RESOLVES`) until this frame.
pub static TRACE_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Traces resolves for the next `frames` frames.
pub fn trace_resolves_for(frames: u64) {
    let now = kernel::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed);
    TRACE_UNTIL.store(now + frames, std::sync::atomic::Ordering::Relaxed);
    kernel::diag::log(&format!("resolve trace: on for {frames} frames from frame {now} (F6)"));
}

fn watch_destination(c: &Ctx, dest: u32, flags: u32, rect: u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static MODE: std::sync::OnceLock<Option<Option<(u32, u32)>>> = std::sync::OnceLock::new();
    static SEEN: std::sync::Mutex<Vec<(u32, u32, u32, u32)>> = std::sync::Mutex::new(Vec::new());
    static WATCHED: AtomicU32 = AtomicU32::new(0);
    // `WWE14_TRACE_RESOLVES`: every resolve into a destination that isn't
    // screen-sized (screen-sized ones included since user84: an arena
    // thumbnail may be a screen capture) (3,000 lines at most), with the frame and caller; for
    // finding the picture a screen takes, reused destinations included.
    {
        static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static LINES: AtomicU32 = AtomicU32::new(0);
        // Each destination once per F6 window: scene passes repeat every frame.
        static SEEN: std::sync::Mutex<Vec<(u64, u32)>> = std::sync::Mutex::new(Vec::new());
        if (*TRACE.get_or_init(|| std::env::var_os("WWE14_TRACE_RESOLVES").is_some())
            || kernel::video::PRESENTED.load(Ordering::Relaxed) < TRACE_UNTIL.load(Ordering::Relaxed))
            && dest != 0
            && let Some(d) = crate::texture::Fetch::decode(core::array::from_fn(|i| mem::r32(dest + 0x1C + 4 * i as u32)))
            && {
                let key = (TRACE_UNTIL.load(Ordering::Relaxed), d.base);
                let mut seen = SEEN.lock().unwrap();
                !seen.contains(&key) && { seen.push(key); true }
            }
            && LINES.fetch_add(1, Ordering::Relaxed) < 3000
        {
            kernel::diag::log(&format!(
                "resolve trace: {}x{} format {} at 0x{:08X}, flags {flags:#x}, frame {} [from 0x{:08X}; stack {}]",
                d.width,
                d.height,
                d.format,
                d.base,
                kernel::video::PRESENTED.load(Ordering::Relaxed),
                c.lr as u32,
                kernel::diag::guest_stack(c.r[1] as u32)
            ));
        }
    }
    let mode = MODE.get_or_init(|| {
        let v = std::env::var("WWE14_WATCH_RESOLVE").ok()?;
        // `every`: the size (0, 0) stands for all of them.
        Some(if v == "every" { Some((0, 0)) } else { v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))) })
    });
    let Some(size) = *mode else { return };
    if dest == 0 {
        return;
    }
    let Some(d) = crate::texture::Fetch::decode(core::array::from_fn(|i| mem::r32(dest + 0x1C + 4 * i as u32))) else { return };
    let Some((virt, len)) = crate::texture::guest_range(&d) else { return };
    let key = (virt, d.width, d.height, d.format);
    let new = {
        let mut seen = SEEN.lock().unwrap();
        let new = !seen.contains(&key);
        if new {
            seen.push(key);
        }
        new
    };
    let wanted = match size {
        // `every`: each destination once, when it is first seen.
        Some((0, 0)) => new,
        Some(s) => (d.width, d.height) == s,
        None => d.height > 736,
    };
    if new || wanted {
        let r: Option<[u32; 4]> = (rect != 0).then(|| core::array::from_fn(|i| mem::r32(rect + 4 * i as u32)));
        kernel::diag::log(&format!(
            "resolve destination {}x{} format {}: header 0x{dest:08X}, memory 0x{virt:08X}+0x{len:X}, rect {r:?}, flags {flags:#x}, frame {} \
             [from 0x{:08X}; stack {}]",
            d.width,
            d.height,
            d.format,
            kernel::video::PRESENTED.load(Ordering::Relaxed),
            c.lr as u32,
            kernel::diag::guest_stack(c.r[1] as u32)
        ));
    }
    if wanted && WATCHED.fetch_add(1, Ordering::Relaxed) < if size == Some((0, 0)) { 400 } else { 12 } {
        mem::watch(virt, len, "a watched resolve destination");
    }
}
/// Records a resolve. Pointers may be null: whole surface, clear only,
/// origin, no clear colour.
#[allow(clippy::too_many_arguments)]
fn record_resolve(dev: u32, flags: u32, rect: u32, dest: u32, point: u32, level: u32, slice: u32, color: u32, clear_z: f32, clear_stencil: u32) {
    crate::ir::resolve(crate::ir::Resolve {
        flags,
        // D3DRECT: x1, y1, x2, y2.
        rect: (rect != 0).then(|| core::array::from_fn(|i| mem::r32(rect + 4 * i as u32))),
        // A texture header keeps its fetch constant at +0x1C.
        dest: (dest != 0).then(|| core::array::from_fn(|i| mem::r32(dest + 0x1C + 4 * i as u32))),
        dest_point: if point != 0 { [mem::r32(point), mem::r32(point + 4)] } else { [0, 0] },
        level,
        slice,
        clear_color: vector(color),
        clear_z,
        clear_stencil,
        surface: crate::ir::surface_regs(dev),
    });
}

/// A `D3DVECTOR4` (null: zero).
fn vector(p: u32) -> [f32; 4] {
    if p == 0 { [0.0; 4] } else { core::array::from_fn(|i| f32::from_bits(mem::r32(p + 4 * i as u32))) }
}

/// Debug log budget for tiling calls.
static TILING_LOG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `D3DDevice_BeginTiling` (`sub_82CB78E0(device, flags, count, tile_rects,
/// clear_color, f1 clear_z, clear_stencil)`).
///
/// Predicated tiling renders a frame bigger than EDRAM one tile at a time: the
/// console replays the frame's commands once per tile, at that tile's screen
/// offset, and resolves each tile. Render targets here are whole textures, so
/// a tiled frame is one full pass (plan §2.4 item 5) into a surface as wide as
/// the screen extent (`ir::tiled_surface_info`).
///
/// The native version makes the original's CPU-side state changes, which the
/// rest of D3D reads while tiling (`SetViewport` clips to the screen extent
/// instead of the one-tile render target when bit 5 of `+0x2B3C` is set):
/// - `+0x3344` tile count, `+0x3348` the tiles (x1, y1, x2, y2), `+0x3438`
///   their origins rounded down to 32;
/// - `+0x34F4`/`+0x34F8` the screen extent (largest x2, y2), `+0x34FC` flags;
/// - `+0x3500` clear colour, `+0x3510` depth, `+0x3514` stencil;
/// - `+0x332C`..`+0x3340` the bound surfaces, `+0x3328` = 2, `+0x2B3C` bit 5,
///   and the predication state (`+0x3320`, `+0x331C`, `+0x2B34`, `+0x2B38`,
///   `+0x2B3C` bit 0).
///
/// It records the tiles' clear (colour when a colour is given, depth and
/// stencil always) unless `flags` bit 0 is set. Flag bit 1's extra setup
/// (`sub_82CB7650`) builds packets only. Like the original it ends with the
/// whole-target viewport, which games read back.
pub extern "C-unwind" fn begin_tiling(c: &mut Ctx, _: *mut u8) {
    let (dev, flags, count, rects, color) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    let (z, stencil) = (c.f[1] as f32, arg(c, 6));
    if TILING_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 24 {
        let r: Vec<u32> = (0..4 * count.min(8)).map(|i| mem::r32(rects + 4 * i)).collect();
        kernel::diag::log(&format!(
            "BeginTiling: flags {flags:X}, {count} tiles {r:?}, clear colour {:?}, z {z}, +0x2B3F {:02X} [from 0x{:08X}]",
            (color != 0).then(|| vector(color)),
            mem::r8(dev + 0x2B3F),
            c.lr as u32
        ));
    }
    mem::w32(dev + 0x3344, count);
    let (mut width, mut height) = (0u32, 0u32);
    for i in 0..count {
        let [x1, y1, x2, y2]: [u32; 4] = core::array::from_fn(|k| mem::r32(rects + 16 * i + 4 * k as u32));
        for (k, v) in [x1, y1, x2, y2].into_iter().enumerate() {
            mem::w32(dev + 0x3348 + 16 * i + 4 * k as u32, v);
        }
        mem::w32(dev + 0x3438 + 8 * i, x1 & !0x1F);
        mem::w32(dev + 0x343C + 8 * i, y1 & !0x1F);
        width = width.max(x2);
        height = height.max(y2);
    }
    mem::w32(dev + 0x34F8, height);
    mem::w32(dev + 0x34F4, width);
    mem::w32(dev + 0x34FC, flags);
    let clear_color = if color != 0 { mem::read(color, 16) } else { vec![0; 16] };
    mem::write(dev + 0x3500, &clear_color);
    mem::w32(dev + 0x3510, z.to_bits());
    mem::w32(dev + 0x3514, stencil);

    // The bound surfaces, which SetViewport compares with the current ones.
    let first = mem::r32(dev + 0x3210);
    mem::w32(dev + 0x332C, if first != 0 { first } else { mem::r32(dev + 0x3220) });
    for k in 0..5 {
        mem::w32(dev + 0x3330 + 4 * k, mem::r32(dev + 0x3210 + 4 * k));
    }
    mem::w32(dev + 0x3328, 2);
    let mode = mem::r8(dev + 0x2B3C) | 0x20;
    mem::w8(dev + 0x2B3C, mode);
    mem::w32(dev + 0x3324, 0);
    reset_predication(dev, mode);

    if flags & 1 == 0 {
        crate::ir::clear(crate::ir::Clear {
            // Colour target 0 when a colour is given, depth and stencil always.
            flags: if color != 0 { 0x31 } else { 0x30 },
            rects: Vec::new(),
            color: vector(color),
            z,
            stencil,
            surface: crate::ir::surface_regs(dev),
        });
    }

    // The original ends by setting D3D's whole-target viewport, which
    // `SetViewport` clips to the screen extent while tiling, then rewrites the
    // viewport and scissor registers and marks all state dirty
    // (`sub_82CAB868(device, 1)`; with 1 it writes no packets). Games read the
    // viewport back after BeginTiling: WWE 2K14 does `GetViewport`, changes the
    // depth range and sets it again (`sub_82C224A0`). Without this it got the
    // one-tile render target's 320×736 and every tiled scene was squeezed into
    // the first 320 pixels.
    kernel::thread::call(c, SET_VIEWPORT, &[dev as u64, WHOLE_TARGET_VIEWPORT as u64]);
    kernel::thread::call(c, 0x82CA_B868, &[dev as u64, 1]);
}

/// `sub_82CABBC8(device, viewport)`: D3D's `SetViewport` (`D3DVIEWPORT9`).
const SET_VIEWPORT: u32 = 0x82CA_BBC8;
/// The executable's `{ 0, 0, 65535, 65535, 0.0, 1.0 }`: the whole target.
const WHOLE_TARGET_VIEWPORT: u32 = 0x8208_2430;

/// The predication state `BeginTiling` and `EndTiling` leave (both originals
/// end the same way): `+0x2B3C` bit 0 set unless bit 3 or 2 is set or
/// `+0x3113` is non-zero, and then only when bit 4 or bit 5 is set.
fn reset_predication(dev: u32, mode: u8) {
    let on = if mode & 0x0C != 0 || mem::r8(dev + 0x3113) != 0 { false } else { mode & 0x30 != 0 };
    mem::w32(dev + 0x3320, 0);
    mem::w32(dev + 0x331C, u32::MAX);
    mem::w32(dev + 0x2B34, 0);
    mem::w32(dev + 0x2B38, 0);
    mem::w8(dev + 0x2B3C, (mode & !1) | on as u8);
}

/// `D3DDevice_EndTiling` (`sub_82CB7C50(device, resolve_flags, resolve_rects,
/// dest_texture, clear_color, f1 clear_z, clear_stencil, parameters)`).
///
/// The original resolves every tile into the destination (through
/// `D3DDevice_Resolve`) and ends predicated replay. Native: one resolve of the
/// whole extent (or the rectangle given), clearing the colour when a clear
/// colour is given, made while the surface still covers the extent; then the
/// original's CPU-side state changes: `+0x2B3F` bits 4-5 and `+0x2B3C` bit 5
/// cleared, `+0x332C` and `+0x3324` zeroed, the predication state reset, and
/// the tile count back to 0.
pub extern "C-unwind" fn end_tiling(c: &mut Ctx, _: *mut u8) {
    let (dev, flags, rect, dest, color) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    if TILING_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 24 {
        kernel::diag::log(&format!("EndTiling: flags {flags:X}, rect {}, dest {dest:08X}, clear colour {} [from 0x{:08X}]", if rect != 0 { format!("{:?}", (0..4).map(|i| mem::r32(rect + 4 * i)).collect::<Vec<_>>()) } else { "none".into() }, color != 0, c.lr as u32));
    }
    let flags = flags | if color != 0 { crate::ir::RESOLVE_CLEAR_TARGET } else { 0 };
    record_resolve(dev, flags, rect, dest, 0, 0, 0, color, c.f[1] as f32, arg(c, 6));
    hold_for_reader(dest);
    watch_destination(c, dest, flags, rect);
    mem::w8(dev + 0x2B3F, mem::r8(dev + 0x2B3F) & 0xCF);
    let mode = mem::r8(dev + 0x2B3C) & 0xDF;
    mem::w32(dev + 0x332C, 0);
    mem::w32(dev + 0x3324, 0);
    mem::w8(dev + 0x2B3C, mode);
    reset_predication(dev, mode);
    mem::w32(dev + 0x3344, 0);
    // As the original ends: the whole-target viewport, now clipped to the
    // bound render target (tiling is over).
    kernel::thread::call(c, SET_VIEWPORT, &[dev as u64, WHOLE_TARGET_VIEWPORT as u64]);
}
