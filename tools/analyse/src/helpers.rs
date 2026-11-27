//! Compiler-generated register save/restore helpers (`__savegprlr_14` and friends).
//!
//! Each helper is one routine with an entry point per register, so
//! `__savegprlr_20` is `__savegprlr_14 + 6 * 4`. The recompiler needs the base
//! addresses. These are found by their first instructions, the same byte
//! patterns XenonRecomp documents, and checked by confirming each ends in `blr`.

use crate::code::Code;

const BLR: u32 = 0x4E80_0020;

#[derive(Clone, Copy, Debug)]
pub struct HelperKind {
    pub name: &'static str,
    pattern: &'static [u8],
    /// Bytes per register entry.
    stride: u32,
    first_reg: u32,
    last_reg: u32,
    /// Bytes after the last register's entry, including the final `blr`.
    tail: u32,
}

pub const KINDS: [HelperKind; 8] = [
    HelperKind { name: "restgprlr_14", pattern: &[0xE9, 0xC1, 0xFF, 0x68], stride: 4, first_reg: 14, last_reg: 31, tail: 12 },
    HelperKind { name: "savegprlr_14", pattern: &[0xF9, 0xC1, 0xFF, 0x68], stride: 4, first_reg: 14, last_reg: 31, tail: 8 },
    HelperKind { name: "restfpr_14", pattern: &[0xC9, 0xCC, 0xFF, 0x70], stride: 4, first_reg: 14, last_reg: 31, tail: 4 },
    HelperKind { name: "savefpr_14", pattern: &[0xD9, 0xCC, 0xFF, 0x70], stride: 4, first_reg: 14, last_reg: 31, tail: 4 },
    HelperKind { name: "restvmx_14", pattern: &[0x39, 0x60, 0xFE, 0xE0, 0x7D, 0xCB, 0x60, 0xCE], stride: 8, first_reg: 14, last_reg: 31, tail: 4 },
    HelperKind { name: "savevmx_14", pattern: &[0x39, 0x60, 0xFE, 0xE0, 0x7D, 0xCB, 0x61, 0xCE], stride: 8, first_reg: 14, last_reg: 31, tail: 4 },
    HelperKind { name: "restvmx_64", pattern: &[0x39, 0x60, 0xFC, 0x00, 0x10, 0x0B, 0x60, 0xCB], stride: 8, first_reg: 64, last_reg: 127, tail: 4 },
    HelperKind { name: "savevmx_64", pattern: &[0x39, 0x60, 0xFC, 0x00, 0x10, 0x0B, 0x61, 0xCB], stride: 8, first_reg: 64, last_reg: 127, tail: 4 },
];

#[derive(Clone, Copy, Debug)]
pub struct Helper {
    pub kind: HelperKind,
    pub address: u32,
}

impl Helper {
    pub fn end(&self) -> u32 {
        self.address + (self.kind.last_reg - self.kind.first_reg + 1) * self.kind.stride + self.kind.tail
    }

    /// Entry point that handles registers `reg..=last`.
    pub fn entry(&self, reg: u32) -> u32 {
        self.address + (reg - self.kind.first_reg) * self.kind.stride
    }

    pub fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        (self.kind.first_reg..=self.kind.last_reg).map(|r| (r, self.entry(r)))
    }
}

/// Finds every helper. Errors if a pattern is missing, ambiguous, or doesn't end in `blr`.
pub fn find(code: &Code) -> Result<Vec<Helper>, String> {
    let mut out = Vec::new();
    for kind in KINDS {
        let hits: Vec<u32> = code
            .words()
            .map(|(addr, _)| addr)
            .filter(|&addr| code.xex.bytes_at(addr, kind.pattern.len()) == Some(kind.pattern))
            .collect();
        let [address] = hits.as_slice() else {
            return Err(format!("{}: expected one match of its byte pattern, found {}", kind.name, hits.len()));
        };
        let helper = Helper { kind, address: *address };
        let last = helper.end() - 4;
        if code.word(last) != Some(BLR) {
            return Err(format!("{} at 0x{address:08X}: expected blr at 0x{last:08X}", kind.name));
        }
        out.push(helper);
    }
    Ok(out)
}
