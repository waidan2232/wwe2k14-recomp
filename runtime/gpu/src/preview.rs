//! Debug previews (`WWE14_PREVIEW=<dir>`): a CPU compositor for the game's
//! inline 2D quads, written out as an animated GIF and PNG stills.
//!
//! This is a visualisation for development, not the renderer. It doesn't run
//! the game's shaders: it composites screen-space quads with their base
//! texture and standard alpha blending, recognises Bink's YUV+A planes, and
//! draws untextured quads in a neutral colour. 3D draws don't appear. The real
//! renderer (translated shaders on wgpu) replaces it.

use std::collections::HashMap;
use std::sync::Mutex;

use kernel::{diag, mem};

use crate::encode::{gif_frame, gif_start, write_png};
use crate::texture::{self, Fetch};

pub const W: usize = 1280;
pub const H: usize = 720;

/// One recorded draw.
struct Quad {
    prim: u32,
    stride: u32,
    verts: Vec<f32>,
    textures: [Option<Fetch>; 4],
}

struct State {
    dir: std::path::PathBuf,
    frame: u64,
    draws: Vec<Quad>,
    gif: Option<std::fs::File>,
    gif_frames: u32,
    stills: u32,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Capture every `GIF_EVERY`-th presented frame into the GIF (10 fps at 60 Hz).
const GIF_EVERY: u64 = 6;
const GIF_MAX: u32 = 1200;
const STILL_EVERY: u64 = 300;

fn enabled() -> bool {
    static ON: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let dir = ON.get_or_init(|| std::env::var_os("WWE14_PREVIEW").map(std::path::PathBuf::from));
    if let Some(d) = dir {
        let mut s = STATE.lock().unwrap();
        if s.is_none() {
            let _ = std::fs::create_dir_all(d);
            *s = Some(State { dir: d.clone(), frame: 0, draws: Vec::new(), gif: None, gif_frames: 0, stills: 0 });
        }
        true
    } else {
        false
    }
}

/// Records an inline draw (`EndVertices`): vertices from `scratch`, textures
/// from the device's fetch-constant shadow.
pub fn record_inline(dev: u32, prim: u32, count: u32, stride: u32, scratch: u32) {
    if !enabled() {
        return;
    }
    let mut s = STATE.lock().unwrap();
    let st = s.as_mut().unwrap();
    // Only frames that will be written are worth recording.
    if st.frame % GIF_EVERY != 0 && st.frame % STILL_EVERY != 0 {
        return;
    }
    let n = (count * stride / 4) as usize;
    let verts = (0..n).map(|i| f32::from_bits(mem::r32(scratch + 4 * i as u32))).collect();
    let textures = core::array::from_fn(|i| Fetch::from_device(dev, i as u32));
    st.draws.push(Quad { prim, stride, verts, textures });
}

/// Called at every present.
pub fn present() {
    if !enabled() {
        return;
    }
    let mut s = STATE.lock().unwrap();
    let st = s.as_mut().unwrap();
    let frame = st.frame;
    st.frame += 1;
    let draws = std::mem::take(&mut st.draws);
    let want_gif = frame % GIF_EVERY == 0 && st.gif_frames < GIF_MAX;
    let want_still = frame % STILL_EVERY == 0;
    if !want_gif && !want_still {
        return;
    }
    let canvas = compose(&draws);
    if want_still {
        let path = st.dir.join(format!("frame_{frame:06}.png"));
        if let Err(e) = write_png(&path, &canvas, W, H) {
            diag::log(&format!("preview: {}: {e}", path.display()));
        }
        st.stills += 1;
    }
    if want_gif {
        let small = downscale_half(&canvas);
        if st.gif.is_none() {
            match gif_start(&st.dir.join("preview.gif"), W / 2, H / 2) {
                Ok(f) => st.gif = Some(f),
                Err(e) => diag::log(&format!("preview: gif: {e}")),
            }
        }
        if let Some(f) = st.gif.as_mut() {
            let _ = gif_frame(f, &small, W / 2, H / 2, 10);
            st.gif_frames += 1;
        }
    }
}

fn compose(draws: &[Quad]) -> Vec<u8> {
    let mut canvas = vec![0u8; W * H * 4];
    for px in canvas.chunks_exact_mut(4) {
        px[3] = 255;
    }
    let mut cache: HashMap<(u32, u32, u32, u32), Option<std::sync::Arc<Vec<u8>>>> = HashMap::new();
    let mut tex = |f: &Fetch| -> Option<std::sync::Arc<Vec<u8>>> {
        cache.entry((f.base, f.format, f.width, f.height)).or_insert_with(|| texture::to_rgba(f).map(std::sync::Arc::new)).clone()
    };
    for d in draws {
        let per = (d.stride / 4) as usize;
        if per < 2 || d.verts.len() < per * 3 {
            continue;
        }
        let v: Vec<&[f32]> = d.verts.chunks_exact(per).collect();
        // Quads (list of 4) and 4-vertex strips: the bounding rectangle.
        let quads: Vec<[&[f32]; 4]> = match d.prim {
            13 => v.chunks_exact(4).map(|q| [q[0], q[1], q[2], q[3]]).collect(),
            6 | 5 if v.len() == 4 => vec![[v[0], v[1], v[3], v[2]]],
            _ => continue,
        };
        let bink = matches!(d.textures, [Some(a), Some(b), Some(c), _]
            if a.format == 2 && b.format == 2 && c.format == 2 && b.width * 2 == a.width);
        for q in quads {
            draw_quad(&mut canvas, q, per, d, bink, &mut tex);
        }
    }
    canvas
}

fn draw_quad(
    canvas: &mut [u8],
    q: [&[f32]; 4],
    per: usize,
    d: &Quad,
    bink: bool,
    tex: &mut impl FnMut(&Fetch) -> Option<std::sync::Arc<Vec<u8>>>,
) {
    let xs = q.map(|v| v[0]);
    let ys = q.map(|v| v[1]);
    let (x0, x1) = (xs.iter().cloned().fold(f32::MAX, f32::min), xs.iter().cloned().fold(f32::MIN, f32::max));
    let (y0, y1) = (ys.iter().cloned().fold(f32::MAX, f32::min), ys.iter().cloned().fold(f32::MIN, f32::max));
    if !(x1 > x0 && y1 > y0) || x1 - x0 > 8192.0 || y1 - y0 > 8192.0 {
        return;
    }
    // Texture coordinates: the last two floats of the vertex, mapped from the
    // corner at the rectangle's minimum x/y.
    let uv = |v: &[f32]| (v[per - 2], v[per - 1]);
    let corner = |fx: f32, fy: f32| q.iter().min_by(|a, b| ((a[0] - fx).abs() + (a[1] - fy).abs()).total_cmp(&((b[0] - fx).abs() + (b[1] - fy).abs()))).unwrap();
    let (u0, v0) = uv(corner(x0, y0));
    let (u1, v1) = uv(corner(x1, y1));
    let textured = per >= 5 && d.textures[0].is_some();
    let images: Vec<Option<(Fetch, std::sync::Arc<Vec<u8>>)>> =
        d.textures.iter().map(|t| t.and_then(|f| tex(&f).map(|img| (f, img)))).collect();
    let sample = |i: usize, u: f32, v: f32| -> Option<[u8; 4]> {
        let (f, img) = images[i].as_ref()?;
        let x = ((u.fract() + 1.0).fract() * f.width as f32) as u32 % f.width;
        let y = ((v.fract() + 1.0).fract() * f.height as f32) as u32 % f.height;
        let k = ((y * f.width + x) * 4) as usize;
        Some([img[k], img[k + 1], img[k + 2], img[k + 3]])
    };
    let (ix0, ix1) = (x0.max(0.0) as usize, (x1.min(W as f32)) as usize);
    let (iy0, iy1) = (y0.max(0.0) as usize, (y1.min(H as f32)) as usize);
    for py in iy0..iy1 {
        let fy = (py as f32 + 0.5 - y0) / (y1 - y0);
        for px in ix0..ix1 {
            let fx = (px as f32 + 0.5 - x0) / (x1 - x0);
            let (u, v) = (u0 + (u1 - u0) * fx, v0 + (v1 - v0) * fy);
            let src: [u8; 4] = if bink {
                let (Some(yv), Some(cr), Some(cb)) = (sample(0, u, v), sample(1, u, v), sample(2, u, v)) else { continue };
                let a = sample(3, u, v).map_or(255, |a| a[0]);
                let (y, r, b) = (yv[0] as f32 - 16.0, cr[0] as f32 - 128.0, cb[0] as f32 - 128.0);
                let c = |x: f32| x.clamp(0.0, 255.0) as u8;
                [c(1.164 * y + 1.596 * r), c(1.164 * y - 0.813 * r - 0.391 * b), c(1.164 * y + 2.018 * b), a]
            } else if textured {
                match sample(0, u, v) {
                    Some(p) => p,
                    None => continue,
                }
            } else {
                [96, 96, 96, 160]
            };
            let k = (py * W + px) * 4;
            let a = src[3] as u32;
            for c in 0..3 {
                canvas[k + c] = ((src[c] as u32 * a + canvas[k + c] as u32 * (255 - a)) / 255) as u8;
            }
        }
    }
}

fn downscale_half(src: &[u8]) -> Vec<u8> {
    let (w, h) = (W / 2, H / 2);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            for c in 0..4 {
                let s = |dx: usize, dy: usize| src[((2 * y + dy) * W + 2 * x + dx) * 4 + c] as u32;
                out[(y * w + x) * 4 + c] = ((s(0, 0) + s(1, 0) + s(0, 1) + s(1, 1)) / 4) as u8;
            }
        }
    }
    out
}

