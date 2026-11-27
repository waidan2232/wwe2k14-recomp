//! Xenos textures as D3D describes them: the 6-word texture fetch constant,
//! the 2D tiled memory layout, and conversion of texel formats to RGBA8.
//!
//! This is data conversion at the API boundary: D3D keeps each bound texture's
//! fetch constant in the device's shadow state (`+0x480`, 32 slots of 24
//! bytes), and the texels sit in guest memory in the console's tiled layout.

use kernel::mem;

/// A decoded texture fetch constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub tiled: bool,
    /// Row pitch in texels (multiple of 32).
    pub pitch: u32,
    pub format: u32,
    /// 0 none, 1 8-in-16, 2 8-in-32, 3 16-in-32.
    pub endian: u32,
    /// Physical address of the base level.
    pub base: u32,
    pub width: u32,
    pub height: u32,
    /// 0 1D, 1 2D, 2 3D, 3 cube.
    pub dimension: u32,
    /// Channel swizzle (word 3 bits 1-12): per R, G, B, A, 3 bits selecting
    /// X, Y, Z, W (0-3), zero (4) or one (5) of the fetched texel.
    pub swizzle: u32,
    /// How each of X, Y, Z, W converts to a number (word 0 bits 2-9): 0
    /// unsigned fraction, 1 signed fraction, 2 unsigned biased (2x - 1), 3
    /// gamma (to linear).
    pub signs: [u8; 4],
    /// Power of two the fetched values are scaled by (word 3 bits 13-18, signed).
    pub exp_adjust: i32,
    /// Integer values rather than fractions (word 3 bit 0, `num_format`).
    pub integer: bool,
    /// The highest mip level stored (word 4 bits 6-9).
    pub mip_max: u32,
    /// Mip levels too small for a tile of their own share one (word 5 bit 11).
    pub packed_mips: bool,
    /// Physical address of mip level 1 (word 5 bits 12-31); 0 when there are none.
    pub mip_address: u32,
}

impl Fetch {
    /// Decodes fetch-constant slot words. `None` unless it describes a texture.
    /// Type 2 is a texture; the game also samples slots D3D left at type 0
    /// (word 0 bits 0-1) with a texture's fields in them, which the console's
    /// texture fetches read as a texture regardless. Type 3 (vertices) and 1 are not.
    pub fn decode(w: [u32; 6]) -> Option<Fetch> {
        if !matches!(w[0] & 3, 0 | 2) || (w[1] & 0xFFFF_F000) == 0 {
            return None;
        }
        let dimension = (w[5] >> 9) & 3;
        let (width, height) = match dimension {
            0 => ((w[2] & 0xFF_FFFF) + 1, 1),
            1 | 3 => ((w[2] & 0x1FFF) + 1, ((w[2] >> 13) & 0x1FFF) + 1),
            _ => ((w[2] & 0x7FF) + 1, ((w[2] >> 11) & 0x7FF) + 1),
        };
        Some(Fetch {
            tiled: w[0] >> 31 != 0,
            pitch: ((w[0] >> 22) & 0x1FF) * 32,
            format: w[1] & 0x3F,
            endian: (w[1] >> 6) & 3,
            base: w[1] & 0xFFFF_F000,
            width,
            height,
            dimension,
            swizzle: (w[3] >> 1) & 0xFFF,
            signs: core::array::from_fn(|c| ((w[0] >> (2 + 2 * c)) & 3) as u8),
            exp_adjust: (((w[3] >> 13) & 0x3F) as i32) << 26 >> 26,
            integer: w[3] & 1 != 0,
            mip_max: (w[4] >> 6) & 0xF,
            packed_mips: (w[5] >> 11) & 1 != 0,
            mip_address: w[5] & 0xFFFF_F000,
        })
    }

    /// The mip levels after the base, each as a fetch of its own: the levels
    /// stored one after another from `mip_address`, every one tiled on its own
    /// at the power-of-two size the base rounds up to, halved per level, in
    /// whole 32x32-block tiles on 4 KB boundaries. Levels in the packed tail
    /// (32x32 blocks shared by every level 16 blocks or less on a side) are
    /// left out: the GPU then samples the smallest level it has. Only tiled 2D
    /// textures; others get no mips.
    pub fn mip_levels(&self) -> Vec<Fetch> {
        let mut out = Vec::new();
        let Some((bpp, block)) = format_layout(self.format) else { return out };
        if self.mip_address == 0 || self.mip_max == 0 || self.dimension != 1 || !self.tiled {
            return out;
        }
        let (pw, ph) = (self.width.next_power_of_two(), self.height.next_power_of_two());
        let mut addr = self.mip_address;
        for level in 1..=self.mip_max.min(12) {
            let (bw, bh) = (((pw >> level).max(1)).div_ceil(block), ((ph >> level).max(1)).div_ceil(block));
            if self.packed_mips && bw.min(bh) <= 16 {
                break;
            }
            let (pitch, rows) = (bw.next_multiple_of(32), bh.next_multiple_of(32));
            out.push(Fetch {
                base: addr,
                width: (self.width >> level).max(1),
                height: (self.height >> level).max(1),
                pitch: pitch * block,
                mip_max: 0,
                mip_address: 0,
                ..*self
            });
            addr += (pitch * rows * bpp).next_multiple_of(4096);
        }
        out
    }

