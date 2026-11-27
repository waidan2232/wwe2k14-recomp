//! The game's `.pac` archives (PLAN §2.4 item 6, Phase 4 "pac reader").
//!
//! Most files under `pac\` are wrapped in the XDK's XCompress file format
//! (identifier `0x0FF512ED`). The game opens them through an XDK file layer
//! that recognises the header and decompresses transparently
//! (`sub_82C9FE60`, `sub_82CA2440`, `sub_82CA2F78`). Decoding uses LZX, a
//! software codec, so this is data conversion, not emulation.
//!
//! The XCompress header, as the game's code reads it (all big-endian):
//! ```text
//! +0x00 identifier 0x0FF512ED
//! +0x04 version 0x0100, reserved 0
//! +0x08 a hash or checksum (varies per file)
//! +0x0C flags: bits 4-5 window = 0x8000 << n; bits 6-21 table entry count;
//!       bits 22-23 non-zero = 32-bit entries, else 20-bit
//! +0x10 bit-packed table of `count` entries (padded to a whole word)
//! ```
//! Table entries are chunk sizes (uncompressed). Chunk n starts at file offset
//! n × 128 KiB (chunk 0 right after the header): the compressor fills each
//! 128 KiB block, so uncompressed chunk sizes vary. A chunk is either stored
//! (a zero frame size, then the bytes) or an independent LZX stream in
//! XMemCompress frames: each a 2-byte big-endian compressed size, with
//! 32 KiB of output except the chunk's last frame (or 0xFF, a 2-byte
//! uncompressed size and a 2-byte compressed size).
//!
//! Inside are Yuke's own containers (little-endian fields):
//! - `EPAC`: a header, a directory at 0x800 of 4-character section tags with
//!   8-character entry names, and the entries (often `PACH` archives).
//! - `PACH`: a count, then (id, offset, size) per entry, then the entries.
//! - `BPE `: byte-pair-encoded data inside `PACH` entries (see [`bpe_expand`]).
//!
//! The game reads all of this itself through recompiled code; this reader
//! exists to find data the build needs offline, such as shaders.

pub const XCOMPRESS_MAGIC: u32 = 0x0FF5_12ED;
const FRAME: usize = 0x8000;
const BLOCK: usize = 0x20000;

fn be32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    XCompress,
    Pach,
    Epac,
    Epk8,
    Unknown,
}

pub fn kind(data: &[u8]) -> Kind {
    match data.get(0..4) {
        Some([0x0F, 0xF5, 0x12, 0xED]) => Kind::XCompress,
        Some(b"PACH") => Kind::Pach,
        Some(b"EPAC") => Kind::Epac,
        Some(b"EPK8") => Kind::Epk8,
        _ => Kind::Unknown,
    }
}

#[derive(Clone, Debug)]
pub struct XCompressHeader {
    pub version: u16,
    pub hash: u32,
    pub flags: u32,
    pub window: u32,
    /// Table entries (meaning not yet established: per-chunk sizes).
    pub table: Vec<u32>,
    /// Header length: where the LZX frames start.
    pub len: usize,
}

pub fn xcompress_header(data: &[u8]) -> Result<XCompressHeader, String> {
    if be32(data, 0) != Some(XCOMPRESS_MAGIC) {
        return Err("not an XCompress file".into());
    }
    let version = u16::from_be_bytes([data[4], data[5]]);
    if version != 0x0100 {
        return Err(format!("XCompress version {version:#06x} (expected 0x0100)"));
    }
    let hash = be32(data, 8).ok_or("truncated header")?;
    let flags = be32(data, 12).ok_or("truncated header")?;
    let window = 0x8000u32 << ((flags >> 4) & 3);
    let count = ((flags >> 6) & 0xFFFF) as usize;
    let bits = if flags & 0x00C0_0000 != 0 { 32 } else { 20 };
    let words = (count * bits).div_ceil(32);
    let len = 16 + 4 * words;
    let table_bytes = data.get(16..len).ok_or("truncated table")?;
    let bit = |i: usize| (table_bytes[i / 8] >> (7 - i % 8)) & 1;
    let table = (0..count).map(|e| (0..bits).fold(0u32, |v, b| (v << 1) | bit(e * bits + b) as u32)).collect();
    Ok(XCompressHeader { version, hash, flags, window, table, len })
}

