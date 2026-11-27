//! The audio render-driver API (`XAudioRegisterRenderDriverClient`,
//! `XAudioSubmitRenderDriverFrame`).
//!
//! The game's XAudio2 mixes on the CPU (recompiled code) and hands the kernel
//! one frame of 256 samples × 6 channels (0x1800 bytes) (big-endian float) at a time. The
//! kernel calls the registered client back once per frame period, 256 / 48 kHz
//! = 5.33 ms, and gives each frame, down-mixed to stereo, to the host's sink
//! (`set_sink`). With an output device (`pace_by`), the callback follows the
//! device's clock: a frame is asked for whenever the device's backlog runs
//! low, as the console's audio hardware asks for one when it needs it. The
//! XMA hardware decoder is not part of this API and is never modelled.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

type Sink = Box<dyn Fn(&[f32]) + Send + Sync>;
type Backlog = Box<dyn Fn() -> usize + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();
static BACKLOG: OnceLock<Backlog> = OnceLock::new();

/// Where mixed frames go: interleaved stereo f32 at 48 kHz, 256 sample pairs
/// per call. Set once, by the host, before the game starts its audio.
pub fn set_sink(sink: impl Fn(&[f32]) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(sink));
}

/// Paces the render driver by an output device: `backlog` returns the stereo
/// samples queued and not yet played. Frames are asked for while it is under
/// `TARGET_BACKLOG`.
pub fn pace_by(backlog: impl Fn() -> usize + Send + Sync + 'static) {
    let _ = BACKLOG.set(Box::new(backlog));
}

/// 60 ms of stereo at 48 kHz: enough to ride out a busy host thread.
const TARGET_BACKLOG: usize = 2 * 48_000 * 60 / 1000;

/// The 360's channel order is front left, front right, centre, LFE, surround
/// left, surround right, each channel a block of 256 samples. Stereo keeps
/// the fronts and folds centre and surrounds in at -3 dB (LFE dropped).
fn downmix(samples: u32, out: &mut [f32; 2 * SAMPLES_PER_FRAME as usize]) {
    let ch = |c: u32, i: u32| f32::from_bits(mem::r32(samples + 4 * (c * SAMPLES_PER_FRAME + i)));
    const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for i in 0..SAMPLES_PER_FRAME {
        let (c, sl, sr) = (ch(2, i) * K, ch(4, i) * K, ch(5, i) * K);
        out[2 * i as usize] = (ch(0, i) + c + sl).clamp(-1.0, 1.0);
        out[2 * i as usize + 1] = (ch(1, i) + c + sr).clamp(-1.0, 1.0);
    }
}

use ppc_rt::Ctx;

use crate::mem;
use crate::thread::{self, Entry};

pub const SAMPLES_PER_FRAME: u32 = 256;
pub const SAMPLE_RATE: u32 = 48_000;

struct Client {
    stop: Arc<AtomicBool>,
}

static CLIENTS: Mutex<Vec<Option<Client>>> = Mutex::new(Vec::new());
pub static FRAMES_SUBMITTED: AtomicU64 = AtomicU64::new(0);

/// Registers `{ callback, argument }`; returns the driver handle.
pub fn register(callback: u32, argument: u32) -> u32 {
    // The callback receives a pointer to a cell holding its argument.
    let cell = mem::kalloc(4, 4);
    mem::w32(cell, argument);
    let stop = Arc::new(AtomicBool::new(false));
    let index = {
        let mut clients = CLIENTS.lock().unwrap();
        let i = clients.iter().position(Option::is_none).unwrap_or(clients.len());
        if i == clients.len() {
            clients.push(None);
        }
        clients[i] = Some(Client { stop: stop.clone() });
        i as u32
    };
    crate::music::start_background();
    let t = thread::spawn(
        Entry::Native(Box::new(move |c: &mut Ctx| render_loop(c, callback, cell, &stop))),
        0x1_0000,
        1 << 5,
        false,
        format!("audio render client {index}"),
    );
    // On the console the audio hardware's interrupt runs the callback at once;
    // here its thread is time-critical, so loading threads can't starve it.
    t.set_priority(15);
    0x4155_0000 | index
}

