//! Read-only view of the executable image for analysis.

use ppc::{Instr, Op};
use xex::Xex;

pub struct Code<'a> {
    pub xex: &'a Xex,
    /// `[begin, end)` of each code section, sorted.
    pub ranges: Vec<(u32, u32)>,
}

impl<'a> Code<'a> {
    pub fn new(xex: &'a Xex) -> Self {
        let mut ranges: Vec<(u32, u32)> = xex.code_sections().map(|s| (s.address, s.end())).collect();
        ranges.sort_unstable();
        Code { xex, ranges }
    }

    /// Big-endian word at any image address.
    pub fn word(&self, addr: u32) -> Option<u32> {
        self.xex.bytes_at(addr, 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }

    pub fn u16(&self, addr: u32) -> Option<u16> {
        self.xex.bytes_at(addr, 2).map(|b| u16::from_be_bytes(b.try_into().unwrap()))
    }

    pub fn u8(&self, addr: u32) -> Option<u8> {
        self.xex.bytes_at(addr, 1).map(|b| b[0])
    }

    pub fn is_code(&self, addr: u32) -> bool {
        addr % 4 == 0 && self.ranges.iter().any(|&(b, e)| (b..e).contains(&addr))
    }

    pub fn instr(&self, addr: u32) -> Option<Instr> {
        if !self.is_code(addr) {
            return None;
        }
        Instr::decode(addr, self.word(addr)?)
    }

    /// Every (address, word) in the code sections.
    pub fn words(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.ranges.iter().flat_map(move |&(b, e)| {
            (b..e).step_by(4).map(move |a| (a, self.word(a).unwrap_or(0)))
        })
    }
}

/// `BO` field says "branch always" (don't test CR, don't decrement CTR).
pub fn bo_always(i: &Instr) -> bool {
    i.rt() & 0b10100 == 0b10100
}

/// General-purpose registers an instruction writes, from the table's operand list.
pub fn gprs_written(i: &Instr) -> impl Iterator<Item = u32> + '_ {
    i.op.info().writes.iter().filter_map(move |&(field, _)| match field.name() {
        "RD" | "RT" => Some(i.rt()),
        "RA" => Some(i.ra()),
        _ => None,
    })
}

pub fn is_call(i: &Instr) -> bool {
    matches!(i.op, Op::bx | Op::bcx | Op::bclrx | Op::bcctrx) && i.lk()
}

/// Ends straight-line execution: unconditional branch, return or CTR jump without link.
pub fn is_unconditional_transfer(i: &Instr) -> bool {
    match i.op {
        Op::bx => !i.lk(),
        Op::bcx | Op::bclrx | Op::bcctrx => !i.lk() && bo_always(i),
        _ => false,
    }
}

pub fn is_branch(i: &Instr) -> bool {
    matches!(i.op, Op::bx | Op::bcx | Op::bclrx | Op::bcctrx)
}

/// `mtspr` to CTR (SPR 9).
pub fn is_mtctr(i: &Instr) -> bool {
    i.op == Op::mtspr && i.spr() == 9
}
