//! Microsoft LZX decompressor, written from the published format description
//! (the LZX bitstream used by CAB/XEX: 32 KiB frames, verbatim/aligned/uncompressed
//! blocks, canonical Huffman main/length/aligned trees sent as deltas through a
//! 20-symbol pretree, three repeated offsets).
//!
//! XEX "normal" compression and XEX delta patches both use this bitstream. A delta
//! patch decodes with the window pre-loaded with reference data (the bytes being
//! patched), so matches may reach back into that data.
//!
//! Xbox streams never use the Intel E8 call translation. A stream that turns it on
//! is rejected loudly rather than silently mis-decoded.
//!
//! This is a data codec used at build time. It never executes guest code.

use crate::{Error, Result};

const FRAME_SIZE: usize = 0x8000;
const NUM_CHARS: usize = 256;
const PRETREE_SIZE: usize = 20;
const LENGTH_TREE_SIZE: usize = 249;
const ALIGNED_TREE_SIZE: usize = 8;
const MIN_MATCH: usize = 2;
const NUM_PRIMARY_LENGTHS: u16 = 7;

const BLOCK_VERBATIM: u32 = 1;
const BLOCK_ALIGNED: u32 = 2;
const BLOCK_UNCOMPRESSED: u32 = 3;

fn bad(msg: impl Into<String>) -> Error {
    Error::Invalid(format!("LZX: {}", msg.into()))
}

/// Number of position slots for a window of `1 << bits` bytes.
fn position_slots(bits: u32) -> Option<usize> {
    Some(match bits {
        15 => 30,
        16 => 32,
        17 => 34,
        18 => 36,
        19 => 38,
        20 => 42,
        21 => 50,
        _ => return None,
    })
}

fn extra_bits(slot: usize) -> u32 {
    if slot < 4 { 0 } else { ((slot as u32 - 2) / 2).min(17) }
}

fn position_base(slot: usize) -> u32 {
    (0..slot).map(|s| 1u32 << extra_bits(s)).sum()
}

/// MSB-first bit reader over 16-bit little-endian words.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    word: u32,
    left: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, word: 0, left: 0 }
    }

    fn bit(&mut self) -> Result<u32> {
        if self.left == 0 {
            let w = self
                .data
                .get(self.pos..self.pos + 2)
                .ok_or_else(|| bad(format!("bitstream ends early at byte 0x{:X}", self.pos)))?;
            self.word = u16::from_le_bytes([w[0], w[1]]) as u32;
            self.pos += 2;
            self.left = 16;
        }
        self.left -= 1;
        Ok((self.word >> self.left) & 1)
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// End of a 32 KiB frame: drop the rest of the current 16-bit word.
    fn align_frame(&mut self) {
        self.left = 0;
    }

    /// Start of an uncompressed block: skip 1..=16 bits to the next word boundary.
    fn align_uncompressed(&mut self) {
        if self.left > 0 {
            self.left = 0;
        } else {
            self.pos += 2;
        }
    }

    fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        debug_assert_eq!(self.left, 0);
        let s = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or_else(|| bad(format!("uncompressed block runs past the input at byte 0x{:X}", self.pos)))?;
        self.pos += n;
        Ok(s)
    }

    fn raw_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.raw(4)?.try_into().unwrap()))
    }
}

/// Canonical Huffman decoder (codes assigned in symbol order within each length).
struct Huffman {
    counts: [u16; 17],
    symbols: Vec<u16>,
    empty: bool,
}