pub fn unregister(driver: u32) -> bool {
    let i = (driver & 0xFFFF) as usize;
    match CLIENTS.lock().unwrap().get_mut(i).and_then(Option::take) {
        Some(c) => {
            c.stop.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// `XAudioSubmitRenderDriverFrame`: accepts one mixed frame.
pub fn submit(_driver: u32, samples: u32) {
    let n = FRAMES_SUBMITTED.fetch_add(1, Ordering::Relaxed) + 1;
    watch_gaps(samples);
    if let Some(sink) = SINK.get() {
        let mut stereo = [0f32; 2 * SAMPLES_PER_FRAME as usize];
        downmix(samples, &mut stereo);
        // Held quiet until the save loads (`music::held`): the game's own mix
        // is dropped; the user's music, added next, still plays.
        if crate::music::held() {
            stereo.fill(0.0);
        }
        // The system music player plays over the title's mix, as on the console.
        crate::music::mix(&mut stereo);
        sink(&stereo);
    }
    if crate::diag::TRACE_AUDIO.load(Ordering::Relaxed) {
        // 256 samples x 6 channels, big-endian f32, channel-major blocks of 256.
        let mut peak = 0f32;
        for i in 0..SAMPLES_PER_FRAME * 6 {
            peak = peak.max(f32::from_bits(mem::r32(samples + 4 * i)).abs());
        }
        let mut p = PEAK.lock().unwrap();
        *p = p.max(peak);
        if n % 188 == 0 {
            crate::diag::log(&format!("audio: {n} frames submitted, peak over the last second {:.4}", *p));
            *p = 0.0;
        }
    }
}

static PEAK: Mutex<f32> = Mutex::new(0.0);

/// Gaps in the game's own mix: a stretch of frames that are exactly zero on
/// every channel, between frames with sound. Music and video soundtracks are
/// never digital silence for long, so a stretch of 20 ms or more in the
/// middle of one means a voice ran out of data inside the game's XAudio2
/// (user40: the 30 Years of WrestleMania intro's sound cut in and out with
/// no underrun at the device). Logged with the game frame, 300 lines at most.
fn watch_gaps(samples: u32) {
    static STATE: Mutex<(u32, bool, u32)> = Mutex::new((0, false, 0)); // (silent frames, heard sound, lines)
    let silent = (0..SAMPLES_PER_FRAME * 6).all(|i| mem::r32(samples + 4 * i) & 0x7FFF_FFFF == 0);
    let mut s = STATE.lock().unwrap();
    if silent {
        s.0 += 1;
        return;
    }
    let (run, heard) = (s.0, s.1);
    s.0 = 0;
    s.1 = true;
    if heard && run >= 4 && s.2 < 300 {
        s.2 += 1;
        crate::diag::log(&format!(
            "audio mix: {:.0} ms of silence ended at game frame {}",
            run as f64 * SAMPLES_PER_FRAME as f64 * 1000.0 / SAMPLE_RATE as f64,
            crate::video::PRESENTED.load(Ordering::Relaxed)
        ));
    }
}

fn render_loop(c: &mut Ctx, callback: u32, cell: u32, stop: &AtomicBool) {
    let period = Duration::from_nanos(1_000_000_000 * SAMPLES_PER_FRAME as u64 / SAMPLE_RATE as u64);
    let mut next = Instant::now() + period;
    while !stop.load(Ordering::Relaxed) {
        if let Some(backlog) = BACKLOG.get() {
            // The device's clock: wait while enough is queued, but no longer
            // than a device that has stopped draining would make the game wait.
            let since = Instant::now();
            while backlog() >= TARGET_BACKLOG && !stop.load(Ordering::Relaxed) && since.elapsed() < Duration::from_millis(100) {
                std::thread::sleep(Duration::from_millis(1));
            }
            thread::call(c, callback, &[cell as u64]);
            continue;
        }
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(100) {
            // Fell far behind (a debugger, a long stall): don't try to catch up.
            next = now;
        }
        next += period;
        thread::call(c, callback, &[cell as u64]);
    }
}
