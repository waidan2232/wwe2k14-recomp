//! Game audio to the host (plan §2.5): the kernel's mixed frames
//! (`kernel::audio`, interleaved stereo f32 at 48 kHz) to the default output
//! device through cpal, or, headless, to a WAV file.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const RATE: u32 = 48_000;

/// Plays the game's audio on the default output device. The stream must be
/// kept alive. `None` (logged) when there is no usable device; the game then
/// runs with its audio paced by the wall clock, as headless.
pub fn start_output() -> Option<cpal::Stream> {
    let fail = |what: &str| {
        eprintln!("host: no audio output ({what})");
        None
    };
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else { return fail("no output device") };
    let Ok(supported) = device.default_output_config() else { return fail("no output configuration") };
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return fail(&format!("output format {:?}, not f32", supported.sample_format()));
    }
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let config: cpal::StreamConfig = supported.into();

    // Mixed stereo waiting to be played, at most a quarter second (older is
    // dropped: the game ran ahead of the device).
    let ring = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(2 * RATE as usize)));
    let max = 2 * RATE as usize / 4;
    let stats = Arc::new(OutputStats::default());
    let r = ring.clone();
    let st = stats.clone();
    kernel::audio::set_sink(move |s| {
        let mut q = r.lock().unwrap();
        q.extend(s);
        let over = q.len().saturating_sub(max) & !1;
        q.drain(..over);
        st.dropped.fetch_add(over as u64 / 2, Relaxed);
    });
    let r = ring.clone();
    kernel::audio::pace_by(move || r.lock().unwrap().len());

    // The device's rate: linear interpolation over the 48 kHz stream.
    let step = RATE as f64 / rate as f64;
    let mut pos = 0.0f64;
    let (mut started, mut starved) = (false, false);
    let r = ring.clone();
    let st = stats.clone();
    let stream = device.build_output_stream(
        config,
        move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut q = r.lock().unwrap();
            let depth = q.len() as u64 / 2;
            st.depth_min.fetch_min(depth, Relaxed);
            st.depth_max.fetch_max(depth, Relaxed);
            st.played.fetch_add((out.len() / channels.max(1)) as u64, Relaxed);
            for frame in out.chunks_mut(channels) {
                let i = pos as usize;
                let (l, rr) = if 2 * i + 3 < q.len() {
                    let t = (pos - i as f64) as f32;
                    let lerp = |a: f32, b: f32| a + (b - a) * t;
                    pos += step;
                    started = true;
                    starved = false;
                    (lerp(q[2 * i], q[2 * i + 2]), lerp(q[2 * i + 1], q[2 * i + 3]))
                } else {
                    // Nothing mixed yet: silence. Once the game's audio has
                    // started, that is an underrun.
                    if started {
                        st.underrun.fetch_add(1, Relaxed);
                        if !starved {
                            st.underruns.fetch_add(1, Relaxed);
                        }
                        starved = true;
                    }
                    (0.0, 0.0)
                };
                match frame {
                    [m] => *m = 0.5 * (l + rr),
                    [a, b, rest @ ..] => {
                        *a = l;
                        *b = rr;
                        rest.iter_mut().for_each(|x| *x = 0.0);
                    }
                    [] => {}
                }
            }
            let whole = pos as usize;
            let n = (2 * whole).min(q.len());
            q.drain(..n);
            pos -= whole as f64;
        },
        |e| eprintln!("host: audio output error: {e}"),
        None,
    );
    let stream = match stream {
        Ok(s) => s,
        Err(e) => return fail(&e.to_string()),
    };
    if let Err(e) = stream.play() {
        return fail(&e.to_string());
    }
    eprintln!("host: audio to {} ({channels} channels, {rate} Hz)", device.description().map(|d| d.name().to_string()).unwrap_or_default());
    let _ = std::thread::Builder::new().name("audio report".into()).spawn(move || stats.report(rate));
    Some(stream)
}

/// What the output device got, for the Phase 5 gate (no underruns, audio in
/// sync): counted in the device callback, logged by a reporter thread.
#[derive(Default)]
struct OutputStats {
    /// Device frames played.
    played: AtomicU64,
    /// Device frames of silence written after the game's audio started, and
    /// the stretches of them.
    underrun: AtomicU64,
    underruns: AtomicU64,
    /// 48 kHz frames dropped because the queue was full (the game ran ahead).
    dropped: AtomicU64,
    /// The queue's depth at callbacks since the last report, in 48 kHz frames:
    /// how far the audio heard lags the game's mixing.
    depth_min: AtomicU64,
    depth_max: AtomicU64,
}

