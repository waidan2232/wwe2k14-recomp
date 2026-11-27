//! Instruction decoder for the Xbox 360's Xenon CPU: 64-bit PowerPC, the FPU,
//! VMX (AltiVec) and the 360's VMX128 extension (128 vector registers).
//!
//! The opcode table is generated at build time from `data/ppc-instructions.xml`.
//! That file is instruction-set data from the Xenia project (BSD-3-Clause; see
//! THIRD_PARTY_NOTICES.md). This crate only decodes and prints instructions.
//! It never executes them.
//!
//! Bit positions in this crate's helpers are LSB-based (bit 0 = least significant).
//! PowerPC documentation numbers bits from the MSB, so IBM bit `n` is LSB bit `31 - n`.

use std::fmt;

pub mod asm;

include!(concat!(env!("OUT_DIR"), "/table.rs"));

/// Static description of one instruction.
#[derive(Debug)]
pub struct OpInfo {
    pub op: Op,
    /// Mnemonic as written in the table (`addx` covers `add`, `add.`, `addo`, `addo.`).
    pub mnemonic: &'static str,
    pub form: Form,
    pub group: Group,
    pub desc: &'static str,
    /// Canonical encoding (operand fields zero).
    pub opcode: u32,
    /// Bits that identify the instruction: the primary opcode plus the form's extended opcode.
    pub mask: u32,
    /// `opcode & mask`.
    pub value: u32,
    /// Changes control flow or machine state in a way that ends a basic block.
    pub sync: bool,
    /// Operands and implicit state read, with a flag for "only conditionally".
    pub reads: &'static [(Field, bool)],
    /// Operands and implicit state written, with a flag for "only conditionally".
    pub writes: &'static [(Field, bool)],
    /// Disassembly template with `[FIELD]` placeholders.
    pub disasm: Option<&'static str>,
}

impl Op {
    pub fn info(self) -> &'static OpInfo {
        &OPS[self as usize]
    }

    pub fn mnemonic(self) -> &'static str {
        self.info().mnemonic
    }

    pub fn form(self) -> Form {
        self.info().form
    }

    pub fn all() -> impl Iterator<Item = Op> {
        OPS.iter().map(|i| i.op)
    }

    pub fn is_vmx128(self) -> bool {
        matches!(
            self.form(),
            Form::VX128
                | Form::VX128_U
                | Form::VX128_1
                | Form::VX128_2
                | Form::VX128_3
                | Form::VX128_4
                | Form::VX128_5
                | Form::VX128_P
                | Form::VX128_R
        )
    }
}

/// Identifies the instruction encoded by `word`, or `None` if no table entry matches.
pub fn decode(word: u32) -> Option<Op> {
    candidates(word).next()
}

/// Every table entry whose encoding matches `word`, most specific first. More than
/// one result means the table is ambiguous for this word.
pub fn decode_all(word: u32) -> impl Iterator<Item = Op> {
    candidates(word)
}

fn candidates(word: u32) -> impl Iterator<Item = Op> {
    BY_PRIMARY[(word >> 26) as usize]
        .iter()
        .map(|&i| &OPS[i as usize])
        .filter(move |info| word & info.mask == info.value)
        .map(|info| info.op)
}

fn sext(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((value as i64) << shift) >> shift
}

/// A decoded instruction at a guest address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instr {
    pub address: u32,
    pub word: u32,
    pub op: Op,
}

impl Instr {
    pub fn decode(address: u32, word: u32) -> Option<Instr> {
        decode(word).map(|op| Instr { address, word, op })
    }

    /// `len` bits starting at LSB bit `lo`.
    #[inline]
    pub fn bits(&self, lo: u32, len: u32) -> u32 {
        (self.word >> lo) & ((1u32 << len) - 1)
    }

    #[inline]
    pub fn bit(&self, lo: u32) -> bool {
        (self.word >> lo) & 1 != 0
    }