    /// Whether the fetched values need more than RGBA8 unsigned fractions:
    /// signed, biased or gamma channels, an exponent adjust, integer values,
    /// float formats or 16- and 32-bit channels. Those textures are converted
    /// to 16-bit floats instead ([`to_half_from`]).
    pub fn wide(&self) -> bool {
        self.signs != [0; 4] || self.exp_adjust != 0 || self.integer || matches!(self.format, 24..=26 | 30..=38)
    }

    /// The fields that change how texels convert, for cache keys.
    pub fn conversion(&self) -> u64 {
        let signs = self.signs.iter().enumerate().fold(0u64, |a, (i, &s)| a | (s as u64) << (2 * i));
        signs | ((self.exp_adjust as u8 as u64) << 8) | ((self.integer as u64) << 16) | ((self.swizzle as u64) << 17)
    }

    /// Reads slot `n` (0-31) of the device's fetch-constant shadow.
    pub fn from_device(dev: u32, n: u32) -> Option<Fetch> {
        let a = dev + 0x480 + 0x18 * n;
        Fetch::decode(core::array::from_fn(|i| mem::r32(a + 4 * i as u32)))
    }
}

/// Bytes per block and block size (1 for uncompressed, 4 for DXT) of a format.
pub fn format_layout(format: u32) -> Option<(u32, u32)> {
    Some(match format {
        2 | 8 | 9 => (1, 1),                                     // k_8, k_8_A, k_8_B
        3 | 4 | 5 | 10 | 15 | 24 | 30 => (2, 1),                 // 1_5_5_5, 5_6_5, 6_5_5, 8_8, 4_4_4_4, 16, 16_FLOAT
        6 | 7 | 14 | 16 | 17 | 25 | 31 | 33 | 36 | 50 | 54 | 55 | 56 => (4, 1), // 8_8_8_8, 2_10_10_10, ..., 16_16, 32, 32_FLOAT
        26 | 32 | 34 | 37 => (8, 1),                             // 16_16_16_16, 16x4 float, 32_32
        35 | 38 => (16, 1),                                      // 32_32_32_32 (float)
        18 | 51 | 58 | 59 | 60 => (8, 4),                        // DXT1, DXT3A, DXT5A, CTX1
        19 | 20 | 49 | 52 | 53 => (16, 4),                       // DXT2_3, DXT4_5, DXN
        _ => return None,
    })
}

pub fn format_name(format: u32) -> &'static str {
    match format {
        2 => "8",
        3 => "1_5_5_5",
        4 => "5_6_5",
        6 => "8_8_8_8",
        7 => "2_10_10_10",
        10 => "8_8",
        15 => "4_4_4_4",
        18 => "DXT1",
        19 => "DXT2_3",
        20 => "DXT4_5",
        24 => "16",
        25 => "16_16",
        26 => "16_16_16_16",
        31 => "16_16_FLOAT",
        32 => "16_16_16_16_FLOAT",
        36 => "32_FLOAT",
        49 => "DXN",
        50..=56 => "AS_16_16_16_16",
        58 => "DXT3A",
        59 => "DXT5A",
        60 => "CTX1",
        _ => "other",
    }
}

/// Offset in blocks of block `(x, y)` in a 2D tiled surface `pitch` blocks
/// wide, with `bpp` bytes per block. The console's 2D tiling: 32x32-block
/// macro tiles in row order, and a fixed swizzle within each.
pub fn tiled_offset_2d(x: u32, y: u32, pitch: u32, bpp: u32) -> u32 {
    let log_bpp = (bpp >> 2) + ((bpp >> 1) >> (bpp >> 2));
    let aligned = (pitch + 31) & !31;
    let macro_ = ((x >> 5) + (y >> 5) * (aligned >> 5)) << (log_bpp + 7);
    let micro = ((x & 7) + ((y & 6) << 2)) << log_bpp;
    let offset = macro_ + ((micro & !15) << 1) + (micro & 15) + ((y & 8) << (3 + log_bpp)) + ((y & 1) << 4);
    (((offset & !511) << 3) + ((offset & 448) << 2) + (offset & 63) + ((y & 16) << 7) + (((((y & 8) >> 2) + (x >> 3)) & 3) << 6))
        >> log_bpp
}

/// Byte swap of one element according to the fetch constant's endianness.
fn swap(bytes: &mut [u8], endian: u32) {
    match endian {
        1 => bytes.chunks_exact_mut(2).for_each(|c| c.swap(0, 1)),
        2 => bytes.chunks_exact_mut(4).for_each(|c| c.reverse()),
        3 => bytes.chunks_exact_mut(4).for_each(|c| {
            c.swap(0, 2);
            c.swap(1, 3);
        }),
        _ => {}
    }
}

/// Array layers of the base level: a cube map's six faces, else one.
pub fn layers(f: &Fetch) -> u32 {
    if f.dimension == 3 { 6 } else { 1 }
}

/// Bytes of one layer of the base level as stored, and the distance between
/// layers. A tiled layer covers whole 32x32-block macro tiles; layers (cube
/// faces) start on 4 KB boundaries.
fn layer_bytes(f: &Fetch) -> Option<(u32, u32)> {
    let (bpp, block) = format_layout(f.format)?;
    let bh = f.height.div_ceil(block);
    let pitch_blocks = f.pitch.max(f.width).div_ceil(block).max(f.width.div_ceil(block));
    let len = if f.tiled { bh.next_multiple_of(32) * pitch_blocks.next_multiple_of(32) * bpp } else { bh * pitch_blocks * bpp };
    Some((len, len.next_multiple_of(4096)))
}