impl OutputStats {
    /// Every 10 s when anything went wrong, and every minute anyway: underruns
    /// and drops so far, and the queue's depth range (the audio delay).
    fn report(&self, rate: u32) {
        self.depth_min.store(u64::MAX, Relaxed);
        let (mut last, mut quiet) = ((0, 0), 0);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let now = (self.underrun.load(Relaxed), self.dropped.load(Relaxed));
            quiet += 1;
            if now == last && quiet < 6 {
                continue;
            }
            let (lo, hi) = (self.depth_min.swap(u64::MAX, Relaxed), self.depth_max.swap(0, Relaxed));
            let ms = |frames: u64, r: u32| frames as f64 * 1000.0 / r as f64;
            eprintln!(
                "host: audio {:.0} s played: underruns {} ({:.0} ms of silence), {:.0} ms dropped (game ahead), queue {:.0}-{:.0} ms",
                self.played.load(Relaxed) as f64 / rate as f64,
                self.underruns.load(Relaxed),
                ms(now.0, rate),
                ms(now.1, RATE),
                if lo == u64::MAX { 0.0 } else { ms(lo, RATE) },
                ms(hi, RATE),
            );
            (last, quiet) = (now, 0);
        }
    }
}

/// Writes the game's audio to `path` as 16-bit stereo 48 kHz WAV, its header
/// kept valid as it grows (the run may be killed at any point). A thread
/// stands in for a device: it takes 48,000 sample pairs a second from the
/// same kind of queue `start_output` plays from, so the game is paced as it
/// would be by a real device, and logs underruns (silence written because
/// the game hadn't mixed in time) every ten seconds.
///
/// With `frames_from` (a video is being recorded), the device's clock is the
/// game's frames instead of the wall clock: 800 sample pairs per presented
/// frame, written from that frame on, so the audio matches the video however
/// fast the run goes.
pub fn start_wav(path: &Path, frames_from: Option<u64>) {
    let Ok(mut f) = std::fs::File::create(path) else {
        eprintln!("host: can't create {}", path.display());
        return;
    };
    let _ = f.write_all(&header(0));
    let ring = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(2 * RATE as usize)));
    let max = 2 * RATE as usize / 4;
    let r = ring.clone();
    kernel::audio::set_sink(move |s| {
        let mut q = r.lock().unwrap();
        q.extend(s);
        let over = q.len().saturating_sub(max) & !1;
        q.drain(..over);
    });
    let r = ring.clone();
    kernel::audio::pace_by(move || r.lock().unwrap().len());
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let (mut played, mut bytes, mut underruns, mut started) = (0u64, 0u32, 0u64, false);
        let mut report = std::time::Instant::now();
        let per_frame = RATE as u64 / 60;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(5));
            let due = match frames_from {
                Some(_) => kernel::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed) * per_frame,
                None => start.elapsed().as_micros() as u64 * RATE as u64 / 1_000_000,
            };
            let want = due.saturating_sub(played) as usize;
            // Before the video's first frame: taken from the game, not written.
            let skip = frames_from.is_some_and(|f| due <= f.saturating_sub(1) * per_frame);
            let mut pcm = Vec::with_capacity(4 * want);
            {
                let mut q = ring.lock().unwrap();
                let have = (q.len() / 2).min(want);
                started |= have > 0;
                if started {
                    underruns += (want - have) as u64;
                }
                for x in q.drain(..2 * have) {
                    pcm.extend(((x * 32767.0) as i16).to_le_bytes());
                }
            }
            pcm.resize(4 * want, 0);
            played += want as u64;
            if skip {
                continue;
            }
            if f.write_all(&pcm).is_err() {
                return;
            }
            bytes += pcm.len() as u32;
            if bytes % (4 * RATE) < pcm.len() as u32 {
                let _ = f.seek(SeekFrom::Start(0)).and_then(|_| f.write_all(&header(bytes))).and_then(|_| f.seek(SeekFrom::End(0)));
            }
            if report.elapsed().as_secs() >= 10 {
                report = std::time::Instant::now();
                eprintln!("host: audio {:.0} s written, {underruns} samples of underrun so far", played as f64 / RATE as f64);
            }
        }
    });
}

fn header(data: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let mut put = |at: usize, b: &[u8]| h[at..at + b.len()].copy_from_slice(b);
    put(0, b"RIFF");
    put(4, &(36 + data).to_le_bytes());
    put(8, b"WAVEfmt ");
    put(16, &16u32.to_le_bytes());
    put(20, &1u16.to_le_bytes()); // PCM
    put(22, &2u16.to_le_bytes());
    put(24, &RATE.to_le_bytes());
    put(28, &(RATE * 4).to_le_bytes());
    put(32, &4u16.to_le_bytes());
    put(34, &16u16.to_le_bytes());
    put(36, b"data");
    put(40, &data.to_le_bytes());
    h
}
