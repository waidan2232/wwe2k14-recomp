//! PE structures inside the decrypted image. PE headers are little-endian even on
//! the 360. `.pdata` entries are big-endian like the rest of the guest data.

use crate::bytes::{be32, latin1, le16, le32, slice};
use crate::{Error, Result};

pub const MACHINE_POWERPC_BE: u16 = 0x01F2;
pub const SCN_CNT_CODE: u32 = 0x0000_0020;
pub const SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
pub const SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
pub const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const SCN_MEM_READ: u32 = 0x4000_0000;
pub const SCN_MEM_WRITE: u32 = 0x8000_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    /// Guest virtual address (image base + RVA).
    pub address: u32,
    /// Offset into the image buffer.
    pub rva: u32,
    pub virtual_size: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

impl Section {
    pub fn is_code(&self) -> bool {
        self.characteristics & SCN_CNT_CODE != 0
    }

    pub fn is_writable(&self) -> bool {
        self.characteristics & SCN_MEM_WRITE != 0
    }

    pub fn end(&self) -> u32 {
        self.address.saturating_add(self.virtual_size)
    }

    pub fn contains(&self, addr: u32) -> bool {
        (self.address..self.end()).contains(&addr)
    }
}

pub fn parse_sections(image: &[u8], base: u32) -> Result<Vec<Section>> {
    if !image.starts_with(b"MZ") {
        return Err(Error::Invalid("decrypted image has no MZ header".into()));
    }
    let nt = le32(image, 0x3C, "e_lfanew")? as usize;
    if slice(image, nt, 4, "PE signature")? != b"PE\0\0" {
        return Err(Error::Invalid("missing PE signature".into()));
    }
    let machine = le16(image, nt + 4, "PE machine")?;
    if machine != MACHINE_POWERPC_BE {
        return Err(Error::Invalid(format!("PE machine 0x{machine:04X} is not PowerPC big-endian (0x01F2)")));
    }
    let count = le16(image, nt + 6, "PE section count")? as usize;
    let optional_size = le16(image, nt + 20, "PE optional header size")? as usize;
    let table = nt + 24 + optional_size;
    (0..count)
        .map(|i| {
            let o = table + i * 40;
            let what = "PE section header";
            let rva = le32(image, o + 12, what)?;
            Ok(Section {
                name: latin1(slice(image, o, 8, what)?),
                address: base.wrapping_add(rva),
                rva,
                virtual_size: le32(image, o + 8, what)?,
                raw_size: le32(image, o + 16, what)?,
                characteristics: le32(image, o + 36, what)?,
            })
        })
        .collect()
}

/// One `.pdata` record: a function's start and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin: u32,
    /// Length in bytes.
    pub length: u32,
    pub prolog_instructions: u8,
    pub is_32bit: bool,
    pub has_exception_handler: bool,
}

impl RuntimeFunction {
    pub fn end(&self) -> u32 {
        self.begin.saturating_add(self.length)
    }
}

/// Parses `.pdata`. Records with a zero start address (padding) are skipped.
pub fn parse_pdata(image: &[u8], section: &Section) -> Result<Vec<RuntimeFunction>> {
    let data = slice(image, section.rva as usize, section.virtual_size as usize, ".pdata")?;
    let mut out = Vec::with_capacity(data.len() / 8);
    for o in (0..data.len() / 8 * 8).step_by(8) {
        let begin = be32(data, o, ".pdata entry")?;
        if begin == 0 {
            continue;
        }
        // Bitfield, from the low end: PrologLength:8, FunctionLength:22, ThirtyTwoBit:1, ExceptionFlag:1.
        let info = be32(data, o + 4, ".pdata entry")?;
        out.push(RuntimeFunction {
            begin,
            length: ((info >> 8) & 0x3F_FFFF) * 4,
            prolog_instructions: (info & 0xFF) as u8,
            is_32bit: (info >> 30) & 1 != 0,
            has_exception_handler: info >> 31 != 0,
        });
    }
    Ok(out)
}