/// Decompresses an XCompress-wrapped file.
pub fn decompress_xcompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let h = xcompress_header(data)?;
    let window = match h.window {
        0x8000 => lzxd::WindowSize::KB32,
        0x10000 => lzxd::WindowSize::KB64,
        0x20000 => lzxd::WindowSize::KB128,
        0x40000 => lzxd::WindowSize::KB256,
        w => return Err(format!("unsupported LZX window {w:#x}")),
    };
    let total: usize = h.table.iter().map(|&s| s as usize).sum();
    let mut out = Vec::with_capacity(total);
    let mut o = h.len;
    let mut frame = 0;
    // Each table entry is one chunk's uncompressed size, and each chunk owns
    // one 128 KiB block of the file (chunk 0's block also holds the header).
    // A block starting with a zero frame size holds its chunk stored as is;
    // otherwise the chunk is an independent LZX stream of 32 KiB frames, the
    // last one short, and zero padding may fill the rest of the block.
    for (n, &size) in h.table.iter().enumerate() {
        if n > 0 {
            let start = n * BLOCK;
            let gap = data.get(o..start).ok_or_else(|| format!("chunk {n}: previous chunk overruns its block (at {o:#x})"))?;
            if gap.iter().any(|&b| b != 0) {
                return Err(format!("chunk {n}: non-zero padding at {o:#x}..{start:#x}"));
            }
            o = start;
        }
        let mut left = size as usize;
        if data.get(o..o + 2) == Some(&[0, 0]) {
            let raw = data.get(o + 2..o + 2 + left).ok_or_else(|| format!("chunk {n}: stored chunk runs past the end"))?;
            out.extend_from_slice(raw);
            o += 2 + left;
            continue;
        }
        // The chunk's frames: (uncompressed size, compressed data).
        let mut frames = Vec::new();
        while left > 0 {
            let (u, c, hl) = match *data.get(o).ok_or_else(|| format!("chunk {n}: data ends at frame {frame}"))? {
                0xFF => {
                    let b = data.get(o + 1..o + 5).ok_or_else(|| format!("frame {frame} header truncated at {o:#x}"))?;
                    (u16::from_be_bytes([b[0], b[1]]) as usize, u16::from_be_bytes([b[2], b[3]]) as usize, 5)
                }
                _ => {
                    let b = data.get(o..o + 2).ok_or_else(|| format!("frame {frame} header truncated at {o:#x}"))?;
                    (left.min(FRAME), u16::from_be_bytes([b[0], b[1]]) as usize, 2)
                }
            };
            let bytes = data
                .get(o + hl..o + hl + c)
                .ok_or_else(|| format!("frame {frame} at {o:#x} runs past the end ({c} bytes)"))?;
            frames.push((u, bytes, o));
            left = left.checked_sub(u).ok_or_else(|| format!("chunk {n}: frames overrun its size"))?;
            o += hl + c;
            frame += 1;
        }
        out.extend_from_slice(&decode_chunk(window, &frames).map_err(|e| format!("chunk {n}: {e}"))?);
    }
    if o != data.len() {
        return Err(format!("{} bytes left after the last chunk (at {o:#x})", data.len() - o));
    }
    Ok(out)
}

/// Decodes one chunk's LZX frames.
///
/// When an uncompressed LZX block of odd length ends exactly at a frame
/// boundary, `lzxd` (like libmspack) skips a realignment pad byte at the
/// start of the next frame. XMemCompress writes no pad there: each frame's
/// data stands alone, and the previous frame then has an odd size. The decoder
/// can't be inspected or cloned, so a chunk that fails right after an odd-sized
/// frame is decoded again from its start, giving that frame a leading dummy
/// byte for the skip to consume. The chunk must still produce exactly its
/// table size.
fn decode_chunk(window: lzxd::WindowSize, frames: &[(usize, &[u8], usize)]) -> Result<Vec<u8>, String> {
    let mut padded = vec![false; frames.len()];
    loop {
        let mut lzx = lzxd::Lzxd::new(window);
        let mut out = Vec::new();
        let mut failed = None;
        for (i, &(u, bytes, at)) in frames.iter().enumerate() {
            let result = if padded[i] {
                let mut b = Vec::with_capacity(bytes.len() + 1);
                b.push(0);
                b.extend_from_slice(bytes);
                lzx.decompress_next(&b, u).map(|p| p.to_vec())
            } else {
                lzx.decompress_next(bytes, u).map(|p| p.to_vec())
            };
            match result {
                Ok(p) => out.extend_from_slice(&p),
                Err(e) => {
                    failed = Some((i, format!("frame at {at:#x}: {e}")));
                    break;
                }
            }
        }
        match failed {
            None => return Ok(out),
            Some((i, _)) if i > 0 && frames[i - 1].1.len() % 2 == 1 && !padded[i] => padded[i] = true,
            Some((_, e)) => return Err(e),
        }
    }
}

