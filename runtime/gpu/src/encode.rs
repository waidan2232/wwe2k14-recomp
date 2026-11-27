//! Image files for previews: PNG stills and animated GIFs, with no
//! dependencies (stored-deflate PNG; GIF with a fixed palette and LZW).

use std::collections::HashMap;
use std::io::Write;

// ---- PNG (stored deflate) -----------------------------------------------------------

fn crc32(data: &[u8], mut crc: u32) -> u32 {
    crc = !crc;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

pub fn write_png(path: &std::path::Path, rgba: &[u8], w: usize, h: usize) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&rgba[(y * w + x) * 4..(y * w + x) * 4 + 3]);
        }
    }
    let mut z = vec![0x78, 0x01];
    for (i, chunk) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        z.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"\x89PNG\r\n\x1a\n")?;
    let mut chunk = |kind: &[u8], data: &[u8]| -> std::io::Result<()> {
        f.write_all(&(data.len() as u32).to_be_bytes())?;
        f.write_all(kind)?;
        f.write_all(data)?;
        f.write_all(&crc32(data, crc32(kind, 0)).to_be_bytes())
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr)?;
    chunk(b"IDAT", &z)?;
    chunk(b"IEND", &[])
}

// ---- Animated GIF (fixed 6x6x6 palette, ordered dither) --------------------------

pub fn gif_start(path: &std::path::Path, w: usize, h: usize) -> std::io::Result<std::fs::File> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"GIF89a")?;
    f.write_all(&(w as u16).to_le_bytes())?;
    f.write_all(&(h as u16).to_le_bytes())?;
    f.write_all(&[0xF7, 0, 0])?; // global table, 256 entries
    for i in 0..256u32 {
        let (r, g, b) = if i < 216 { (i / 36, (i / 6) % 6, i % 6) } else { (0, 0, 0) };
        f.write_all(&[(r * 51) as u8, (g * 51) as u8, (b * 51) as u8])?;
    }
    f.write_all(&[0x21, 0xFF, 11])?;
    f.write_all(b"NETSCAPE2.0")?;
    f.write_all(&[3, 1, 0, 0, 0])?;
    f.write_all(&[0x3B])?;
    Ok(f)
}

/// Appends a frame, keeping the file a complete GIF (the trailer is rewritten).
pub fn gif_frame(f: &mut std::fs::File, rgba: &[u8], w: usize, h: usize, delay_cs: u16) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    let mut idx = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let p = &rgba[(y * w + x) * 4..];
            let t = (BAYER[y & 3][x & 3] as u32 * 51) / 16;
            let q = |c: u8| ((c as u32 + t).min(255 + 25) / 51).min(5);
            idx.push((q(p[0]) * 36 + q(p[1]) * 6 + q(p[2])) as u8);
        }
    }
    f.seek(SeekFrom::End(-1))?;
    let mut out = vec![0x21, 0xF9, 4, 0x04];
    out.extend_from_slice(&delay_cs.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0x2C, 0, 0, 0, 0]);
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.push(0);
    out.push(8);
    let data = lzw(&idx, 8);
    for c in data.chunks(255) {
        out.push(c.len() as u8);
        out.extend_from_slice(c);
    }
    out.push(0);
    out.push(0x3B);
    f.write_all(&out)
}

/// GIF LZW (variable code width, with clear codes when the table fills).
fn lzw(data: &[u8], min_bits: u32) -> Vec<u8> {
    let clear = 1u32 << min_bits;
    let end = clear + 1;
    let mut out = Vec::new();
    let (mut acc, mut nbits) = (0u32, 0u32);
    let mut emit = |code: u32, width: u32, out: &mut Vec<u8>| {
        acc |= code << nbits;
        nbits += width;
        while nbits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            nbits -= 8;
        }
    };
    let mut dict: HashMap<(u32, u8), u32> = HashMap::new();
    let mut next = end + 1;
    let mut width = min_bits + 1;
    emit(clear, width, &mut out);
    let mut cur: Option<u32> = None;
    for &b in data {
        match cur {
            None => cur = Some(b as u32),
            Some(c) => {
                if let Some(&n) = dict.get(&(c, b)) {
                    cur = Some(n);
                } else {
                    emit(c, width, &mut out);
                    if next < 4096 {
                        dict.insert((c, b), next);
                        next += 1;
                        if next > (1 << width) && width < 12 {
                            width += 1;
                        }
                    } else {
                        emit(clear, width, &mut out);
                        dict.clear();
                        next = end + 1;
                        width = min_bits + 1;
                    }
                    cur = Some(b as u32);
                }
            }
        }
    }
    if let Some(c) = cur {
        emit(c, width, &mut out);
    }
    emit(end, width, &mut out);
    if nbits > 0 {
        out.push(acc as u8);
    }
    out
}

/// Halves an RGBA image in each direction (box filter).
pub fn downscale_half(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (ow, oh) = (w / 2, h / 2);
    let mut out = vec![0u8; ow * oh * 4];
    for y in 0..oh {
        for x in 0..ow {
            for c in 0..4 {
                let s = |dx: usize, dy: usize| src[((2 * y + dy) * w + 2 * x + dx) * 4 + c] as u32;
                out[(y * ow + x) * 4 + c] = ((s(0, 0) + s(1, 0) + s(0, 1) + s(1, 1)) / 4) as u8;
            }
        }
    }
    out
}
