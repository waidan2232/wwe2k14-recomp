//! The host (plan §2.6): boots the guest on its own threads and runs the
//! renderer on the frames the native D3D layer hands over at each `Swap`.
//!
//! Modes:
//! - `--headless DIR`: no window. Frames are rendered offscreen by the same
//!   renderer; every `--capture-every N`th frame (default 30) is appended to
//!   `DIR\frames.gif` at half size, and every tenth of those is also written
//!   to `DIR` as a PNG. With `--wav`, the game's audio goes to `DIR\audio.wav`.
//!   With `--video FROM`, every frame from number `FROM` on goes to
//!   `DIR\video.mp4` (H.264 through ffmpeg: `WWE14_FFMPEG`, else `ffmpeg` on
//!   the path). The game's logic steps once a frame, so the 60 fps file plays
//!   at the game's real speed however slowly the run went; with `--wav` too,
//!   the audio follows the frames (800 samples each) from `FROM` on, to match.
//! - otherwise the Bevy window (`window.rs`, `GpuBridgePlugin`).
//!
//! Translated shaders are read from `WWE14_SHADERS` (directories separated by
//! `;`), by default `private\pac-wgsl` and `private\xex-wgsl` next to the game.

mod audio;
mod portable;
mod window;

// The process uses Rust's system allocator. mimalloc (a C library) was used
// before for its per-thread heaps; the user asked for no C in the build
// (2026-11-25).

pub use window::GpuBridgePlugin;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

/// Drives a future to completion on this thread (wgpu's device requests).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

fn shader_dirs(xex: &Path) -> Vec<PathBuf> {
    if let Ok(v) = std::env::var("WWE14_SHADERS") {
        return v.split(';').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    }
    let private = xex.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    vec![private.join("pac-wgsl"), private.join("xex-wgsl")]
}

/// Starts ffmpeg encoding raw RGBA frames of `w`×`h` at 60 fps into `path`
/// (fragmented MP4, so the file stays playable when the run is killed).
/// Returns the pipe to write frames into.
fn start_video(path: &Path, w: u32, h: u32) -> Option<std::process::ChildStdin> {
    use std::process::{Command, Stdio};
    let ffmpeg = std::env::var("WWE14_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let child = Command::new(&ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{w}x{h}"), "-r", "60", "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "24", "-pix_fmt", "yuv420p", "-movflags", "+frag_keyframe+empty_moov"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn();
    match child {
        Ok(mut c) => {
            eprintln!("host: recording video to {}", path.display());
            c.stdin.take()
        }
        Err(e) => {
            eprintln!("host: no video ({ffmpeg}: {e})");
            None
        }
    }
}

/// The game binary's entry point. Returns only by exiting the process.
pub fn main(mut p: kernel::Program) -> ! {
    portable::setup(&mut p);
    let mut headless: Option<PathBuf> = None;
    let mut every = 30u64;
    let mut wav = false;
    let mut video: Option<u64> = None;
    let mut kernel_args = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--headless" => headless = args.next().map(PathBuf::from),
            "--capture-every" => every = args.next().and_then(|s| s.parse().ok()).unwrap_or(30).max(1),
            "--wav" => wav = true,
            "--video" => video = Some(args.next().and_then(|s| s.parse().ok()).unwrap_or(0)),
            _ => kernel_args.push(a),
        }
    }
    let dirs = shader_dirs(Path::new(p.default_xex));
    let Some(out) = headless else { window::run(p, kernel_args, dirs) };
    std::fs::create_dir_all(&out).expect("create the capture directory");
    if wav {
        audio::start_wav(&out.join("audio.wav"), video);
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .expect("no GPU adapter");
    eprintln!("host: rendering offscreen on {}", adapter.get_info().name);
    // Like Bevy's window renderer, ask for what the adapter offers that the
    // renderer uses: filtering 32-bit float textures (depth resolves).
    // The pipeline cache too, so compiled pipelines carry over between runs.
    // And its limits, as Bevy asks: the translated shaders pass 17 inter-stage
    // variables (the 16 interpolators and the draw number).
    let required_features = adapter.features() & (wgpu::Features::FLOAT32_FILTERABLE | wgpu::Features::PIPELINE_CACHE | wgpu::Features::TEXTURE_COMPRESSION_BC);
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features, required_limits: adapter.limits(), ..Default::default() }))
        .expect("no GPU device");

    let frames = gpu::ir::subscribe();
    let rendered = Arc::new(AtomicU64::new(0));
    let r2 = rendered.clone();
    std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            kernel::profile::register_host_thread("render");
            let mut r = render::Renderer::new(device, queue, dirs);
            let (ow, oh) = render::output_size();
            let (gw, gh) = ((ow / 2) as usize, (oh / 2) as usize);
            let mut gif = gpu::encode::gif_start(&out.join("frames.gif"), gw, gh).ok();
            let mut captured = 0u64;
            let mut encoder = video.and_then(|_| start_video(&out.join("video.mp4"), ow, oh));
            let mut done = 0u64;
            loop {
                // While no frame arrives, images the game reads back are still
                // served: its frames stop while the reader waits.
                let frame = match frames.recv_timeout(std::time::Duration::from_millis(4)) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        r.serve_readbacks(done);
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                r.render(&frame);
                done = frame.number;
                r.serve_readbacks(done);
                r2.fetch_add(1, Ordering::Relaxed);
                if let (Some(from), Some(stdin)) = (video, encoder.as_mut())
                    && frame.number >= from
                {
                    use std::io::Write;
                    if stdin.write_all(&r.read_target()).is_err() {
                        eprintln!("host: the video encoder stopped");
                        encoder = None;
                    }
                }
                if frame.number % every == 0 {
                    let px = r.read_target();
                    captured += 1;
                    // Full-size stills less often: they are stored uncompressed (2.7 MB). Counted
                    // by captures, since frames the renderer can't keep up with are dropped.
                    if captured % 10 == 0 {
                        let name = format!("frame{:06}.png", frame.number);
                        let _ = gpu::encode::write_png(&out.join(name), &px, ow as usize, oh as usize);
                        // WWE14_DUMP_TARGETS=N: from frame N on, also every resolved texture and
                        // render target.
                        let dump_from = std::env::var("WWE14_DUMP_TARGETS").ok().and_then(|v| v.parse::<u64>().ok());
                        if dump_from.is_some_and(|n| frame.number >= n) {
                            for (what, w, h, img) in r.debug_images() {
                                let name = format!("frame{:06}_{what}.png", frame.number);
                                let _ = gpu::encode::write_png(&out.join(name), &img, w as usize, h as usize);
                            }
                        }
                    }
                    if let Some(g) = gif.as_mut() {
                        let half = gpu::encode::downscale_half(&px, ow as usize, oh as usize);
                        let _ = gpu::encode::gif_frame(g, &half, gw, gh, (every * 100 / 60) as u16);
                    }
                    eprintln!(
                        "host: frame {} captured ({} draws; {} drawn, {} skipped so far)",
                        frame.number,
                        frame.draws().count(),
                        r.draws_drawn,
                        r.draws_skipped
                    );
                }
                render::drop_later(frame);
            }
        })
        .expect("start the render thread");

    kernel::boot(p, kernel_args).wait()
}
