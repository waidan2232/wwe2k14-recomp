//! `Game_BpeExpand` (`sub_82C41918`): the game's decoder for its `BPE `
//! streams (Yuke's packaging of Philip Gage's byte-pair encoding), run natively.
//!
//! The game expands some data on its main thread as it needs it: at the
//! start of a match this took 110–130 ms of a frame, most of it in this one
//! routine (`profile::watch_hitches`). The native version reads and writes guest
//! memory exactly as the original does:
//!
//! - `r3`: the stream: `BPE `, version, packed size, then the unpacked size
//!   (little-endian) at `+0xC`, and the blocks from `+0x10`.
//! - `r4`: where the output goes.
//! - Blocks are expanded until the output reaches the unpacked size (checked
//!   after each block, as the original does; the packed size is not used).
//! - A block: the pair table (runs of literal codes and pair definitions,
//!   Gage's format), a 2-byte little-endian length, then that many packed
//!   bytes, each expanded through the table with a stack.
//! - Returns the output's end (`r4` + unpacked size) in `r3`, as the original
//!   leaves it.
//!
//! `WWE14_BPE=original` runs the recompiled routine; `WWE14_BPE=verify` runs
//! both and fails loudly on any difference.

use ppc_rt::Ctx;

use crate::{diag, mem};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Native,
    Original,
    Verify,
}

fn mode() -> Mode {
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("WWE14_BPE").as_deref() {
        Ok("original") => Mode::Original,
        Ok("verify") => Mode::Verify,
        _ => Mode::Native,
    })
}

/// Expands the stream at guest `src` into `out` (bytes, in order), reading
/// guest memory as the original does. Returns the output's guest length.
fn expand(src: u32, out: &mut Vec<u8>) -> u32 {
    let unpacked = u32::from_le_bytes([mem::r8(src + 0xC), mem::r8(src + 0xD), mem::r8(src + 0xE), mem::r8(src + 0xF)]);
    let mut at = src + 0x10;
    let mut next = || {
        let b = mem::r8(at);
        at = at.wrapping_add(1);
        b
    };
    let (mut left, mut right) = ([0u8; 256], [0u8; 256]);
    let mut stack: Vec<u8> = Vec::with_capacity(256);
    out.clear();
    while (out.len() as u32) < unpacked {
        for (c, l) in left.iter_mut().enumerate() {
            *l = c as u8;
        }
        // Pair table: a count over 127 skips (count - 127) codes, then one
        // entry follows; otherwise (count + 1) entries follow.
        let mut c = 0usize;
        loop {
            let count = next() as usize;
            let entries = if count > 127 {
                c += count - 127;
                if c == 256 {
                    break;
                }
                1
            } else {
                count + 1
            };
            for _ in 0..entries {
                // The original indexes its 256-byte tables with the code
                // unchecked; a well-formed stream never passes 255.
                let k = c & 0xFF;
                left[k] = next();
                if c != left[k] as usize {
                    right[k] = next();
                }
                c += 1;
            }
            if c == 256 {
                break;
            }
        }
        // Little-endian block length, then the packed bytes.
        let lo = next() as usize;
        let size = lo | (next() as usize) << 8;
        for _ in 0..size {
            let mut code = next();
            loop {
                if left[code as usize] == code {
                    out.push(code);
                    match stack.pop() {
                        Some(s) => code = s,
                        None => break,
                    }
                } else {
                    stack.push(right[code as usize]);
                    code = left[code as usize];
                }
            }
        }
    }
    unpacked
}

/// `sub_82C41918(stream, out)`.
pub extern "C-unwind" fn bpe_expand(c: &mut Ctx, b: *mut u8) {
    let (src, dst) = (c.r[3] as u32, c.r[4] as u32);
    match mode() {
        Mode::Original => unsafe { ppc_rt::original(0x82C4_1918).expect("the BPE decoder is replaced")(c, b) },
        Mode::Native => {
            let mut out = Vec::new();
            let unpacked = expand(src, &mut out);
            mem::write(dst, &out);
            c.r[3] = u64::from(dst.wrapping_add(unpacked));
        }
        Mode::Verify => {
            let mut out = Vec::new();
            let unpacked = expand(src, &mut out);
            unsafe { ppc_rt::original(0x82C4_1918).expect("the BPE decoder is replaced")(c, b) };
            let theirs = mem::read(dst, out.len());
            let end = dst.wrapping_add(unpacked);
            if theirs != out || c.r[3] as u32 != end {
                let at = theirs.iter().zip(&out).position(|(a, b)| a != b);
                panic!(
                    "BPE verify: stream {src:08X} -> {dst:08X}: native and original differ (first differing byte {at:?} of {}, returned {:08X} vs {end:08X})",
                    out.len(),
                    c.r[3] as u32
                );
            }
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n <= 20 || n.is_power_of_two() {
                diag::log(&format!("bpe: verify {n}: {} bytes from {src:08X} match", out.len()));
            }
        }
    }
}