    /// Bits 21-25: RT/RD/RS/FT/VD (non-VMX128), BO, CRBD, TO.
    pub fn rt(&self) -> u32 {
        self.bits(21, 5)
    }

    /// Bits 16-20: RA/FA/VA (non-VMX128), BI, CRBA.
    pub fn ra(&self) -> u32 {
        self.bits(16, 5)
    }

    /// Bits 11-15: RB/FB/VB (non-VMX128), CRBB.
    pub fn rb(&self) -> u32 {
        self.bits(11, 5)
    }

    /// Sign-extended 16-bit immediate (D form).
    pub fn simm16(&self) -> i32 {
        self.bits(0, 16) as u16 as i16 as i32
    }

    pub fn uimm16(&self) -> u32 {
        self.bits(0, 16)
    }

    /// Vector destination/source register, 0-127 for VMX128 forms, 0-31 otherwise.
    pub fn vd(&self) -> u32 {
        if self.op.is_vmx128() { self.bits(21, 5) | (self.bits(2, 2) << 5) } else { self.rt() }
    }

    pub fn va(&self) -> u32 {
        if self.op.is_vmx128() {
            self.bits(16, 5) | ((self.bit(5) as u32) << 5) | ((self.bit(10) as u32) << 6)
        } else {
            self.ra()
        }
    }

    pub fn vb(&self) -> u32 {
        if self.op.is_vmx128() { self.bits(11, 5) | (self.bits(0, 2) << 5) } else { self.rb() }
    }

    /// Record bit (`.` suffix): updates CR0 (integer), CR1 (FPU) or CR6 (vector compares).
    pub fn rc(&self) -> bool {
        match self.op.form() {
            Form::X | Form::XO | Form::A | Form::M | Form::MD | Form::MDS | Form::XS | Form::XFL => self.bit(0),
            Form::VC => self.bit(10),
            Form::VX128_R => self.bit(6),
            _ => false,
        }
    }

    pub fn lk(&self) -> bool {
        matches!(self.op.form(), Form::I | Form::B | Form::XL) && self.bit(0)
    }

    /// Target of a direct branch (`b`, `bl`, `bc`, …), resolving relative offsets.
    pub fn branch_target(&self) -> Option<u32> {
        let (offset, absolute) = match self.op.form() {
            Form::I => (sext(self.bits(2, 24) << 2, 26), self.bit(1)),
            Form::B => (sext(self.bits(2, 14) << 2, 16), self.bit(1)),
            _ => return None,
        };
        let base = if absolute { 0 } else { self.address as i64 };
        Some((base + offset) as u32)
    }

    /// SPR number for `mfspr`/`mtspr`/`mftb` (the encoded halves are swapped).
    pub fn spr(&self) -> u32 {
        let raw = self.bits(11, 10);
        ((raw & 0x1F) << 5) | (raw >> 5)
    }

