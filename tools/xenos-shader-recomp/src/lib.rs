//! Offline Xenos shader translation (PLAN §2.4 item 6).
//!
//! Xenos microcode is translated to WGSL at build time; nothing here runs in
//! the game. A shader the translator can't handle is an error naming the
//! shader and the unsupported feature, fixed here rather than worked around at
//! runtime.

pub mod ucode;
pub mod wgsl;

pub use wgsl::{Reflection, Stage, Translation, translate};

/// Parses and validates generated WGSL with naga, returning a readable error.
#[cfg(feature = "tool")]
pub fn validate(src: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(src).map_err(|e| e.emit_to_string(src))?;
    let mut v = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default());
    v.validate(&module).map_err(|e| e.emit_to_string(src))?;
    Ok(())
}

/// A compiled shader found in data.
pub struct Container {
    /// Offset of the container.
    pub at: usize,
    pub stage: Stage,
    /// The microcode's bytes in the data.
    pub code: std::ops::Range<usize>,
    /// Literal constants (`def`): stage-relative register and value (as bits).
    pub literals: Vec<(u32, [u32; 4])>,
}

/// Compiled shader containers (the XDK's format). Header words:
/// +0x00 magic 0x102A1100 (pixel) / 0x102A1101 (vertex), +0x04 virtual
/// (header) size, +0x08 physical size, +0x10 constant table offset,
/// +0x14 definition table offset, +0x18 shader record offset. The physical
/// part follows the virtual part; the shader record's +0x00 is the
/// microcode's offset in the physical part and +0x04 its size.
///
/// Literal constants: D3D loads them into the constant registers when the
/// shader is set, from the physical part. The definition table: 16 bytes, a
/// section size, then 8-byte entries of (register: u16, float count: u16)
/// and the values' byte offset in the physical part; registers 256 and up
/// are the pixel bank, and empty entries pad the table. The microcode follows
/// the literals.
pub fn containers(data: &[u8]) -> Vec<Container> {
    let word = |o: usize| data.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    for (at, stage, code) in find_containers(data) {
        let (virt, def) = (word(at + 4).unwrap() as usize, word(at + 0x14).unwrap() as usize);
        let mut literals = Vec::new();
        if def != 0 && def + 0x14 <= virt {
            let size = word(at + def + 0x10).unwrap_or(0) as usize;
            let physical = at + virt;
            let mut e = at + def + 0x14;
            while e + 8 <= at + def + 0x10 + size.min(virt) {
                let entry = word(e).unwrap_or(0);
                let offset = word(e + 4).unwrap_or(0) as usize;
                let (reg, count) = (entry >> 16, entry & 0xFFFF);
                for k in 0..count / 4 {
                    let v: [u32; 4] = core::array::from_fn(|i| word(physical + offset + 16 * k as usize + 4 * i).unwrap_or(0));
                    literals.push(((reg & 0xFF) + k, v));
                }
                e += 8;
            }
        }
        out.push(Container { at, stage, code, literals });
    }
    out
}

/// Container positions only (see [`containers`]).
pub fn find_containers(data: &[u8]) -> Vec<(usize, Stage, std::ops::Range<usize>)> {
    let word = |o: usize| data.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    let mut o = 0;
    while o + 0x24 <= data.len() {
        let stage = match word(o) {
            Some(0x102A_1100) => Some(Stage::Pixel),
            Some(0x102A_1101) => Some(Stage::Vertex),
            _ => None,
        };
        if let Some(stage) = stage {
            let (virt, phys, rec) = (word(o + 4).unwrap() as usize, word(o + 8).unwrap() as usize, word(o + 0x18).unwrap() as usize);
            if let (Some(off), Some(size)) = (word(o + rec), word(o + rec + 4))
                && rec >= 0x24
                && rec + 8 <= virt
                && size > 0
                && size % 12 == 0
                && (off + size) as usize <= phys
            {
                let start = o + virt + off as usize;
                if start + size as usize <= data.len() {
                    out.push((o, stage, start..start + size as usize));
                }
            }
        }
        o += 4;
    }
    out
}
