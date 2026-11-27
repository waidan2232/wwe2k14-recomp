//! Debugging: records every frame the renderer draws (the display image after
//! each game frame) to a video while the game runs in its window
//! (`WWE14_RECORD=DIR`: `DIR\video.mp4`, and `DIR\frames.txt` naming each
//! video frame's game frame). Nothing stalls: each image is copied into one of
//! a few buffers and read a few frames later, once the GPU is done with it, and
//! a writer thread pipes it to ffmpeg (`WWE14_FFMPEG`, else `ffmpeg` on the
//! path; NVENC unless `WWE14_RECORD_CODEC` names another encoder).

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

const SLOTS: usize = 4;

pub(crate) struct Recorder {
    slots: Vec<(wgpu::Buffer, Arc<AtomicBool>)>,
    /// Slots copied into and not yet read, oldest first, with their game
    /// frame and note.
    inflight: VecDeque<(usize, u64, String)>,
    next: usize,
    width: u32,
    height: u32,
    row: u32,
    tx: Option<SyncSender<(u64, String, Vec<u8>)>>,
    dropped: u64,
}

impl Recorder {
    pub(crate) fn start(device: &wgpu::Device, dir: &Path, width: u32, height: u32) -> Option<Recorder> {
        use std::process::{Command, Stdio};
        let _ = std::fs::create_dir_all(dir);
        let ffmpeg = std::env::var("WWE14_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
        let codec = std::env::var("WWE14_RECORD_CODEC").unwrap_or_else(|_| "h264_nvenc".into());
        let mut args: Vec<String> = ["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s"].map(String::from).to_vec();
        args.push(format!("{width}x{height}"));
        args.extend(["-r", "60", "-i", "-", "-c:v"].map(String::from));
        args.push(codec.clone());
        // NVENC takes the RGBA frames as they are and converts them on the GPU;
        // a CPU encoder needs them converted first.
        if codec.contains("nvenc") {
            args.extend(["-preset", "p1", "-rc", "constqp", "-qp", "18"].map(String::from));
        } else {
            args.extend(["-preset", "ultrafast", "-crf", "18", "-pix_fmt", "yuv420p"].map(String::from));
        }
        args.extend(["-movflags", "+frag_keyframe+empty_moov"].map(String::from));
        let video = dir.join("video.mp4");
        let child = Command::new(&ffmpeg).args(&args).arg(&video).stdin(Stdio::piped()).stdout(Stdio::null()).spawn();
        let mut stdin = match child {
            Ok(mut c) => c.stdin.take()?,
            Err(e) => {
                eprintln!("record: no ffmpeg ({ffmpeg}: {e})");
                return None;
            }
        };
        let mut list = std::fs::File::create(dir.join("frames.txt")).ok()?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, String, Vec<u8>)>(16);
        std::thread::Builder::new()
            .name("record".into())
            .spawn(move || {
                let mut index = 0u64;
                while let Ok((frame, note, px)) = rx.recv() {
                    if stdin.write_all(&px).is_err() {
                        eprintln!("record: ffmpeg stopped (video frame {index})");
                        return;
                    }
                    let _ = writeln!(list, "{index} frame {frame} {note}");
                    index += 1;
                }
            })
            .ok()?;
        eprintln!("record: recording every rendered frame to {} ({codec})", video.display());
        let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let slots = (0..SLOTS)
            .map(|_| {
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("recording"),
                    size: (row * height) as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                (buffer, Arc::new(AtomicBool::new(false)))
            })
            .collect();
        Some(Recorder { slots, inflight: VecDeque::new(), next: 0, width, height, row, tx: Some(tx), dropped: 0 })
    }

    /// Queues the display image of game frame `frame` (after the frame's work
    /// was submitted), and passes on the images the GPU has finished copying.
    pub(crate) fn capture(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, target: &wgpu::Texture, frame: u64, note: String) {
        if self.tx.is_none() {
            return;
        }
        let _ = device.poll(wgpu::PollType::Poll);
        self.drain(device, false);
        if self.inflight.len() == SLOTS {
            self.drain(device, true);
        }
        // Stopped (or the oldest copy never mapped): its slot may still be
        // mapping, and copying into a mapped buffer is a validation error that
        // ends the app (2026-11-25). The slots are left alone from now on.
        if self.tx.is_none() || self.inflight.len() == SLOTS {
            return;
        }
        let i = self.next;
        self.next = (i + 1) % SLOTS;
        let (buffer, ready) = &self.slots[i];
        ready.store(false, Ordering::Release);
        let mut enc = device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: target, mip_level: 0, origin: Default::default(), aspect: Default::default() },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.row), rows_per_image: Some(self.height) },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        let flag = ready.clone();
        buffer.map_async(wgpu::MapMode::Read, .., move |r| flag.store(r.is_ok(), Ordering::Release));
        self.inflight.push_back((i, frame, note));
    }

    /// Reads the finished copies, oldest first; with `wait`, waits for the oldest.
    fn drain(&mut self, device: &wgpu::Device, mut wait: bool) {
        while let Some((i, frame, _)) = self.inflight.front() {
            let (i, frame) = (*i, *frame);
            let (buffer, ready) = &self.slots[i];
            if !ready.load(Ordering::Acquire) {
                if !wait {
                    return;
                }
                // The map callback can run on another thread's poll (the
                // window's), just after this one returns: wait for the flag.
                let start = std::time::Instant::now();
                while !ready.load(Ordering::Acquire) && start.elapsed() < std::time::Duration::from_secs(2) {
                    let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_millis(100)) });
                    if !ready.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                if !ready.load(Ordering::Acquire) {
                    eprintln!("record: the copy of frame {frame} was not mapped; recording stops");
                    self.tx = None;
                    return;
                }
            }
            let (_, _, note) = self.inflight.pop_front().expect("the slot just looked at");
            let mut px = Vec::with_capacity((self.width * self.height * 4) as usize);
            if let Ok(view) = buffer.get_mapped_range(..) {
                for y in 0..self.height as usize {
                    let at = y * self.row as usize;
                    px.extend_from_slice(&view[at..at + self.width as usize * 4]);
                }
            }
            buffer.unmap();
            wait = false;
            if let Some(tx) = &self.tx {
                match tx.try_send((frame, note, px)) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // Marked so the video's frames can still be matched to game frames.
                        self.dropped += 1;
                        eprintln!("record: dropped frame {frame} (the encoder is behind; {} dropped)", self.dropped);
                    }
                    Err(TrySendError::Disconnected(_)) => self.tx = None,
                }
            }
        }
    }
}