    /// Value of an operand by its table name (as used in `OpInfo::reads`/`writes`
    /// and disassembly templates), interpreted according to this instruction's form.
    /// Returns `None` for implicit state (CR, CA, LR, …) and unknown names.
    pub fn operand(&self, name: &str) -> Option<i64> {
        let form = self.op.form();
        let rb = self.rb() as i64;
        Some(match name {
            "RT" | "RD" | "RS" | "FT" | "FD" | "FS" | "TO" | "BO" | "CRBD" | "FPSCRD" => self.rt() as i64,
            "RA" | "RA0" | "FA" | "BI" | "CRBA" => self.ra() as i64,
            "RB" | "FB" | "CRBB" => rb,
            "VD" | "VS" | "VT" => self.vd() as i64,
            "VA" => self.va() as i64,
            "VB" => self.vb() as i64,
            "FC" | "RC" => self.bits(6, 5) as i64,
            "VC" => (if form == Form::VX128_2 { self.bits(6, 3) } else { self.bits(6, 5) }) as i64,
            "d" => self.simm16() as i64,
            "ds" => sext(self.bits(2, 14) << 2, 16),
            "SIMM" => match form {
                Form::VX => sext(self.ra(), 5),
                Form::VX128_3 => sext(self.bits(16, 5), 5),
                _ => self.simm16() as i64,
            },
            "UIMM" => match form {
                Form::VX | Form::VX128_3 => self.bits(16, 5) as i64,
                Form::VX128_P => (self.bits(16, 5) | (self.bits(6, 3) << 5)) as i64,
                _ => self.uimm16() as i64,
            },
            "CRFD" => self.bits(23, 3) as i64,
            "CRFS" => self.bits(18, 3) as i64,
            "L" => (if form == Form::XFL { self.bit(25) } else { self.bit(21) }) as i64,
            "L15" => self.bit(16) as i64,
            "SPR" | "TBR" => self.spr() as i64,
            "CRM" => self.bits(12, 8) as i64,
            "FM" => self.bits(17, 8) as i64,
            "SH" => match form {
                Form::XS | Form::MD => rb | ((self.bit(1) as i64) << 5),
                Form::VX128_5 => self.bits(6, 4) as i64,
                _ => rb,
            },
            "SHB" => self.bits(6, 4) as i64,
            "MB" => match form {
                Form::M => self.bits(6, 5) as i64,
                Form::MD | Form::MDS => (self.bits(6, 5) | ((self.bit(5) as u32) << 5)) as i64,
                _ => return None,
            },
            "ME" => match form {
                Form::M => self.bits(1, 5) as i64,
                Form::MD | Form::MDS => (self.bits(6, 5) | ((self.bit(5) as u32) << 5)) as i64,
                _ => return None,
            },
            "IMM" => (if form == Form::VX128_4 { self.bits(16, 5) } else { self.rb() }) as i64,
            "z" => self.bits(6, 2) as i64,
            "LEV" => self.bits(5, 7) as i64,
            "LK" => self.bit(0) as i64,
            "AA" => self.bit(1) as i64,
            "OE" => self.bit(10) as i64,
            "Rc" => self.rc() as i64,
            "ADDR" => self.branch_target()? as i64,
            _ => return None,
        })
    }

    pub fn field(&self, field: Field) -> Option<i64> {
        self.operand(field.name())
    }