/// The guest bytes holding the base level (every layer), as stored (tiled
/// or linear). `None` if the texture isn't in committed memory.
pub fn snapshot(f: &Fetch) -> Option<Vec<u8>> {
    let (virt, total) = guest_range(f)?;
    Some(mem::read(virt, total as usize))
}

/// Where `snapshot` reads: the guest virtual address and length of the base
/// level's bytes (every layer). `None` if the texture isn't in committed memory.
pub fn guest_range(f: &Fetch) -> Option<(u32, u32)> {
    let (len, stride) = layer_bytes(f)?;
    let total = stride * (layers(f) - 1) + len;
    let virt = physical_to_virtual(f.base)?;
    if mem::protect_of(virt.wrapping_add(total.saturating_sub(1))) == 0 {
        return None;
    }
    Some((virt, total))
}

/// The base level as linear rows of blocks (`bpp` bytes each, endian-corrected),
/// from the bytes `snapshot` returned.
pub fn untile(f: &Fetch, raw: &[u8]) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    let (bpp, block) = format_layout(f.format)?;
    let bw = f.width.div_ceil(block);
    let bh = f.height.div_ceil(block);
    let pitch_blocks = f.pitch.max(f.width).div_ceil(block).max(bw);
    let mut out = vec![0u8; (bw * bh * bpp) as usize];
    for y in 0..bh {
        for x in 0..bw {
            let src = (if f.tiled { tiled_offset_2d(x, y, pitch_blocks, bpp) } else { y * pitch_blocks + x } * bpp) as usize;
            let Some(s) = raw.get(src..src + bpp as usize) else { continue };
            let d = ((y * bw + x) * bpp) as usize;
            out[d..d + bpp as usize].copy_from_slice(s);
            swap(&mut out[d..d + bpp as usize], f.endian);
        }
    }
    Some((out, bw, bh, bpp, block))
}

/// The physical address a texture or resolve base names, whichever view
/// (`0xA0..`, `0xC0..`, `0xE0..` with its 4 KB offset) D3D took it from. Both
/// the capture and the renderer key resolved textures by it.
pub fn physical(base: u32) -> u32 {
    match base >> 29 {
        5 | 6 => base & 0x1FFF_FFFF,
        7 => base.wrapping_sub(0xE000_0000).wrapping_add(0x1000) & 0x1FFF_FFFF,
        _ => base,
    }
}

/// Guest virtual address of a texture base. D3D stores the CPU virtual address
/// of the allocation (the GPU uses only its low 29 bits); a bare physical
/// address is looked up through the physical views. `None` if nothing
/// committed holds it.
pub fn physical_to_virtual(phys: u32) -> Option<u32> {
    if mem::protect_of(phys) != 0 {
        return Some(phys);
    }
    let p = phys & 0x1FFF_FFFF;
    for v in [0xE000_0000u32.wrapping_add(p).wrapping_sub(0x1000), 0xA000_0000 + p, 0xC000_0000 + p] {
        if mem::protect_of(v) != 0 {
            return Some(v);
        }
    }
    None
}

/// Converts the base level to RGBA8 (`width * height * 4` bytes).
pub fn to_rgba(f: &Fetch) -> Option<Vec<u8>> {
    to_rgba_from(f, &snapshot(f)?)
}

