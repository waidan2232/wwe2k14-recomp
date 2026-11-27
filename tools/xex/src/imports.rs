//! Import libraries (`xboxkrnl.exe`, `xam.xex`, …) and their resolved records.
//!
//! Each record names a guest address holding a big-endian word:
//! `type << 24 | ordinal` (the middle byte is a hint). Type 0 is an import-address
//! slot: a variable, or a function's pointer slot. Type 1 is a function thunk
//! (code). So every imported function contributes a type 0 and a type 1 record,
//! and a variable contributes only a type 0 record.

use std::collections::BTreeSet;

use crate::bytes::{be16, be32, latin1, slice};
use crate::exports;
use crate::headers::Version;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// Import-address slot (variable, or a function's pointer).
    Slot,
    /// Function thunk in code.
    Thunk,
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportRecord {
    /// Guest address of the slot or thunk.
    pub address: u32,
    pub kind: RecordKind,
    pub ordinal: u16,
    pub name: Option<&'static str>,
}

impl ImportRecord {
    pub fn display_name(&self) -> String {
        self.name.map_or_else(|| format!("ordinal_0x{:04X}", self.ordinal), str::to_owned)
    }
}

#[derive(Debug, Clone)]
pub struct ImportLibrary {
    pub name: String,
    pub id: u32,
    pub version: Version,
    pub min_version: Version,
    pub records: Vec<ImportRecord>,
}

impl ImportLibrary {
    /// Imported functions, one record (the thunk) per function.
    pub fn functions(&self) -> impl Iterator<Item = &ImportRecord> {
        self.records.iter().filter(|r| r.kind == RecordKind::Thunk)
    }

    /// Imported variables: slots whose ordinal has no thunk.
    pub fn variables(&self) -> Vec<&ImportRecord> {
        let functions: BTreeSet<u16> = self.functions().map(|r| r.ordinal).collect();
        self.records
            .iter()
            .filter(|r| r.kind == RecordKind::Slot && !functions.contains(&r.ordinal))
            .collect()
    }
}

/// Parses the import-libraries optional header (`data` includes its size word)
/// and resolves each record against the decrypted `image` loaded at `base`.
pub(crate) fn parse(data: &[u8], image: &[u8], base: u32) -> Result<Vec<ImportLibrary>> {
    let what = "import header";
    let string_table_size = be32(data, 4, what)? as usize;
    let string_count = be32(data, 8, what)? as usize;
    let strings_raw = slice(data, 12, string_table_size, "import string table")?;

    let mut strings = Vec::with_capacity(string_count);
    let mut off = 0usize;
    while strings.len() < string_count && off < strings_raw.len() {
        let s = latin1(&strings_raw[off..]);
        off += (s.len() + 1 + 3) & !3;
        strings.push(s);
    }
    if strings.len() != string_count {
        return Err(Error::Invalid(format!(
            "import string table holds {} names, header says {string_count}",
            strings.len()
        )));
    }

    let mut libraries = Vec::new();
    let mut off = 12 + string_table_size;
    while off + 40 <= data.len() {
        let what = "import library";
        let size = be32(data, off, what)? as usize;
        let id = be32(data, off + 24, what)?;
        let version = Version::from_u32(be32(data, off + 28, what)?);
        let min_version = Version::from_u32(be32(data, off + 32, what)?);
        let name_index = be16(data, off + 36, what)? as usize;
        let count = be16(data, off + 38, what)? as usize;
        if size < 40 + count * 4 {
            return Err(Error::Invalid(format!("import library size {size} too small for {count} records")));
        }
        let name = strings
            .get(name_index)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("import library name index {name_index} out of range")))?;

        let mut records = Vec::with_capacity(count);
        for i in 0..count {
            let address = be32(data, off + 40 + i * 4, "import record")?;
            let image_offset = address
                .checked_sub(base)
                .ok_or_else(|| Error::Invalid(format!("import record 0x{address:08X} is below the image base")))?;
            let value = be32(image, image_offset as usize, "import thunk data")?;
            let ordinal = (value & 0xFFFF) as u16;
            records.push(ImportRecord {
                address,
                kind: match value >> 24 {
                    0 => RecordKind::Slot,
                    1 => RecordKind::Thunk,
                    n => RecordKind::Other(n as u8),
                },
                ordinal,
                name: exports::lookup(&name, ordinal as u32),
            });
        }
        libraries.push(ImportLibrary { name, id, version, min_version, records });
        off += size;
    }
    Ok(libraries)
}