    fn write_token(&self, f: &mut fmt::Formatter<'_>, token: &str) -> fmt::Result {
        fn flag(bit: bool, s: &'static str) -> &'static str {
            if bit { s } else { "" }
        }
        match token {
            "Rc" => f.write_str(flag(self.rc(), ".")),
            "OE" => f.write_str(flag(self.bit(10), "o")),
            "LK" => f.write_str(flag(self.bit(0), "l")),
            "AA" => f.write_str(flag(self.bit(1), "a")),
            _ => {
                let Some(v) = self.operand(token) else { return write!(f, "<{token}?>") };
                match token {
                    "RA" | "RA0" | "RB" | "RC" | "RT" | "RS" | "RD" => write!(f, "r{v}"),
                    "FA" | "FB" | "FC" | "FT" | "FS" | "FD" => write!(f, "f{v}"),
                    "VA" | "VB" | "VC" | "VT" | "VS" | "VD" => write!(f, "v{v}"),
                    "CRFD" | "CRFS" => write!(f, "cr{v}"),
                    "UIMM" | "ADDR" => write!(f, "0x{v:X}"),
                    "d" | "ds" | "SIMM" if v < 0 => write!(f, "-0x{:X}", -v),
                    "d" | "ds" | "SIMM" => write!(f, "0x{v:X}"),
                    "LEV" if v == 0 => Ok(()),
                    _ => write!(f, "{v}"),
                }
            }
        }
    }
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(template) = self.op.info().disasm else {
            return write!(f, "{} <0x{:08X}>", self.op.mnemonic(), self.word);
        };
        let mut rest = template;
        while let Some(open) = rest.find('[') {
            f.write_str(&rest[..open])?;
            let Some(len) = rest[open..].find(']') else { break };
            self.write_token(f, &rest[open + 1..open + len])?;
            rest = &rest[open + len + 1..];
        }
        f.write_str(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dis(address: u32, word: u32) -> String {
        Instr::decode(address, word).map(|i| i.to_string()).unwrap_or_else(|| "<invalid>".into())
    }

    #[test]
    fn canonical_encodings_decode_to_themselves() {
        let wrong: Vec<String> = OPS
            .iter()
            .filter(|info| decode(info.opcode) != Some(info.op))
            .map(|info| format!("{} 0x{:08X} -> {:?}", info.mnemonic, info.opcode, decode(info.opcode)))
            .collect();
        assert!(wrong.is_empty(), "canonical encodings that decode wrongly:\n{}", wrong.join("\n"));
    }

    #[test]
    fn common_integer_and_branch_forms() {
        assert_eq!(dis(0, 0x7C08_02A6), "mfspr r0, 8"); // mflr r0
        assert_eq!(dis(0, 0x7C09_03A6), "mtspr 9, r0"); // mtctr r0
        assert_eq!(decode(0x4E80_0020), Some(Op::bclrx)); // blr
        assert_eq!(decode(0x4E80_0420), Some(Op::bcctrx)); // bctr
        assert_eq!(dis(0, 0x3860_0001), "addi r3, r0, 0x1"); // li r3, 1
        assert_eq!(dis(0, 0xE9C1_FF68), "ld r14, -0x98(r1)");
        assert_eq!(dis(0, 0xF9C1_FF68), "std r14, -0x98(r1)");
        assert_eq!(decode(0x7C00_07EC), Some(Op::dcbz));
        assert_eq!(decode(0x7C20_07EC), Some(Op::dcbz128));

        let bl = Instr::decode(0x8200_0010, 0x4BFF_FFF1).unwrap(); // bl -0x10
        assert_eq!(bl.op, Op::bx);
        assert!(bl.lk());
        assert_eq!(bl.branch_target(), Some(0x8200_0000));
        let fwd = Instr::decode(0x8200_0000, 0x4800_0101).unwrap(); // bl +0x100
        assert_eq!(fwd.branch_target(), Some(0x8200_0100));
        let beq = Instr::decode(0x8200_0100, 0x4182_FFF0).unwrap(); // beq cr0, -0x10
        assert_eq!(beq.op, Op::bcx);
        assert_eq!(beq.branch_target(), Some(0x8200_00F0));
    }

    #[test]
    fn vmx128_register_fields() {
        // The __restvmx_64 helper's first load: lvx128 v64, r11, r12
        let i = Instr::decode(0, 0x100B_60CB).unwrap();
        assert_eq!(i.op, Op::lvx128);
        assert_eq!((i.vd(), i.ra(), i.rb()), (64, 11, 12));
        assert_eq!(i.to_string(), "lvx128 v64, r11, r12");
        assert_eq!(decode(0x100B_61CB), Some(Op::stvx128));
        // __restvmx_14's lvx v14, r11, r12 is plain VMX.
        assert_eq!(dis(0, 0x7DCB_60CE), "lvx v14, r11, r12");
        // vmsum3fp128 v127, v127, v127 sets every VMX128 register bit.
        let word = 0x1400_0190 | (31 << 21) | (31 << 16) | (31 << 11) | (3 << 2) | 3 | (1 << 5) | (1 << 10);
        let i = Instr::decode(0, word).unwrap();
        assert_eq!(i.op, Op::vmsum3fp128);
        assert_eq!((i.vd(), i.va(), i.vb()), (127, 127, 127));
    }

    #[test]
    fn field_names_round_trip() {
        for info in OPS.iter() {
            for &(field, _) in info.reads.iter().chain(info.writes) {
                assert!(!field.name().is_empty());
            }
        }
    }
}