/// Converts a snapshot of the base level to RGBA8, layer after layer (a cube
/// map's faces in D3D order: +X, -X, +Y, -Y, +Z, -Z).
/// Whether the renderer's device samples BC formats (set once it starts).
pub static BC_UPLOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The BC format a texture is uploaded in as it is stored (1 BC1, 2 BC2, 3
/// BC3, 5 BC5), when the host GPU's decoding gives what the console's does:
/// DXT1, DXT2/3, DXT4/5 and DXN, read with an identity swizzle as unsigned
/// fractions (no signs, gamma, exponent adjust or integers), with a base level
/// a whole number of blocks wide and high. Everything else is decoded to RGBA
/// on the CPU (`to_rgba_from`). Decoding a new scene's textures on the CPU
/// held the renderer up for a second at a time (2026-11-25).
pub fn bc_passthrough(f: &Fetch) -> Option<u8> {
    const IDENTITY: u32 = 3 << 9 | 2 << 6 | 1 << 3;
    if !BC_UPLOAD.load(std::sync::atomic::Ordering::Relaxed) || f.swizzle != IDENTITY || f.wide() || f.width % 4 != 0 || f.height % 4 != 0 {
        return None;
    }
    match f.format {
        18 => Some(1),
        19 => Some(2),
        20 => Some(3),
        49 => Some(5),
        _ => None,
    }
}

/// A BC-passthrough texture's blocks (`bc_passthrough`), every layer one after
/// another: untiled and endian-corrected, as BCn stores them.
pub fn to_bc_from(f: &Fetch, raw: &[u8]) -> Option<Vec<u8>> {
    let (_, stride) = layer_bytes(f)?;
    let mut out = Vec::new();
    for layer in 0..layers(f) {
        let start = (stride * layer) as usize;
        out.extend(untile(f, raw.get(start..)?)?.0);
    }
    Some(out)
}

pub fn to_rgba_from(f: &Fetch, raw: &[u8]) -> Option<Vec<u8>> {
    let (_, stride) = layer_bytes(f)?;
    let mut out = Vec::with_capacity((f.width * f.height * 4 * layers(f)) as usize);
    for layer in 0..layers(f) {
        let start = (stride * layer) as usize;
        out.extend(layer_to_rgba(f, raw.get(start..)?)?);
    }
    Some(out)
}

fn layer_to_rgba(f: &Fetch, raw: &[u8]) -> Option<Vec<u8>> {
    let (data, bw, bh, bpp, block) = untile(f, raw)?;
    let (w, h) = (f.width, f.height);
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut put = |x: u32, y: u32, p: [u8; 4]| {
        if x < w && y < h {
            let i = ((y * w + x) * 4) as usize;
            out[i..i + 4].copy_from_slice(&p);
        }
    };
    let at = |bx: u32, by: u32| &data[((by * bw + bx) * bpp) as usize..((by * bw + bx + 1) * bpp) as usize];
    // The fetch's channel swizzle picks R, G, B, A from the texel's X, Y, Z, W.
    let swizzle = |t: [u8; 4]| -> [u8; 4] {
        core::array::from_fn(|i| match (f.swizzle >> (3 * i)) & 7 {
            s @ 0..=3 => t[s as usize],
            5 => 255,
            _ => 0,
        })
    };
    if block == 4 {
        for by in 0..bh {
            for bx in 0..bw {
                let Some(px) = block_texels(f.format, at(bx, by)) else { return None };
                for (i, p) in px.iter().enumerate() {
                    put(bx * 4 + (i as u32 % 4), by * 4 + (i as u32 / 4), swizzle(*p));
                }
            }
        }
    } else {
        for y in 0..bh {
            for x in 0..bw {
                let Some(t) = texel(f.format, at(x, y)) else { return None };
                put(x, y, swizzle(t));
            }
        }
    }
    Some(out)
}

/// Converts a snapshot of the base level to RGBA 16-bit floats (little-endian,
/// `width * height * 8` bytes per layer), with the fetch constant's number
/// conversion applied: per-channel signed, biased or gamma, integer values,
/// the exponent adjust, and float formats kept beyond 0..1.
pub fn to_half_from(f: &Fetch, raw: &[u8]) -> Option<Vec<u8>> {
    let (_, stride) = layer_bytes(f)?;
    let mut out = Vec::with_capacity((f.width * f.height * 8 * layers(f)) as usize);
    for layer in 0..layers(f) {
        let start = (stride * layer) as usize;
        let (data, bw, bh, bpp, block) = untile(f, raw.get(start..)?)?;
        let (w, h) = (f.width, f.height);
        let mut px = vec![0u16; (w * h * 4) as usize];
        let scale = 2f32.powi(f.exp_adjust);
        let mut put = |x: u32, y: u32, t: [f32; 4]| {
            if x < w && y < h {
                let i = ((y * w + x) * 4) as usize;
                for c in 0..4 {
                    px[i + c] = match (f.swizzle >> (3 * c)) & 7 {
                        s @ 0..=3 => f16_bits(t[s as usize]),
                        5 => 0x3C00,
                        _ => 0,
                    };
                }
            }
        };
        let at = |bx: u32, by: u32| &data[((by * bw + bx) * bpp) as usize..((by * bw + bx + 1) * bpp) as usize];
        for by in 0..bh {
            for bx in 0..bw {
                if block == 4 {
                    let texels = block_texels_wide(f, at(bx, by), scale)?;
                    for (i, t) in texels.into_iter().enumerate() {
                        put(bx * 4 + (i as u32 % 4), by * 4 + (i as u32 / 4), t);
                    }
                } else {
                    put(bx, by, texel_wide(f, at(bx, by), scale)?);
                }
            }
        }
        out.extend(px.iter().flat_map(|v| v.to_le_bytes()));
    }
    Some(out)
}

/// One uncompressed texel's X, Y, Z, W as the shader receives them.
fn texel_wide(f: &Fetch, p: &[u8], scale: f32) -> Option<[f32; 4]> {
    let w32 = |i: usize| u32::from_le_bytes(p[4 * i..4 * i + 4].try_into().unwrap());
    let w16 = || u16::from_le_bytes([p[0], p[1]]);
    // Float formats: the values as stored; the sign modes don't apply.
    let floats: Option<[f32; 4]> = match f.format {
        30 => Some([half(w16()), 0.0, 0.0, 1.0]),
        31 => Some([half(w32(0) as u16), half((w32(0) >> 16) as u16), 0.0, 1.0]),
        32 => Some([half(w32(0) as u16), half((w32(0) >> 16) as u16), half(w32(1) as u16), half((w32(1) >> 16) as u16)]),
        36 => Some([f32::from_bits(w32(0)), 0.0, 0.0, 1.0]),
        37 => Some([f32::from_bits(w32(0)), f32::from_bits(w32(1)), 0.0, 1.0]),
        38 => Some(core::array::from_fn(|i| f32::from_bits(w32(i)))),
        _ => None,
    };
    if let Some(v) = floats {
        return Some(v.map(|x| x * scale));
    }
    let (v, bits) = fields(f.format, p)?;
    Some(core::array::from_fn(|c| match bits[c] {
        0 if c == 3 => 1.0,
        0 => 0.0,
        n => number(v[c], n, f.signs[c], f.integer) * scale,
    }))
}

/// An uncompressed texel's raw X, Y, Z, W fields and their widths in bits (0
/// for a component the format lacks).
fn fields(format: u32, p: &[u8]) -> Option<([u32; 4], [u32; 4])> {
    let w16 = || u16::from_le_bytes([p[0], p[1]]) as u32;
    let w32 = |i: usize| u32::from_le_bytes(p[4 * i..4 * i + 4].try_into().unwrap());
    let v = if p.len() >= 4 { w32(0) } else { 0 };
    Some(match format {
        2 | 8 | 9 => ([p[0] as u32, 0, 0, 0], [8, 0, 0, 0]),
        10 => ([p[0] as u32, p[1] as u32, 0, 0], [8, 8, 0, 0]),
        6 | 50 => (core::array::from_fn(|i| p[i] as u32), [8; 4]),
        4 => ([w16() & 31, (w16() >> 5) & 63, w16() >> 11, 0], [5, 6, 5, 0]),
        5 => ([w16() & 31, (w16() >> 5) & 31, w16() >> 10, 0], [5, 5, 6, 0]),
        3 => ([w16() & 31, (w16() >> 5) & 31, (w16() >> 10) & 31, w16() >> 15], [5, 5, 5, 1]),
        15 => (core::array::from_fn(|i| (w16() >> (4 * i)) & 15), [4; 4]),
        7 | 54 => ([v & 1023, (v >> 10) & 1023, (v >> 20) & 1023, v >> 30], [10, 10, 10, 2]),
        16 | 55 => ([v & 2047, (v >> 11) & 2047, v >> 22, 0], [11, 11, 10, 0]),
        17 | 56 => ([v & 1023, (v >> 10) & 2047, v >> 21, 0], [10, 11, 11, 0]),
        24 => ([w16(), 0, 0, 0], [16, 0, 0, 0]),
        25 => ([v & 0xFFFF, v >> 16, 0, 0], [16, 16, 0, 0]),
        26 => ([v & 0xFFFF, v >> 16, w32(1) & 0xFFFF, w32(1) >> 16], [16; 4]),
        33 => ([v, 0, 0, 0], [32, 0, 0, 0]),
        34 => ([v, w32(1), 0, 0], [32, 32, 0, 0]),
        35 => (core::array::from_fn(w32), [32; 4]),
        _ => return None,
    })
}

/// An `n`-bit field as the texture unit converts it: unsigned or signed
/// fraction, unsigned biased to -1..1, gamma to linear, or the integer.
fn number(v: u32, n: u32, sign: u8, integer: bool) -> f32 {
    let max = ((1u64 << n) - 1) as f64;
    match sign {
        1 if n >= 2 => {
            let s = ((v as i64) << (64 - n)) >> (64 - n);
            if integer { s as f32 } else { (s as f64 / ((1u64 << (n - 1)) - 1) as f64).max(-1.0) as f32 }
        }
        _ if integer => v as f32,
        2 => (v as f64 / max * 2.0 - 1.0) as f32,
        3 => gamma_to_linear((v as f64 / max) as f32),
        _ => (v as f64 / max) as f32,
    }
}

/// The texture unit's piecewise-linear gamma ramp, inverted: 8-bit gamma
/// 0-64, 64-96, 96-192 and 192-255 cover 10-bit linear 0-64, 64-128,
/// 128-512 and 512-1023 (slopes 1, 2, 4 and 8).
fn gamma_to_linear(g: f32) -> f32 {
    let g = g.clamp(0.0, 1.0) * 255.0;
    let l = if g < 64.0 {
        g
    } else if g < 96.0 {
        64.0 + (g - 64.0) * 2.0
    } else if g < 192.0 {
        128.0 + (g - 96.0) * 4.0
    } else {
        512.0 + (g - 192.0) * 8.0
    };
    l / 1023.0
}

/// A compressed block's 16 texels as the shader receives them. DXN and DXT5A
/// channels fetched as signed decode their endpoints as signed bytes.
fn block_texels_wide(f: &Fetch, b: &[u8], scale: f32) -> Option<[[f32; 4]; 16]> {
    let signed_bc4 = |b: &[u8]| -> [f32; 16] {
        let (a0, a1) = (b[0] as i8 as i32, b[1] as i8 as i32);
        let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
        core::array::from_fn(|i| {
            let k = ((bits >> (3 * i)) & 7) as i32;
            let v = match (k, a0 > a1) {
                (0, _) => a0,
                (1, _) => a1,
                (k, true) => (a0 * (8 - k) + a1 * (k - 1)) / 7,
                (6, false) => -127,
                (7, false) => 127,
                (k, false) => (a0 * (6 - k) + a1 * (k - 1)) / 5,
            };
            (v as f32 / 127.0).max(-1.0)
        })
    };
    match f.format {
        49 | 59 if f.signs.contains(&1) => {
            let unsigned = block_texels(f.format, b)?;
            let (x, y) = if f.format == 49 { (signed_bc4(&b[0..8]), signed_bc4(&b[8..16])) } else { (signed_bc4(&b[0..8]), signed_bc4(&b[0..8])) };
            Some(core::array::from_fn(|i| {
                core::array::from_fn(|c| {
                    let s = if f.format == 49 { [Some(x[i]), Some(y[i]), None, None][c] } else { Some(x[i]) };
                    match s {
                        Some(v) if f.signs[c] == 1 => v * scale,
                        _ if f.format == 49 && c == 2 => 0.0,
                        _ if f.format == 49 && c == 3 => 1.0,
                        _ => number(unsigned[i][c] as u32, 8, f.signs[c], f.integer) * scale,
                    }
                })
            }))
        }
        _ => block_texels(f.format, b).map(|px| {
            px.map(|t| {
                core::array::from_fn(|c| match (f.format, c) {
                    // DXN has no Z or W: they read 0 and 1.
                    (49, 2) => 0.0,
                    (49, 3) => 1.0,
                    _ => number(t[c] as u32, 8, f.signs[c], f.integer) * scale,
                })
            })
        }),
    }
}

/// A 32-bit float as a 16-bit float's bits (round to nearest; overflow to
/// infinity, tiny values to subnormals or zero).
fn f16_bits(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xFF) as i32;
    let man = b & 0x7F_FFFF;
    if exp == 0xFF {
        return sign | 0x7C00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 31 {
        return sign | 0x7C00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        return sign | ((m >> shift) + ((m >> (shift - 1)) & 1)) as u16;
    }
    let h = ((e as u32) << 10) | (man >> 13);
    sign | (h + ((man >> 12) & 1)).min(0x7C00) as u16
}

/// One uncompressed texel as X, Y, Z, W (X in the lowest bits of the word the
/// GPU reads, after the endian swap); missing components are 0, W 1.
fn texel(format: u32, p: &[u8]) -> Option<[u8; 4]> {
    let w16 = || u16::from_le_bytes([p[0], p[1]]) as u32;
    let w32 = |i: usize| u32::from_le_bytes(p[4 * i..4 * i + 4].try_into().unwrap());
    let n = |v: u32, bits: u32| (v * 255 / ((1 << bits) - 1)) as u8;
    let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let h = |v: u32| f(half(v as u16));
    Some(match format {
        2 | 8 | 9 => [p[0], 0, 0, 255],
        10 => [p[0], p[1], 0, 255],
        6 | 50 => [p[0], p[1], p[2], p[3]],
        4 => {
            let v = w16();
            [n(v & 31, 5), n((v >> 5) & 63, 6), n(v >> 11, 5), 255]
        }
        5 => {
            let v = w16();
            [n(v & 31, 5), n((v >> 5) & 31, 5), n(v >> 10, 6), 255]
        }
        3 => {
            let v = w16();
            [n(v & 31, 5), n((v >> 5) & 31, 5), n((v >> 10) & 31, 5), n(v >> 15, 1)]
        }
        15 => {
            let v = w16();
            [n(v & 15, 4), n((v >> 4) & 15, 4), n((v >> 8) & 15, 4), n(v >> 12, 4)]
        }
        7 | 54 => {
            let v = w32(0);
            [n(v & 1023, 10), n((v >> 10) & 1023, 10), n((v >> 20) & 1023, 10), n(v >> 30, 2)]
        }
        16 | 55 => {
            let v = w32(0);
            [n(v & 2047, 11), n((v >> 11) & 2047, 11), n(v >> 22, 10), 255]
        }
        17 | 56 => {
            let v = w32(0);
            [n(v & 1023, 10), n((v >> 10) & 2047, 11), n(v >> 21, 11), 255]
        }
        24 => [(w16() >> 8) as u8, 0, 0, 255],
        25 => {
            let v = w32(0);
            [(v >> 8) as u8, (v >> 24) as u8, 0, 255]
        }
        26 => {
            let (a, b) = (w32(0), w32(1));
            [(a >> 8) as u8, (a >> 24) as u8, (b >> 8) as u8, (b >> 24) as u8]
        }
        30 => [h(w16()), 0, 0, 255],
        31 => {
            let v = w32(0);
            [h(v & 0xFFFF), h(v >> 16), 0, 255]
        }
        32 => {
            let (a, b) = (w32(0), w32(1));
            [h(a & 0xFFFF), h(a >> 16), h(b & 0xFFFF), h(b >> 16)]
        }
        33 => [(w32(0) >> 24) as u8, 0, 0, 255],
        34 => [(w32(0) >> 24) as u8, (w32(1) >> 24) as u8, 0, 255],
        35 => core::array::from_fn(|i| (w32(i) >> 24) as u8),
        36 => [f(f32::from_bits(w32(0))), 0, 0, 255],
        37 => [f(f32::from_bits(w32(0))), f(f32::from_bits(w32(1))), 0, 255],
        38 => core::array::from_fn(|i| f(f32::from_bits(w32(i)))),
        _ => return None,
    })
}

/// A 16-bit float.
fn half(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 31) as u32, (h & 1023) as u32);
    let v = if e == 0 { m as f32 * 2f32.powi(-24) } else { f32::from_bits(((e + 112) << 23) | (m << 13)) };
    if s != 0 { -v } else { v }
}