/// Expands one `BPE ` stream: Yuke's byte-pair-encoding compression, used
/// for entries inside `PACH` archives. Header (little-endian): `BPE `,
/// version 0x100, packed size, unpacked size; then Philip Gage's BPE blocks
/// (a pair table, a 2-byte little-endian block length, the packed block).
pub fn bpe_expand(data: &[u8]) -> Result<Vec<u8>, String> {
    let le = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    if data.get(0..4) != Some(b"BPE ") {
        return Err("not a BPE stream".into());
    }
    let (version, packed, unpacked) = (le(4).ok_or("truncated")?, le(8).ok_or("truncated")? as usize, le(12).ok_or("truncated")? as usize);
    if version != 0x100 {
        return Err(format!("BPE version {version:#x}"));
    }
    let src = data.get(16..16 + packed).ok_or("BPE data runs past the end")?;
    // A false match can carry any sizes: never trust them for allocation.
    if unpacked > 1 << 30 {
        return Err(format!("BPE unpacked size {unpacked:#x} is implausible"));
    }
    let mut out = Vec::with_capacity(unpacked.min(packed.saturating_mul(16) + 4096));
    let mut i = 0;
    let next = |i: &mut usize| -> Result<u8, String> {
        let b = *src.get(*i).ok_or("BPE stream truncated")?;
        *i += 1;
        Ok(b)
    };
    let (mut left, mut right) = ([0u8; 256], [0u8; 256]);
    let mut stack = Vec::with_capacity(256);
    while i < src.len() {
        for (c, l) in left.iter_mut().enumerate() {
            *l = c as u8;
        }
        // Pair table: runs of literal codes and of pair definitions.
        let mut c = 0usize;
        let mut count = next(&mut i)? as usize;
        loop {
            if count > 127 {
                c += count - 127;
                count = 0;
            }
            if c == 256 {
                break;
            }
            for _ in 0..=count {
                if c >= 256 {
                    return Err("BPE pair table overruns".into());
                }
                left[c] = next(&mut i)?;
                if c != left[c] as usize {
                    right[c] = next(&mut i)?;
                }
                c += 1;
            }
            if c == 256 {
                break;
            }
            count = next(&mut i)? as usize;
        }
        // Yuke's block length is little-endian (Gage's original is big-endian).
        let mut size = next(&mut i)? as usize | ((next(&mut i)? as usize) << 8);
        loop {
            let c = match stack.pop() {
                Some(c) => c,
                None => {
                    if size == 0 {
                        break;
                    }
                    size -= 1;
                    next(&mut i)?
                }
            };
            if c == left[c as usize] {
                out.push(c);
            } else {
                // Pairs nest at most 256 deep; deeper means a cyclic (false) table.
                if stack.len() > 512 {
                    return Err("BPE pair table is cyclic".into());
                }
                stack.push(right[c as usize]);
                stack.push(left[c as usize]);
            }
            if out.len() > unpacked {
                return Err("BPE output exceeds its declared size".into());
            }
        }
    }
    if out.len() != unpacked {
        return Err(format!("BPE produced {} bytes, header says {unpacked}", out.len()));
    }
    Ok(out)
}

/// Every `BPE ` stream in a buffer that expands cleanly: (offset, contents).
pub fn find_bpe(data: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let mut o = 0;
    while let Some(p) = data[o..].windows(4).position(|w| w == b"BPE ") {
        let at = o + p;
        if let Ok(d) = bpe_expand(&data[at..]) {
            out.push((at, d));
        }
        o = at + 4;
    }
    out
}

/// The file's contents with any XCompress wrapper removed.
pub fn unwrap(data: Vec<u8>) -> Result<Vec<u8>, String> {
    if kind(&data) == Kind::XCompress { decompress_xcompress(&data) } else { Ok(data) }
}