impl Huffman {
    fn new(lens: &[u8], what: &str) -> Result<Huffman> {
        let mut counts = [0u16; 17];
        for &l in lens {
            if l > 16 {
                return Err(bad(format!("{what} code length {l} > 16")));
            }
            counts[l as usize] += 1;
        }
        let empty = counts[0] as usize == lens.len();
        counts[0] = 0;
        // Reject over-subscribed trees.
        let mut space: i64 = 1;
        for len in 1..=16 {
            space = space * 2 - counts[len] as i64;
            if space < 0 {
                return Err(bad(format!("{what} tree is over-subscribed")));
            }
        }
        let mut offs = [0u16; 17];
        for len in 1..16 {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut symbols = vec![0u16; lens.len()];
        for (sym, &l) in lens.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols, empty })
    }

    fn decode(&self, bits: &mut Bits, what: &str) -> Result<u16> {
        if self.empty {
            return Err(bad(format!("symbol read from empty {what} tree")));
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=16 {
            code |= bits.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(bad(format!("invalid {what} code")))
    }
}

/// Reads code lengths `lens[first..last]` as deltas via a freshly sent pretree.
fn read_lengths(bits: &mut Bits, lens: &mut [u8], first: usize, last: usize) -> Result<()> {
    let mut pre = [0u8; PRETREE_SIZE];
    for l in &mut pre {
        *l = bits.bits(4)? as u8;
    }
    let pre = Huffman::new(&pre, "pretree")?;
    let mut x = first;
    while x < last {
        let z = pre.decode(bits, "pretree")?;
        let (run, value) = match z {
            17 => (bits.bits(4)? as usize + 4, 0),
            18 => (bits.bits(5)? as usize + 20, 0),
            19 => {
                let run = bits.bits(1)? as usize + 4;
                let z = pre.decode(bits, "pretree")? as u8;
                if z > 16 {
                    return Err(bad("pretree run value out of range"));
                }
                (run, (lens[x] + 17 - z) % 17)
            }
            z => (1, (lens[x] + 17 - z as u8) % 17),
        };
        if x + run > last {
            return Err(bad("code-length run overflows its tree"));
        }
        lens[x..x + run].fill(value);
        x += run;
    }
    Ok(())
}

/// Decompresses one LZX stream into exactly `out_len` bytes.
///
/// `window_size` is the encoder's window (a power of two, 32 KiB..2 MiB).
/// `reference` pre-loads the end of the window (LZX delta patching); pass `&[]`
/// for ordinary decompression. Returns the output and the number of input bytes
/// consumed.
pub fn decompress(input: &[u8], out_len: usize, window_size: u32, reference: &[u8]) -> Result<(Vec<u8>, usize)> {
    let window_bits = window_size.max(1).next_power_of_two().trailing_zeros();
    let slots = position_slots(window_bits)
        .ok_or_else(|| Error::Unsupported(format!("LZX window size 0x{window_size:X}")))?;
    let window = 1usize << window_bits;
    if reference.len() > window {
        return Err(bad(format!("reference data (0x{:X} bytes) is larger than the window", reference.len())));
    }
    let num_main = NUM_CHARS + slots * 8;

    // out = initial window ++ decoded bytes. The window starts zeroed, with the
    // reference (if any) at its end, so matches may reach back up to a full window
    // into the reference and the zeros before it. (TU1's patches do reach into the
    // zeros.) Keeping the window linear before the output is equivalent to the
    // circular window for offsets up to the window size.
    let prefix = window;
    let mut out = Vec::with_capacity(prefix + out_len);
    out.resize(window - reference.len(), 0);
    out.extend_from_slice(reference);

    let mut bits = Bits::new(input);
    let mut main_lens = vec![0u8; num_main];
    let mut length_lens = vec![0u8; LENGTH_TREE_SIZE];
    let mut main = Huffman::new(&main_lens, "main")?;
    let mut length = Huffman::new(&length_lens, "length")?;
    let mut aligned = Huffman::new(&[0; ALIGNED_TREE_SIZE], "aligned")?;
    let (mut r0, mut r1, mut r2) = (1u32, 1u32, 1u32);
    let mut block_type = 0u32;
    let mut block_length = 0usize;
    let mut block_remaining = 0usize;

    if bits.bit()? != 0 {
        return Err(Error::Unsupported("LZX stream enables Intel E8 translation (never used on Xbox)".into()));
    }

    let mut produced = 0usize;
    while produced < out_len {
        let frame_end = (produced + FRAME_SIZE).min(out_len);
        while produced < frame_end {
            if block_remaining == 0 {
                if block_type == BLOCK_UNCOMPRESSED && block_length & 1 == 1 {
                    bits.raw(1)?; // pad byte after an odd-length uncompressed block
                }
                block_type = bits.bits(3)?;
                block_length = bits.bits(24)? as usize;
                block_remaining = block_length;
                match block_type {
                    BLOCK_VERBATIM | BLOCK_ALIGNED => {
                        if block_type == BLOCK_ALIGNED {
                            let mut lens = [0u8; ALIGNED_TREE_SIZE];
                            for l in &mut lens {
                                *l = bits.bits(3)? as u8;
                            }
                            aligned = Huffman::new(&lens, "aligned")?;
                        }
                        read_lengths(&mut bits, &mut main_lens, 0, NUM_CHARS)?;
                        read_lengths(&mut bits, &mut main_lens, NUM_CHARS, num_main)?;
                        main = Huffman::new(&main_lens, "main")?;
                        read_lengths(&mut bits, &mut length_lens, 0, LENGTH_TREE_SIZE)?;
                        length = Huffman::new(&length_lens, "length")?;
                    }
                    BLOCK_UNCOMPRESSED => {
                        bits.align_uncompressed();
                        r0 = bits.raw_u32()?;
                        r1 = bits.raw_u32()?;
                        r2 = bits.raw_u32()?;
                    }
                    t => return Err(bad(format!("invalid block type {t} at output byte 0x{produced:X}"))),
                }
                continue;
            }

            if block_type == BLOCK_UNCOMPRESSED {
                let n = block_remaining.min(frame_end - produced);
                out.extend_from_slice(bits.raw(n)?);
                produced += n;
                block_remaining -= n;
                continue;
            }

            let sym = main.decode(&mut bits, "main")?;
            if (sym as usize) < NUM_CHARS {
                out.push(sym as u8);
                produced += 1;
                block_remaining -= 1;
                continue;
            }
            let sym = sym - NUM_CHARS as u16;
            let mut match_len = (sym & 7) as usize;
            if sym & 7 == NUM_PRIMARY_LENGTHS {
                match_len += length.decode(&mut bits, "length")? as usize;
            }
            match_len += MIN_MATCH;
            let slot = (sym >> 3) as usize;
            let offset = match slot {
                0 => r0,
                1 => {
                    std::mem::swap(&mut r0, &mut r1);
                    r0
                }
                2 => {
                    std::mem::swap(&mut r0, &mut r2);
                    r0
                }
                _ => {
                    let extra = extra_bits(slot);
                    let mut off = position_base(slot) - 2;
                    if block_type == BLOCK_ALIGNED && extra >= 3 {
                        off += bits.bits(extra - 3)? << 3;
                        off += aligned.decode(&mut bits, "aligned")? as u32;
                    } else if extra > 0 {
                        off += bits.bits(extra)?;
                    } else {
                        off = 1;
                    }
                    r2 = r1;
                    r1 = r0;
                    r0 = off;
                    off
                }
            } as usize;
            if match_len > block_remaining || produced + match_len > frame_end {
                return Err(bad(format!(
                    "match of {match_len} bytes at output 0x{produced:X} overruns its block or frame"
                )));
            }
            if offset == 0 || offset > window {
                return Err(bad(format!("match offset 0x{offset:X} at output 0x{produced:X} is outside the window")));
            }
            let start = out.len() - offset;
            for i in 0..match_len {
                let b = out[start + i];
                out.push(b);
            }
            produced += match_len;
            block_remaining -= match_len;
        }
        if block_type != BLOCK_UNCOMPRESSED {
            bits.align_frame();
        }
    }
    let consumed = bits.pos;
    out.drain(..prefix);
    Ok((out, consumed))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test-only LZX bitstream writer (MSB-first into 16-bit LE words).
    pub(crate) struct Writer {
        out: Vec<u8>,
        acc: u32,
        n: u32,
    }

    impl Writer {
        pub(crate) fn new() -> Self {
            Writer { out: Vec::new(), acc: 0, n: 0 }
        }
        pub(crate) fn put(&mut self, value: u32, bits: u32) {
            for i in (0..bits).rev() {
                self.acc = (self.acc << 1) | ((value >> i) & 1);
                self.n += 1;
                if self.n == 16 {
                    self.out.extend_from_slice(&(self.acc as u16).to_le_bytes());
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
        pub(crate) fn flush(&mut self) {
            if self.n > 0 {
                self.put(0, 16 - self.n);
            }
        }
        pub(crate) fn finish(mut self) -> Vec<u8> {
            self.flush();
            self.out
        }
        pub(crate) fn bytes(&mut self, b: &[u8]) {
            assert_eq!(self.n, 0);
            self.out.extend_from_slice(b);
        }
    }

    /// Canonical codes for `lens` (same assignment as the decoder).
    fn codes(lens: &[u8]) -> Vec<(u32, u32)> {
        let mut out = vec![(0, 0); lens.len()];
        let mut code = 0u32;
        for len in 1..=16u8 {
            for (s, &l) in lens.iter().enumerate() {
                if l == len {
                    out[s] = (code, len as u32);
                    code += 1;
                }
            }
            code <<= 1;
        }
        out
    }

    /// Sends `lens` (previous lengths all zero) through a pretree in which symbols
    /// 0..15 have 4-bit codes (a complete tree).
    fn send_lengths(w: &mut Writer, lens: &[u8]) {
        let pre: Vec<u8> = (0..20).map(|i| if i < 16 { 4 } else { 0 }).collect();
        for &l in &pre {
            w.put(l as u32, 4);
        }
        let pc = codes(&pre);
        for &l in lens {
            let z = (17 - l as u32) % 17;
            assert!(z < 16, "length {l} not representable in test pretree");
            w.put(pc[z as usize].0, pc[z as usize].1);
        }
    }

    /// One verbatim block, 32 KiB window. Main tree: literals 9 bits, match symbols
    /// 256..384 8 bits. Length tree empty. `ops` are literals or (slot, header, extra bits, nbits).
    enum Op {
        Lit(u8),
        Match { slot: u32, len: u32, extra: u32, nbits: u32 },
    }

    fn verbatim_stream(ops: &[Op], total: usize) -> Vec<u8> {
        let mut w = Writer::new();
        w.put(0, 1); // no E8
        w.put(BLOCK_VERBATIM, 3);
        w.put(total as u32, 24);
        let num_main = 256 + 30 * 8;
        let mut main: Vec<u8> = vec![0; num_main];
        main[..256].fill(9);
        main[256..384].fill(8);
        send_lengths(&mut w, &main[..256]);
        send_lengths(&mut w, &main[256..]);
        send_lengths(&mut w, &[0; LENGTH_TREE_SIZE]);
        let mc = codes(&main);
        for op in ops {
            match *op {
                Op::Lit(b) => w.put(mc[b as usize].0, mc[b as usize].1),
                Op::Match { slot, len, extra, nbits } => {
                    let s = 256 + slot * 8 + (len - 2);
                    w.put(mc[s as usize].0, mc[s as usize].1);
                    w.put(extra, nbits);
                }
            }
        }
        w.finish()
    }

    #[test]
    fn verbatim_literals_and_matches() {
        // "abc" + match(offset 3, len 8) + "X" + repeat-offset R0 (3) match of 4
        let ops = [
            Op::Lit(b'a'),
            Op::Lit(b'b'),
            Op::Lit(b'c'),
            // offset 3: slot 4 (base 4, 1 extra bit): 4 - 2 + 1 = 3
            Op::Match { slot: 4, len: 8, extra: 1, nbits: 1 },
            Op::Lit(b'X'),
            Op::Match { slot: 0, len: 4, extra: 0, nbits: 0 },
        ];
        let expected = b"abcabcabcabXabXa";
        let s = verbatim_stream(&ops, expected.len());
        let (out, used) = decompress(&s, expected.len(), 0x8000, &[]).unwrap();
        assert_eq!(out, expected);
        assert_eq!(used, s.len());
    }

    #[test]
    fn matches_reach_into_reference_window() {
        let reference = b"hello world";
        // offset 11 (back to 'h'): slot 7 (base 12, 2 extra bits): 12 - 2 + 1 = 11
        let ops = [Op::Match { slot: 7, len: 5, extra: 1, nbits: 2 }, Op::Lit(b'!')];
        let s = verbatim_stream(&ops, 6);
        let (out, _) = decompress(&s, 6, 0x8000, reference).unwrap();
        assert_eq!(out, b"hello!");
        // Without the reference the match reads the zeroed window.
        assert_eq!(decompress(&s, 6, 0x8000, &[]).unwrap().0, b"\0\0\0\0\0!");
        // Reference shorter than the reach: zeros come first, then the reference.
        assert_eq!(decompress(&s, 6, 0x8000, b"llo world").unwrap().0, b"\0\0llo!");
    }

    #[test]
    fn uncompressed_blocks_with_padding() {
        let mut w = Writer::new();
        w.put(0, 1);
        w.put(BLOCK_UNCOMPRESSED, 3);
        w.put(3, 24);
        w.flush(); // 28 bits used -> pad to the word boundary
        for r in [1u32, 1, 1] {
            w.bytes(&r.to_le_bytes());
        }
        w.bytes(b"xyz");
        w.bytes(&[0]); // odd-length pad
        // Second block: 27 header bits leave 5 unused bits in the second word.
        w.put(BLOCK_UNCOMPRESSED, 3);
        w.put(2, 24);
        w.flush();
        for r in [1u32, 1, 1] {
            w.bytes(&r.to_le_bytes());
        }
        w.bytes(b"!!");
        let s = w.finish();
        let (out, used) = decompress(&s, 5, 0x8000, &[]).unwrap();
        assert_eq!(out, b"xyz!!");
        assert_eq!(used, s.len());
    }

    #[test]
    fn uncompressed_header_on_word_boundary_skips_a_word() {
        // Ending exactly on a word boundary skips a whole padding word; otherwise only
        // the rest of the current word is skipped.
        let data = [0u8; 8];
        let mut b = Bits::new(&data);
        b.bits(16).unwrap();
        b.align_uncompressed();
        assert_eq!(b.pos, 4);
        let mut b = Bits::new(&data);
        b.bits(5).unwrap();
        b.align_uncompressed();
        assert_eq!(b.pos, 2);
    }

    #[test]
    fn position_tables() {
        assert_eq!(position_base(4), 4);
        assert_eq!(position_base(6), 8);
        assert_eq!(position_base(30), 1 << 15);
        assert_eq!(extra_bits(37), 17);
    }
}