/// The 16 texels of a 4x4 compressed block as X, Y, Z, W.
fn block_texels(format: u32, b: &[u8]) -> Option<[[u8; 4]; 16]> {
    Some(match format {
        18 | 51 => dxt_block(18, None, &b[0..8]),
        19 | 52 => dxt_block(19, Some(&b[0..8]), &b[8..16]),
        20 | 53 => dxt_block(20, Some(&b[0..8]), &b[8..16]),
        // DXN: two single-channel blocks, X then Y.
        49 => {
            let (x, y) = (bc4_block(&b[0..8]), bc4_block(&b[8..16]));
            core::array::from_fn(|i| [x[i], y[i], 0, 255])
        }
        // DXT5A: one channel, in every component.
        59 => bc4_block(&b[0..8]).map(|v| [v, v, v, v]),
        // DXT3A: 4-bit values, in every component.
        58 => core::array::from_fn(|i| {
            let v = ((b[i / 2] >> (4 * (i % 2))) & 15) * 17;
            [v, v, v, v]
        }),
        // CTX1: two 8:8 endpoints (X, Y) and 2-bit indices, four colours.
        60 => {
            let (e0, e1) = ([b[0] as u32, b[1] as u32], [b[2] as u32, b[3] as u32]);
            let bits = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            let mix = |a: u32, c: u32, k: u32| ((a * (3 - k) + c * k) / 3) as u8;
            core::array::from_fn(|i| {
                let k = match (bits >> (2 * i)) & 3 {
                    0 => 0,
                    1 => 3,
                    2 => 1,
                    _ => 2,
                };
                [mix(e0[0], e1[0], k), mix(e0[1], e1[1], k), 0, 255]
            })
        }
        _ => return None,
    })
}

/// A BC4 (DXT5 alpha) block: two endpoints and 3-bit indices.
fn bc4_block(b: &[u8]) -> [u8; 16] {
    let (a0, a1) = (b[0] as u32, b[1] as u32);
    let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
    core::array::from_fn(|i| {
        let k = ((bits >> (3 * i)) & 7) as u32;
        (match (k, a0 > a1) {
            (0, _) => a0,
            (1, _) => a1,
            (k, true) => (a0 * (8 - k) + a1 * (k - 1)) / 7,
            (6, false) => 0,
            (7, false) => 255,
            (k, false) => (a0 * (6 - k) + a1 * (k - 1)) / 5,
        }) as u8
    })
}

/// Decodes one DXT block (color data little-endian after the endian swap).
fn dxt_block(format: u32, alpha: Option<&[u8]>, color: &[u8]) -> [[u8; 4]; 16] {
    let c0 = u16::from_le_bytes([color[0], color[1]]);
    let c1 = u16::from_le_bytes([color[2], color[3]]);
    let rgb = |c: u16| [((c >> 11) & 31) as u32 * 255 / 31, ((c >> 5) & 63) as u32 * 255 / 63, (c & 31) as u32 * 255 / 31];
    let (a, b) = (rgb(c0), rgb(c1));
    let mix = |x: u32, y: u32, n: u32, d: u32| (x * (d - n) + y * n) / d;
    let palette: [[u32; 3]; 4] = if c0 > c1 || format != 18 {
        [a, b, core::array::from_fn(|i| mix(a[i], b[i], 1, 3)), core::array::from_fn(|i| mix(a[i], b[i], 2, 3))]
    } else {
        [a, b, core::array::from_fn(|i| (a[i] + b[i]) / 2), [0, 0, 0]]
    };
    let bits = u32::from_le_bytes([color[4], color[5], color[6], color[7]]);
    let mut out = [[0u8; 4]; 16];
    for (i, o) in out.iter_mut().enumerate() {
        let idx = ((bits >> (2 * i)) & 3) as usize;
        let p = palette[idx];
        let transparent = format == 18 && c0 <= c1 && idx == 3;
        *o = [p[0] as u8, p[1] as u8, p[2] as u8, if transparent { 0 } else { 255 }];
    }
    if let Some(al) = alpha {
        if format == 19 {
            for (i, o) in out.iter_mut().enumerate() {
                let n = (al[i / 2] >> (4 * (i % 2))) & 15;
                o[3] = n * 17;
            }
        } else {
            let (a0, a1) = (al[0] as u32, al[1] as u32);
            let bits = u64::from_le_bytes([al[2], al[3], al[4], al[5], al[6], al[7], 0, 0]);
            for (i, o) in out.iter_mut().enumerate() {
                let k = ((bits >> (3 * i)) & 7) as u32;
                o[3] = match (k, a0 > a1) {
                    (0, _) => a0,
                    (1, _) => a1,
                    (k, true) => (a0 * (8 - k) + a1 * (k - 1)) / 7,
                    (6, false) => 0,
                    (7, false) => 255,
                    (k, false) => (a0 * (6 - k) + a1 * (k - 1)) / 5,
                } as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiling_is_a_permutation_of_each_macro_tile() {
        for bpp in [1u32, 2, 4, 8, 16] {
            let (w, h) = (64u32, 64u32);
            let mut seen = std::collections::HashSet::new();
            for y in 0..h {
                for x in 0..w {
                    let o = tiled_offset_2d(x, y, w, bpp);
                    assert!(o < w * h, "bpp {bpp}: ({x},{y}) -> {o}");
                    assert!(seen.insert(o), "bpp {bpp}: ({x},{y}) collides at {o}");
                }
            }
        }
    }

    #[test]
    fn fetch_constant_fields() {
        // Tiled 2D DXT1 texture, 256x128, pitch 256, base 0x12345000, 8-in-16.
        let w = [2 | (8 << 22) | (1 << 31), 18 | (1 << 6) | 0x1234_5000, 255 | (127 << 13), 0, 0, 1 << 9];
        let f = Fetch::decode(w).unwrap();
        assert_eq!((f.width, f.height, f.pitch, f.format, f.endian, f.base, f.tiled, f.dimension), (256, 128, 256, 18, 1, 0x1234_5000, true, 1));
    }

    fn linear_8888(signs: [u8; 4], exp_adjust: i32) -> Fetch {
        Fetch {
            tiled: false,
            pitch: 32,
            format: 6,
            endian: 0,
            base: 0,
            width: 1,
            height: 1,
            dimension: 1,
            swizzle: 0o3210,
            signs,
            exp_adjust,
            integer: false,
            mip_max: 0,
            packed_mips: false,
            mip_address: 0,
        }
    }

    fn halves(bytes: &[u8]) -> Vec<f32> {
        bytes.chunks_exact(2).map(|c| half(u16::from_le_bytes([c[0], c[1]]))).collect()
    }

    #[test]
    fn fetch_constant_number_conversion_fields() {
        // X signed, Y biased, Z gamma, W unsigned; exponent adjust -3; integer.
        let w = [2 | (1 << 2) | (2 << 4) | (3 << 6), 0x1000 | 6, 0, 1 | (0x3D << 13), 0, 1 << 9];
        let f = Fetch::decode(w).unwrap();
        assert_eq!((f.signs, f.exp_adjust, f.integer), ([1, 2, 3, 0], -3, true));
        assert!(f.wide());
        assert!(!linear_8888([0; 4], 0).wide());
    }

    #[test]
    fn signed_biased_and_scaled_channels() {
        let mut raw = vec![0u8; 4096];
        // X = -127 (signed -1), Y = 0x80 (biased ~0), Z = 255 (gamma 1 -> ~0.993), W = 255.
        raw[..4].copy_from_slice(&[0x81, 0x80, 0xFF, 0xFF]);
        let v = halves(&to_half_from(&linear_8888([1, 2, 3, 0], 0), &raw).unwrap());
        assert_eq!(v[0], -1.0);
        assert!(v[1].abs() < 0.01, "biased {}", v[1]);
        assert!((v[2] - 1016.0 / 1023.0).abs() < 0.002, "gamma {}", v[2]);
        assert_eq!(v[3], 1.0);
        // -128 clamps to -1; exponent adjust +2 scales by 4.
        raw[..4].copy_from_slice(&[0x80, 0x40, 0, 0]);
        let v = halves(&to_half_from(&linear_8888([1, 0, 0, 0], 2), &raw).unwrap());
        assert_eq!(v[0], -4.0);
        assert!((v[1] - 4.0 * 64.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn half_floats_round_trip() {
        for x in [0.0f32, 1.0, -2.5, 65504.0, 0.000061035156, 1.0e-6, 3.14159] {
            let back = half(f16_bits(x));
            assert!((back - x).abs() <= x.abs() * 0.001 + 1.0e-7, "{x} -> {back}");
        }
        assert_eq!(f16_bits(1.0e6), 0x7C00);
    }

    #[test]
    fn signed_dxn_decodes_signed_endpoints() {
        // One 4x4 DXN block; X endpoints -127 and 127 (every texel index 0 -> -1),
        // Y endpoints 0 and 0.
        let mut raw = vec![0u8; 4096];
        raw[0] = 0x81;
        raw[1] = 0x7F;
        let f = Fetch { format: 49, width: 4, height: 4, signs: [1, 1, 0, 0], ..linear_8888([0; 4], 0) };
        let v = halves(&to_half_from(&f, &raw).unwrap());
        assert_eq!(&v[0..4], &[-1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn mip_levels_follow_from_the_mip_address() {
        // Tiled 2048x1024 DXT1 (512x256 blocks, 8 bytes each), packed mips, 11 levels.
        let f = Fetch {
            tiled: true,
            pitch: 2048,
            format: 18,
            base: 0x1000_0000,
            width: 2048,
            height: 1024,
            mip_max: 11,
            packed_mips: true,
            mip_address: 0x1100_0000,
            ..linear_8888([0; 4], 0)
        };
        let levels = f.mip_levels();
        // Levels 1-3 (256x128, 128x64, 64x32 blocks); level 4 (32x16 blocks) is in the packed tail.
        let sizes: Vec<(u32, u32)> = levels.iter().map(|l| (l.width, l.height)).collect();
        assert_eq!(sizes, [(1024, 512), (512, 256), (256, 128)]);
        let bases: Vec<u32> = levels.iter().map(|l| l.base).collect();
        assert_eq!(bases, [0x1100_0000, 0x1100_0000 + 256 * 128 * 8, 0x1100_0000 + 256 * 128 * 8 + 128 * 64 * 8]);
        assert!(levels.iter().all(|l| l.tiled && l.mip_address == 0));
        // No mip address, or not tiled: no levels.
        assert!(Fetch { mip_address: 0, ..f }.mip_levels().is_empty());
        assert!(Fetch { tiled: false, ..f }.mip_levels().is_empty());
    }

    #[test]
    fn cube_faces_follow_each_other_on_4k_boundaries() {
        // Linear 8x8 8_8_8_8 cube: each face is 32 texels (the minimum pitch) x 8 rows x 4 bytes = 1 KB, so faces are 4 KB apart.
        let f = Fetch {
            tiled: false,
            pitch: 32,
            format: 6,
            endian: 0,
            base: 0,
            width: 8,
            height: 8,
            dimension: 3,
            swizzle: 0o3210,
            signs: [0; 4],
            exp_adjust: 0,
            integer: false,
            mip_max: 0,
            packed_mips: false,
            mip_address: 0,
        };
        assert_eq!(layers(&f), 6);
        assert_eq!(layer_bytes(&f), Some((1024, 4096)));
        let mut raw = vec![0u8; 5 * 4096 + 1024];
        for face in 0..6 {
            raw[face * 4096] = face as u8 + 1;
        }
        let rgba = to_rgba_from(&f, &raw).unwrap();
        assert_eq!(rgba.len(), 8 * 8 * 4 * 6);
        for face in 0..6 {
            assert_eq!(rgba[face * 8 * 8 * 4], face as u8 + 1, "face {face}");
        }
    }
}
