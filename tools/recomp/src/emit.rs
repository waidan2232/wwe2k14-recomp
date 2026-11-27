//! Translation of one guest function into Rust source.
//!
//! Each function becomes `unsafe extern "C" fn sub_XXXXXXXX(c: &mut Ctx, b: *mut u8)`.
//! Straight-line functions are emitted as plain code. Functions with internal
//! branches become a block-dispatch loop, `loop { match pc { ... } }`, where each
//! arm is a basic block and ends by assigning the next block's address. LLVM
//! threads constant `pc` assignments into direct branches.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use ppc::{Instr, Op};

use crate::locals::{self, Plan};
use crate::regs::{self, RegSet};

/// What the emitter needs to know about the rest of the program.
pub struct Program<'a> {
    /// Guest addresses that have a callable recompiled function.
    pub entries: &'a BTreeSet<u32>,
    /// `bctr` address -> (switch register, labels, default).
    pub switches: &'a HashMap<u32, (u32, Vec<u32>, Option<u32>)>,
    pub word: &'a dyn Fn(u32) -> Option<u32>,
    /// `setjmp` entry points: each call to one is a point `longjmp` can resume.
    pub setjmp: &'a BTreeSet<u32>,
    /// `blr`s that switch to a saved stack pointer (`longjmp`'s exit). Each
    /// becomes a native unwind to the frame that called `setjmp`.
    pub longjmp_returns: &'a BTreeSet<u32>,
    /// Entry points of the compiler's register save/restore helpers
    /// (`__savegprlr_14` and friends), with the address each runs to: straight
    /// code ending in `blr`. A call or branch to one is translated in place
    /// (the helper's instructions, in the caller), so the caller's saves and
    /// restores are visible to the register analysis and cost no call.
    pub inline: &'a BTreeMap<u32, u32>,
}

pub struct FnDef {
    pub begin: u32,
    pub end: u32,
    /// Embedded data ranges to skip (inline switch tables).
    pub skip: Vec<(u32, u32)>,
    /// The last block falls into the next function: emit a tail call to `end`.
    pub falls_through: bool,
}

pub struct Emitted {
    pub source: String,
    /// Functions this one calls directly (for extern declarations).
    pub callees: BTreeSet<u32>,
    pub instructions: usize,
    pub todo: usize,
    /// Mnemonics of instructions emitted as `todo_instr`.
    pub todo_ops: Vec<&'static str>,
}

pub fn fn_name(addr: u32) -> String {
    format!("sub_{addr:08X}")
}

fn bo_cond(i: &Instr) -> String {
    // BO (IBM bits 6-10): 0x10 = ignore CR, 0x08 = CR bit value to test,
    // 0x04 = don't touch CTR, 0x02 = branch if CTR == 0 (else != 0).
    let bo = i.rt();
    let bi = i.ra();
    let mut parts = Vec::new();
    if bo & 0x04 == 0 {
        parts.push(if bo & 0x02 != 0 { "{ c.ctr = c.ctr.wrapping_sub(1); c.ctr as u32 == 0 }".to_string() } else {
            "{ c.ctr = c.ctr.wrapping_sub(1); c.ctr as u32 != 0 }".to_string()
        });
    }
    if bo & 0x10 == 0 {
        let field = ["lt", "gt", "eq", "so"][(bi % 4) as usize];
        let test = format!("c.cr[{}].{field}", bi / 4);
        parts.push(if bo & 0x08 != 0 { test } else { format!("!{test}") });
    }
    if parts.is_empty() { "true".to_string() } else { parts.join(" && ") }
}

/// Where a call or tail call goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A guest address known at build time.
    Direct(u32),
    /// The address in CTR (`bctr`, `bctrl`).
    Ctr,
    /// The address in LR (`blrl`).
    Lr,
}

/// One step of a block.
#[derive(Clone, Debug)]
pub enum Item {
    /// Straight-line code for the instruction at `addr`: Rust statements over
    /// the register context. `instr` is the instruction, when the code is its
    /// translation (not a diagnostic or the link-register update of an inlined call).
    Code { addr: u32, lines: Vec<String>, instr: Option<Instr> },
    /// A call that returns to `lr` (`bl`, `bcl`, `bctrl`, `blrl`), under `cond`
    /// if it is conditional. `setjmp`: the callee is `setjmp`, so `lr` is also
    /// where a `longjmp` resumes.
    Call { addr: u32, target: Target, cond: Option<String>, lr: u32, setjmp: bool },
    /// Leaves the function if `cond` holds (`beqlr`, a conditional branch to
    /// another function); otherwise the block goes on.
    CondExit { addr: u32, cond: String, exit: Exit },
}

/// How control leaves the function.
#[derive(Clone, Debug)]
pub enum Exit {
    /// `blr`.
    Return,
    /// A branch to another function, or through CTR: a tail call.
    Tail(Target),
    /// `longjmp`'s final `blr`: a native unwind to the frame that called `setjmp`.
    Longjmp,
    /// The function's last block runs into the next function: a tail call to it.
    FallInto(u32),
    /// Execution ran past the last instruction (a fatal error at run time).
    FellOff(u32),
}

/// How a block ends.
#[derive(Clone, Debug)]
pub enum Term {
    /// Continues at another block of the function.
    Jump(u32),
    /// After a call to `setjmp`: continues at the resume point (the call's
    /// line already sets it).
    Resume(u32),
    Branch { cond: String, taken: u32, next: u32 },
    /// `bctr` through a switch table: the selector register picks a label.
    Switch { reg: u32, labels: Vec<u32>, default: Option<u32> },
    Exit(Exit),
}

/// A basic block: straight-line items, then how it ends.
#[derive(Clone, Debug)]
pub struct Block {
    pub start: u32,
    pub items: Vec<Item>,
    pub term: Term,
}

/// One guest function as blocks, before it becomes Rust text.
#[derive(Clone, Debug)]
pub struct Body {
    pub begin: u32,
    pub end: u32,
    /// One block of straight-line code: emitted without the dispatch loop.
    pub single: bool,
    pub blocks: Vec<Block>,
    /// Return addresses of this function's calls to `setjmp`.
    pub resume_sites: Vec<u32>,
    pub instructions: usize,
    pub todo_ops: Vec<&'static str>,
}

/// Emits one non-branch instruction as Rust statements. `None` if it isn't
/// implemented yet.
fn emit_op(i: &Instr) -> Option<Vec<String>> {
    let (d, a, rb) = (i.rt(), i.ra(), i.rb());
    let simm = i.simm16();
    let uimm = i.uimm16();
    let rc = i.rc();
    // Effective addresses: (rA|0) + d and (rA|0) + rB.
    let ea_d = |a: u32| if a == 0 { format!("0x{:08X}u32", simm as u32) } else { format!("ea(c.r[{a}], 0x{:08X})", simm as u32) };
    let ea_ds = |a: u32| {
        let ds = i.operand("ds").unwrap() as i32 as u32;
        if a == 0 { format!("0x{ds:08X}u32") } else { format!("ea(c.r[{a}], 0x{ds:08X})") }
    };
    let ea_x = |a: u32| if a == 0 { format!("(c.r[{rb}] as u32)") } else { format!("ea(c.r[{a}], c.r[{rb}] as u32)") };
    // The 360 runs code in 32-bit mode: record forms compare the low word, even
    // for doubleword instructions (`divdu.` of 0xFFFFFFFF sets LT).
    let cr0 = |reg: u32| format!("c.cr[0] = cmp_s(c.r[{reg}] as i32 as i64, 0, c.xer_so);");
    let mut s: Vec<String> = Vec::new();
    // Vector record forms set CR6 in their own arm.
    let vector_record = matches!(i.op.info().group, ppc::Group::v);
    // OE forms (`addo`, ...) also update XER[OV] and XER[SO], which isn't
    // translated yet. Leave them untranslated rather than wrong.
    if i.op.form() == ppc::Form::XO && i.bit(10) {
        return None;
    }
    use Op::*;
    match i.op {
        // ---- integer arithmetic
        addi => s.push(if a == 0 { format!("c.r[{d}] = 0x{:X}u64;", simm as i64 as u64) } else {
            format!("c.r[{d}] = c.r[{a}].wrapping_add(0x{:X}u64);", simm as i64 as u64)
        }),
        addis => s.push(if a == 0 { format!("c.r[{d}] = 0x{:X}u64;", ((simm as i64) << 16) as u64) } else {
            format!("c.r[{d}] = c.r[{a}].wrapping_add(0x{:X}u64);", ((simm as i64) << 16) as u64)
        }),
        addx => s.push(format!("c.r[{d}] = c.r[{a}].wrapping_add(c.r[{rb}]);")),
        // A register combined with itself gives a value that doesn't depend on
        // it (the compiler's ways to make 0, -1 or a mask from the carry).
        // Written as the constant, so the register isn't read: to the register
        // analysis a read is a use of whatever the caller left there.
        subfx if a == rb => s.push(format!("c.r[{d}] = 0;")),
        subfx => s.push(format!("c.r[{d}] = c.r[{rb}].wrapping_sub(c.r[{a}]);")),
        negx => s.push(format!("c.r[{d}] = c.r[{a}].wrapping_neg();")),
        addic | addicx => s.push(format!(
            "{{ let (v, ca) = (c.r[{a}] as u32).overflowing_add({}); c.xer_ca = ca; c.r[{d}] = c.r[{a}].wrapping_add(0x{:X}u64); let _ = v; }}",
            simm as u32, simm as i64
        )),
        addcx => s.push(format!(
            "{{ c.xer_ca = (c.r[{a}] as u32).overflowing_add(c.r[{rb}] as u32).1; c.r[{d}] = c.r[{a}].wrapping_add(c.r[{rb}]); }}"
        )),
        addex => s.push(format!(
            "{{ let ci = c.xer_ca as u64; c.xer_ca = (c.r[{a}] as u32 as u64 + c.r[{rb}] as u32 as u64 + ci) >> 32 != 0; c.r[{d}] = c.r[{a}].wrapping_add(c.r[{rb}]).wrapping_add(ci); }}"
        )),
        addzex => s.push(format!(
            "{{ let ci = c.xer_ca as u64; c.xer_ca = (c.r[{a}] as u32 as u64 + ci) >> 32 != 0; c.r[{d}] = c.r[{a}].wrapping_add(ci); }}"
        )),
        addmex => s.push(format!(
            "{{ let ci = c.xer_ca as u64; c.xer_ca = (c.r[{a}] as u32 as u64 + ci + 0xFFFF_FFFF) >> 32 != 0; c.r[{d}] = c.r[{a}].wrapping_add(ci).wrapping_sub(1); }}"
        )),
        subfcx => s.push(format!(
            "{{ c.xer_ca = (c.r[{rb}] as u32) >= (c.r[{a}] as u32); c.r[{d}] = c.r[{rb}].wrapping_sub(c.r[{a}]); }}"
        )),
        // subfe rD, rA, rA: ~x + x + CA is -1 + CA, and the carry out is CA again.
        subfex if a == rb => s.push(format!("c.r[{d}] = (c.xer_ca as u64).wrapping_sub(1);")),
        subfex => s.push(format!(
            "{{ let ci = c.xer_ca as u64; c.xer_ca = ((!c.r[{a}]) as u32 as u64 + c.r[{rb}] as u32 as u64 + ci) >> 32 != 0; c.r[{d}] = (!c.r[{a}]).wrapping_add(c.r[{rb}]).wrapping_add(ci); }}"
        )),
        subfzex => s.push(format!(
            "{{ let ci = c.xer_ca as u64; c.xer_ca = ((!c.r[{a}]) as u32 as u64 + ci) >> 32 != 0; c.r[{d}] = (!c.r[{a}]).wrapping_add(ci); }}"
        )),
        subfmex => s.push(format!(
            "{{ let ci = c.xer_ca as u64; c.xer_ca = ((!c.r[{a}]) as u32 as u64 + ci + 0xFFFF_FFFF) >> 32 != 0; c.r[{d}] = (!c.r[{a}]).wrapping_add(ci).wrapping_sub(1); }}"
        )),
        mulhdx => s.push(format!("c.r[{d}] = (((c.r[{a}] as i64 as i128) * (c.r[{rb}] as i64 as i128)) >> 64) as u64;")),
        mulhdux => s.push(format!("c.r[{d}] = ((c.r[{a}] as u128 * c.r[{rb}] as u128) >> 64) as u64;")),
        // subfic rD, rA, -1 is MSVC's NOT idiom: ~rA + 0xFFFFFFFF + 1 always carries.
        subficx if simm == -1 => s.push(format!("{{ c.xer_ca = true; c.r[{d}] = !c.r[{a}]; }}")),
        subficx => s.push(format!(
            "{{ c.xer_ca = (c.r[{a}] as u32) <= {}u32; c.r[{d}] = 0x{:X}u64.wrapping_sub(c.r[{a}]); }}",
            simm as u32, simm as i64
        )),
        mullwx => s.push(format!("c.r[{d}] = (c.r[{a}] as i32 as i64).wrapping_mul(c.r[{rb}] as i32 as i64) as u64;")),
        mulli => s.push(format!("c.r[{d}] = (c.r[{a}] as i64).wrapping_mul({}) as u64;", simm as i64)),
        mulhwx => s.push(format!("c.r[{d}] = (((c.r[{a}] as i32 as i64) * (c.r[{rb}] as i32 as i64)) >> 32) as u64;")),
        mulhwux => s.push(format!("c.r[{d}] = ((c.r[{a}] as u32 as u64 * c.r[{rb}] as u32 as u64) >> 32) as u64;")),
        mulldx => s.push(format!("c.r[{d}] = c.r[{a}].wrapping_mul(c.r[{rb}]);")),
        divwx => s.push(format!(
            "{{ let (n, m) = (c.r[{a}] as i32, c.r[{rb}] as i32); c.r[{d}] = if m == 0 || (n == i32::MIN && m == -1) {{ 0 }} else {{ (n / m) as u32 as u64 }}; }}"
        )),
        divwux => s.push(format!(
            "{{ let (n, m) = (c.r[{a}] as u32, c.r[{rb}] as u32); c.r[{d}] = if m == 0 {{ 0 }} else {{ (n / m) as u64 }}; }}"
        )),
        divdx => s.push(format!(
            "{{ let (n, m) = (c.r[{a}] as i64, c.r[{rb}] as i64); c.r[{d}] = if m == 0 || (n == i64::MIN && m == -1) {{ 0 }} else {{ (n / m) as u64 }}; }}"
        )),
        divdux => s.push(format!(
            "{{ let (n, m) = (c.r[{a}], c.r[{rb}]); c.r[{d}] = if m == 0 {{ 0 }} else {{ n / m }}; }}"
        )),
        // ---- logical (destination is RA, source RS = bits 21-25)
        andx => s.push(format!("c.r[{a}] = c.r[{d}] & c.r[{rb}];")),
        andcx if d == rb => s.push(format!("c.r[{a}] = 0;")),
        andcx => s.push(format!("c.r[{a}] = c.r[{d}] & !c.r[{rb}];")),
        // `or rX, rX, rX` changes nothing: the compiler's padding and the CPU's
        // priority hints. `or rA, rS, rS` is `mr`.
        orx if d == rb && d == a => {}
        orx if d == rb => s.push(format!("c.r[{a}] = c.r[{d}];")),
        orx => s.push(format!("c.r[{a}] = c.r[{d}] | c.r[{rb}];")),
        orcx => s.push(format!("c.r[{a}] = c.r[{d}] | !c.r[{rb}];")),
        xorx if d == rb => s.push(format!("c.r[{a}] = 0;")),
        xorx => s.push(format!("c.r[{a}] = c.r[{d}] ^ c.r[{rb}];")),
        norx => s.push(format!("c.r[{a}] = !(c.r[{d}] | c.r[{rb}]);")),
        nandx => s.push(format!("c.r[{a}] = !(c.r[{d}] & c.r[{rb}]);")),
        eqvx if d == rb => s.push(format!("c.r[{a}] = u64::MAX;")),
        eqvx => s.push(format!("c.r[{a}] = !(c.r[{d}] ^ c.r[{rb}]);")),
        andix => s.push(format!("c.r[{a}] = c.r[{d}] & 0x{uimm:X}; {}", cr0(a))),
        andisx => s.push(format!("c.r[{a}] = c.r[{d}] & 0x{:X}; {}", (uimm as u64) << 16, cr0(a))),
        ori => s.push(if d == a && uimm == 0 { String::new() } else { format!("c.r[{a}] = c.r[{d}] | 0x{uimm:X};") }),
        oris => s.push(format!("c.r[{a}] = c.r[{d}] | 0x{:X};", (uimm as u64) << 16)),
        xori => s.push(format!("c.r[{a}] = c.r[{d}] ^ 0x{uimm:X};")),
        xoris => s.push(format!("c.r[{a}] = c.r[{d}] ^ 0x{:X};", (uimm as u64) << 16)),
        extsbx => s.push(format!("c.r[{a}] = c.r[{d}] as i8 as i64 as u64;")),
        extshx => s.push(format!("c.r[{a}] = c.r[{d}] as i16 as i64 as u64;")),
        extswx => s.push(format!("c.r[{a}] = c.r[{d}] as i32 as i64 as u64;")),
        cntlzwx => s.push(format!("c.r[{a}] = (c.r[{d}] as u32).leading_zeros() as u64;")),
        cntlzdx => s.push(format!("c.r[{a}] = c.r[{d}].leading_zeros() as u64;")),
        // ---- rotates and shifts
        rlwinmx | rlwimix => {
            let (sh, mb, me) = (rb, i.bits(6, 5), i.bits(1, 5));
            let m = mask_expr(mb + 32, me + 32);
            s.push(if i.op == rlwinmx {
                format!("c.r[{a}] = rotl32(c.r[{d}], {sh}) & {m};")
            } else {
                format!("c.r[{a}] = (rotl32(c.r[{d}], {sh}) & {m}) | (c.r[{a}] & !{m});")
            });
        }
        rlwnmx => {
            let (mb, me) = (i.bits(6, 5), i.bits(1, 5));
            s.push(format!("c.r[{a}] = rotl32(c.r[{d}], (c.r[{rb}] & 31) as u32) & {};", mask_expr(mb + 32, me + 32)));
        }
        rldiclx | rldicrx | rldicx | rldimix => {
            let sh = i.operand("SH").unwrap() as u32;
            let mb = i.operand("MB").unwrap() as u32;
            let rot = format!("c.r[{d}].rotate_left({sh})");
            s.push(match i.op {
                rldiclx => format!("c.r[{a}] = {rot} & {};", mask_expr(mb, 63)),
                rldicrx => format!("c.r[{a}] = {rot} & {};", mask_expr(0, mb)),
                rldicx => format!("c.r[{a}] = {rot} & {};", mask_expr(mb, 63 - sh)),
                _ => {
                    let m = mask_expr(mb, 63 - sh);
                    format!("c.r[{a}] = ({rot} & {m}) | (c.r[{a}] & !{m});")
                }
            });
        }
        slwx => s.push(format!("{{ let n = c.r[{rb}] & 63; c.r[{a}] = if n >= 32 {{ 0 }} else {{ ((c.r[{d}] as u32) << n) as u64 }}; }}")),
        srwx => s.push(format!("{{ let n = c.r[{rb}] & 63; c.r[{a}] = if n >= 32 {{ 0 }} else {{ ((c.r[{d}] as u32) >> n) as u64 }}; }}")),
        srawx => s.push(format!(
            "{{ let n = (c.r[{rb}] & 63) as u32; let v = c.r[{d}] as i32; let (r, lost) = if n >= 32 {{ (v >> 31, v != 0) }} else {{ (v >> n, n > 0 && (v as u32) << (32 - n) != 0) }}; c.xer_ca = v < 0 && lost; c.r[{a}] = r as i64 as u64; }}"
        )),
        srawix => {
            let n = rb;
            s.push(if n == 0 {
                format!("{{ c.xer_ca = false; c.r[{a}] = c.r[{d}] as i32 as i64 as u64; }}")
            } else {
                format!(
                    "{{ let v = c.r[{d}] as i32; c.xer_ca = v < 0 && (v as u32) << {} != 0; c.r[{a}] = (v >> {n}) as i64 as u64; }}",
                    32 - n
                )
            });
        }
        sradx => s.push(format!(
            "{{ let n = (c.r[{rb}] & 127) as u32; let v = c.r[{d}] as i64; let (r, lost) = if n >= 64 {{ (v >> 63, v != 0) }} else {{ (v >> n, n > 0 && (v as u64) << (64 - n) != 0) }}; c.xer_ca = v < 0 && lost; c.r[{a}] = r as u64; }}"
        )),
        sldx => s.push(format!("{{ let n = c.r[{rb}] & 127; c.r[{a}] = if n >= 64 {{ 0 }} else {{ c.r[{d}] << n }}; }}")),
        srdx => s.push(format!("{{ let n = c.r[{rb}] & 127; c.r[{a}] = if n >= 64 {{ 0 }} else {{ c.r[{d}] >> n }}; }}")),
        sradix => {
            let n = i.operand("SH").unwrap() as u32;
            s.push(if n == 0 {
                format!("{{ c.xer_ca = false; c.r[{a}] = c.r[{d}]; }}")
            } else {
                format!(
                    "{{ let v = c.r[{d}] as i64; c.xer_ca = v < 0 && (v as u64) << {} != 0; c.r[{a}] = (v >> {n}) as u64; }}",
                    64 - n
                )
            });
        }
        // ---- compares
        cmpi | cmp | cmpli | cmpl => {
            let crf = i.bits(23, 3);
            let wide = i.bit(21);
            let rhs_reg = format!("c.r[{rb}]");
            let (lhs, rhs) = match (i.op, wide) {
                (cmpi, false) => (format!("c.r[{a}] as i32 as i64"), format!("{}", simm as i64)),
                (cmpi, true) => (format!("c.r[{a}] as i64"), format!("{}", simm as i64)),
                (cmp, false) => (format!("c.r[{a}] as i32 as i64"), format!("{rhs_reg} as i32 as i64")),
                (cmp, true) => (format!("c.r[{a}] as i64"), format!("{rhs_reg} as i64")),
                (cmpli, false) => (format!("c.r[{a}] as u32 as u64"), format!("{uimm}")),
                (cmpli, true) => (format!("c.r[{a}]"), format!("{uimm}")),
                (_, false) => (format!("c.r[{a}] as u32 as u64"), format!("{rhs_reg} as u32 as u64")),
                (_, true) => (format!("c.r[{a}]"), rhs_reg),
            };
            let f = if matches!(i.op, cmpi | cmp) { "cmp_s" } else { "cmp_u" };
            s.push(format!("c.cr[{crf}] = {f}({lhs}, {rhs}, c.xer_so);"));
        }
        // ---- loads
        lbz | lhz | lha | lwz | lbzu | lhzu | lhau | lwzu => {
            let update = matches!(i.op, lbzu | lhzu | lhau | lwzu);
            let ea = if update { format!("ea(c.r[{a}], 0x{:08X})", simm as u32) } else { ea_d(a) };
            let val = match i.op {
                lbz | lbzu => "ld8(b, e) as u64",
                lhz | lhzu => "ld16(b, e) as u64",
                lha | lhau => "ld16(b, e) as i16 as i64 as u64",
                _ => "ld32(b, e) as u64",
            };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; c.r[{d}] = {val};{upd} }}"));
        }
        lbzx | lhzx | lhax | lwzx | lwax | ldx | lbzux | lhzux | lwzux | ldux | lwbrx | lhbrx => {
            let update = matches!(i.op, lbzux | lhzux | lwzux | ldux);
            let ea = if update { format!("ea(c.r[{a}], c.r[{rb}] as u32)") } else { ea_x(a) };
            let val = match i.op {
                lbzx | lbzux => "ld8(b, e) as u64",
                lhzx | lhzux => "ld16(b, e) as u64",
                lhax => "ld16(b, e) as i16 as i64 as u64",
                lwzx | lwzux => "ld32(b, e) as u64",
                lwax => "ld32(b, e) as i32 as i64 as u64",
                lwbrx => "ld32r(b, e) as u64",
                lhbrx => "ld16r(b, e) as u64",
                _ => "ld64(b, e)",
            };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; c.r[{d}] = {val};{upd} }}"));
        }
        ld | ldu | lwa => {
            let update = i.op == ldu;
            let val = if i.op == lwa { "ld32(b, e) as i32 as i64 as u64" } else { "ld64(b, e)" };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {}; c.r[{d}] = {val};{upd} }}", ea_ds(a)));
        }
        // ---- stores (source is RS = bits 21-25)
        stb | sth | stw | stbu | sthu | stwu => {
            let update = matches!(i.op, stbu | sthu | stwu);
            let ea = if update { format!("ea(c.r[{a}], 0x{:08X})", simm as u32) } else { ea_d(a) };
            let st = match i.op {
                stb | stbu => format!("st8(b, e, c.r[{d}] as u8)"),
                sth | sthu => format!("st16(b, e, c.r[{d}] as u16)"),
                _ => format!("st32(b, e, c.r[{d}] as u32)"),
            };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; {st};{upd} }}"));
        }
        stbx | sthx | stwx | stdx | stbux | sthux | stwux | stdux | stwbrx | sthbrx => {
            let update = matches!(i.op, stbux | sthux | stwux | stdux);
            let ea = if update { format!("ea(c.r[{a}], c.r[{rb}] as u32)") } else { ea_x(a) };
            let st = match i.op {
                stbx | stbux => format!("st8(b, e, c.r[{d}] as u8)"),
                sthx | sthux => format!("st16(b, e, c.r[{d}] as u16)"),
                stwx | stwux => format!("st32(b, e, c.r[{d}] as u32)"),
                stwbrx => format!("st32r(b, e, c.r[{d}] as u32)"),
                sthbrx => format!("st16r(b, e, c.r[{d}] as u16)"),
                _ => format!("st64(b, e, c.r[{d}])"),
            };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; {st};{upd} }}"));
        }
        std | stdu => {
            let upd = if i.op == stdu { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {}; st64(b, e, c.r[{d}]);{upd} }}", ea_ds(a)));
        }
        // ---- floating-point loads and stores
        lfs | lfd | lfsu | lfdu => {
            let update = matches!(i.op, lfsu | lfdu);
            let ea = if update { format!("ea(c.r[{a}], 0x{:08X})", simm as u32) } else { ea_d(a) };
            let val = if matches!(i.op, lfs | lfsu) { "f32::from_bits(ld32(b, e)) as f64" } else { "f64::from_bits(ld64(b, e))" };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; c.f[{d}] = {val};{upd} }}"));
        }
        lfsx | lfdx | lfsux | lfdux => {
            let update = matches!(i.op, lfsux | lfdux);
            let ea = if update { format!("ea(c.r[{a}], c.r[{rb}] as u32)") } else { ea_x(a) };
            let val = if matches!(i.op, lfsx | lfsux) { "f32::from_bits(ld32(b, e)) as f64" } else { "f64::from_bits(ld64(b, e))" };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; c.f[{d}] = {val};{upd} }}"));
        }
        stfs | stfd | stfsu | stfdu => {
            let update = matches!(i.op, stfsu | stfdu);
            let ea = if update { format!("ea(c.r[{a}], 0x{:08X})", simm as u32) } else { ea_d(a) };
            let st = if matches!(i.op, stfs | stfsu) {
                format!("st32(b, e, (c.f[{d}] as f32).to_bits())")
            } else {
                format!("st64(b, e, c.f[{d}].to_bits())")
            };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; {st};{upd} }}"));
        }
        stfsx | stfdx | stfiwx | stfsux | stfdux => {
            let update = matches!(i.op, stfsux | stfdux);
            let ea = if update { format!("ea(c.r[{a}], c.r[{rb}] as u32)") } else { ea_x(a) };
            let st = match i.op {
                stfsx | stfsux => format!("st32(b, e, (c.f[{d}] as f32).to_bits())"),
                stfdx | stfdux => format!("st64(b, e, c.f[{d}].to_bits())"),
                _ => format!("st32(b, e, c.f[{d}].to_bits() as u32)"),
            };
            let upd = if update { format!(" c.r[{a}] = e as u64;") } else { String::new() };
            s.push(format!("{{ let e = {ea}; {st};{upd} }}"));
        }
        // ---- FPSCR. Only round-to-nearest with exceptions masked is supported;
        // set_fpscr stops with a diagnostic if the game asks for anything else.
        mffsx => s.push(format!("c.f[{d}] = f64::from_bits(c.fpscr as u64);")),
        mtfsfx => s.push(format!(
            "set_fpscr(c, (c.fpscr & !0x{m:08X}) | (c.f[{rb}].to_bits() as u32 & 0x{m:08X}));",
            m = fxm_mask(i.bits(17, 8))
        )),
        fctidx => s.push(format!("c.f[{d}] = f64::from_bits(fctid(c.f[{rb}]) as u64);")),
        // ---- condition register logic (bit 0 = CR0[LT])
        crand | crandc | creqv | crnand | crnor | cror | crorc | crxor => {
            let (bd, ba, bb) = (d, a, rb);
            // CR bit n is a named field of CR field n / 4.
            let bit = |n: u32| format!("c.cr[{}].{}", n / 4, ["lt", "gt", "eq", "so"][(n % 4) as usize]);
            let (x1, y1) = (bit(ba), bit(bb));
            let v = match i.op {
                crand => format!("{x1} & {y1}"),
                crandc => format!("{x1} & !{y1}"),
                creqv => format!("{x1} == {y1}"),
                crnand => format!("!({x1} & {y1})"),
                crnor => format!("!({x1} | {y1})"),
                cror => format!("{x1} | {y1}"),
                crorc => format!("{x1} | !{y1}"),
                _ => format!("{x1} ^ {y1}"),
            };
            s.push(format!("{{ let v = {v}; {} = v; }}", bit(bd)));
        }
        mcrf => s.push(format!("c.cr[{}] = c.cr[{}];", i.bits(23, 3), i.bits(18, 3))),
        // ---- floating point arithmetic (A form: FD, FA, FB, FC)
        faddx | fsubx | fmulx | fdivx | faddsx | fsubsx | fmulsx | fdivsx => {
            let fc = i.bits(6, 5);
            let expr = match i.op {
                faddx | faddsx => format!("c.f[{a}] + c.f[{rb}]"),
                fsubx | fsubsx => format!("c.f[{a}] - c.f[{rb}]"),
                fmulx | fmulsx => format!("c.f[{a}] * c.f[{fc}]"),
                _ => format!("c.f[{a}] / c.f[{rb}]"),
            };
            let single = matches!(i.op, faddsx | fsubsx | fmulsx | fdivsx);
            s.push(if single { format!("c.f[{d}] = ({expr}) as f32 as f64;") } else { format!("c.f[{d}] = {expr};") });
        }
        fmaddx | fmsubx | fnmaddx | fnmsubx | fmaddsx | fmsubsx | fnmaddsx | fnmsubsx => {
            let fc = i.bits(6, 5);
            let base = match i.op {
                fmaddx | fmaddsx | fnmaddx | fnmaddsx => format!("c.f[{a}].mul_add(c.f[{fc}], c.f[{rb}])"),
                _ => format!("c.f[{a}].mul_add(c.f[{fc}], -c.f[{rb}])"),
            };
            let neg = matches!(i.op, fnmaddx | fnmsubx | fnmaddsx | fnmsubsx);
            let expr = if neg { format!("-({base})") } else { base };
            let single = matches!(i.op, fmaddsx | fmsubsx | fnmaddsx | fnmsubsx);
            s.push(if single { format!("c.f[{d}] = ({expr}) as f32 as f64;") } else { format!("c.f[{d}] = {expr};") });
        }
        fmrx => s.push(format!("c.f[{d}] = c.f[{rb}];")),
        fnegx => s.push(format!("c.f[{d}] = -c.f[{rb}];")),
        fabsx => s.push(format!("c.f[{d}] = c.f[{rb}].abs();")),
        fnabsx => s.push(format!("c.f[{d}] = -c.f[{rb}].abs();")),
        frspx => s.push(format!("c.f[{d}] = c.f[{rb}] as f32 as f64;")),
        fsqrtx => s.push(format!("c.f[{d}] = c.f[{rb}].sqrt();")),
        fsqrtsx => s.push(format!("c.f[{d}] = c.f[{rb}].sqrt() as f32 as f64;")),
        fresx => s.push(format!("c.f[{d}] = (1.0 / c.f[{rb}]) as f32 as f64;")),
        frsqrtex => s.push(format!("c.f[{d}] = 1.0 / c.f[{rb}].sqrt();")),
        fselx => s.push(format!("c.f[{d}] = if c.f[{a}] >= 0.0 {{ c.f[{}] }} else {{ c.f[{rb}] }};", i.bits(6, 5))),
        // Out of range saturates; NaN gives the most negative integer. The 32-bit
        // result is sign-extended into the register (as the test vectors expect).
        fctiwzx => s.push(format!("c.f[{d}] = f64::from_bits(fctiwz(c.f[{rb}]) as i64 as u64);")),
        fctidzx => s.push(format!("c.f[{d}] = f64::from_bits(fctidz(c.f[{rb}]) as u64);")),
        fcfidx => s.push(format!("c.f[{d}] = c.f[{rb}].to_bits() as i64 as f64;")),
        fcmpu | fcmpo => s.push(format!("c.cr[{}] = cmp_f(c.f[{a}], c.f[{rb}]);", i.bits(23, 3))),
        // ---- special registers
        mfspr => match i.spr() {
            8 => s.push(format!("c.r[{d}] = c.lr;")),
            9 => s.push(format!("c.r[{d}] = c.ctr;")),
            1 => s.push(format!(
                "c.r[{d}] = ((c.xer_so as u64) << 31) | ((c.xer_ov as u64) << 30) | ((c.xer_ca as u64) << 29);"
            )),
            _ => return None,
        },
        mtspr => match i.spr() {
            8 => s.push(format!("c.lr = c.r[{d}];")),
            9 => s.push(format!("c.ctr = c.r[{d}];")),
            1 => s.push(format!(
                "{{ let x = c.r[{d}]; c.xer_so = x & (1 << 31) != 0; c.xer_ov = x & (1 << 30) != 0; c.xer_ca = x & (1 << 29) != 0; }}"
            )),
            _ => return None,
        },
        mfcr => s.push(format!("c.r[{d}] = {} as u64;", cr_fields_word(0xFF))),
        // Only the selected fields are defined; return just those.
        mfocrf => s.push(format!("c.r[{d}] = {} as u64;", cr_fields_word(i.bits(12, 8)))),
        // The machine state register has no effect on recompiled code (there are
        // no guest interrupts to mask); keep the value so mfmsr reads it back.
        mfmsr => s.push(format!("c.r[{d}] = c.msr;")),
        mtmsrd => s.push(if i.bit(16) {
            format!("c.msr = (c.msr & !0x8002) | (c.r[{d}] & 0x8002);")
        } else {
            format!("c.msr = c.r[{d}];")
        }),
        mtcrf => {
            let fields: String = (0..8)
                .filter(|n| i.bits(12, 8) & (0x80 >> n) != 0)
                .map(|n| format!(" c.cr[{n}] = Cr::from_nibble(w >> {});", 28 - 4 * n))
                .collect();
            s.push(format!("{{ let w = c.r[{d}] as u32;{fields} }}"));
        }
        mftb => s.push(format!("c.r[{d}] = timebase();")),
        // ---- synchronization, cache control, atomics
        // sync is a full barrier; lwsync (L=1) orders everything except store->load.
        sync if i.bit(21) => s.push("core::sync::atomic::fence(core::sync::atomic::Ordering::AcqRel);".into()),
        sync => s.push("core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);".into()),
        isync | eieio => s.push("core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);".into()),
        dcbt | dcbtst | dcbf | dcbst | icbi => {}
        dcbz => s.push(format!("zero_line(b, {}, 32);", ea_x(a))),
        dcbz128 => s.push(format!("zero_line(b, {}, 128);", ea_x(a))),
        lwarx => s.push(format!("{{ let e = {}; c.reserved = ld32(b, e) as u64; c.r[{d}] = c.reserved; }}", ea_x(a))),
        ldarx => s.push(format!("{{ let e = {}; c.reserved = ld64(b, e); c.r[{d}] = c.reserved; }}", ea_x(a))),
        stwcx => s.push(format!(
            "{{ let e = {}; let ok = cas32(b, e, c.reserved as u32, c.r[{d}] as u32); c.cr[0] = Cr {{ lt: false, gt: false, eq: ok, so: c.xer_so }}; }}",
            ea_x(a)
        )),
        stdcx => s.push(format!(
            "{{ let e = {}; let ok = cas64(b, e, c.reserved, c.r[{d}]); c.cr[0] = Cr {{ lt: false, gt: false, eq: ok, so: c.xer_so }}; }}",
            ea_x(a)
        )),
        // ---- traps (guest assertions)
        twi | tw | tdi | td => {
            let to = d;
            let (sty, uty) = if matches!(i.op, twi | tw) { ("i32", "u32") } else { ("i64", "u64") };
            let rhs = if matches!(i.op, twi | tdi) { format!("({}{sty})", simm) } else { format!("(c.r[{rb}] as {sty})") };
            if to == 31 {
                s.push(format!("trap(0x{:08X});", i.address));
            } else {
                let mut conds = Vec::new();
                let l = format!("(c.r[{a}] as {sty})");
                if to & 16 != 0 { conds.push(format!("{l} < {rhs}")); }
                if to & 8 != 0 { conds.push(format!("{l} > {rhs}")); }
                if to & 4 != 0 { conds.push(format!("{l} == {rhs}")); }
                if to & 2 != 0 { conds.push(format!("({l} as {uty}) < ({rhs} as {uty})")); }
                if to & 1 != 0 { conds.push(format!("({l} as {uty}) > ({rhs} as {uty})")); }
                if !conds.is_empty() {
                    s.push(format!("if {} {{ trap(0x{:08X}); }}", conds.join(" || "), i.address));
                }
            }
        }
        // ---- vector loads/stores and bitwise ops (memory-order u128 registers)
        // Element loads leave the other elements undefined, so loading the whole
        // aligned quadword puts the element in the right place.
        lvx | lvxl | lvx128 | lvxl128 | lvebx | lvehx | lvewx | lvewx128 => {
            s.push(format!("c.v[{}] = ldv(b, {} & !15);", i.vd(), ea_x(a)))
        }
        stvx | stvxl | stvx128 | stvxl128 => s.push(format!("stv(b, {} & !15, c.v[{}]);", ea_x(a), i.vd())),
        stvebx => s.push(format!("{{ let e = {}; st8(b, e, (c.v[{}] >> (8 * (15 - (e & 15)))) as u8); }}", ea_x(a), i.vd())),
        stvehx => s.push(format!(
            "{{ let e = {} & !1; st16(b, e, (c.v[{}] >> (8 * (14 - (e & 15)))) as u16); }}",
            ea_x(a),
            i.vd()
        )),
        stvewx | stvewx128 => s.push(format!(
            "{{ let e = {} & !3; st32(b, e, (c.v[{}] >> (8 * (12 - (e & 15)))) as u32); }}",
            ea_x(a),
            i.vd()
        )),
        lvlx | lvlxl | lvlx128 | lvlxl128 => s.push(format!("c.v[{}] = ldvl(b, {});", i.vd(), ea_x(a))),
        lvrx | lvrxl | lvrx128 | lvrxl128 => s.push(format!("c.v[{}] = ldvr(b, {});", i.vd(), ea_x(a))),
        stvlx | stvlxl | stvlx128 | stvlxl128 => s.push(format!("stvl(b, {}, c.v[{}]);", ea_x(a), i.vd())),
        stvrx | stvrxl | stvrx128 | stvrxl128 => s.push(format!("stvr(b, {}, c.v[{}]);", ea_x(a), i.vd())),
        lvsl | lvsl128 => s.push(format!("c.v[{}] = lvsl({});", i.vd(), ea_x(a))),
        lvsr | lvsr128 => s.push(format!("c.v[{}] = lvsr({});", i.vd(), ea_x(a))),
        vor | vor128 => s.push(format!("c.v[{}] = c.v[{}] | c.v[{}];", i.vd(), i.va(), i.vb())),
        vand | vand128 => s.push(format!("c.v[{}] = c.v[{}] & c.v[{}];", i.vd(), i.va(), i.vb())),
        vandc | vandc128 | vxor | vxor128 if i.va() == i.vb() => s.push(format!("c.v[{}] = 0;", i.vd())),
        vandc | vandc128 => s.push(format!("c.v[{}] = c.v[{}] & !c.v[{}];", i.vd(), i.va(), i.vb())),
        vxor | vxor128 => s.push(format!("c.v[{}] = c.v[{}] ^ c.v[{}];", i.vd(), i.va(), i.vb())),
        vnor | vnor128 => s.push(format!("c.v[{}] = !(c.v[{}] | c.v[{}]);", i.vd(), i.va(), i.vb())),
        _ => {
            if !emit_vector(&i, &mut s) {
                return None;
            }
        }
    }
    // Record forms of integer instructions update CR0. `addic.` is a D-form
    // instruction of its own, so its record bit isn't in the word.
    let record = rc || i.op == addicx;
    if record && !matches!(i.op, andix | andisx | stwcx | stdcx) && !vector_record {
        let dest = match i.op.info().writes.iter().find(|(f, _)| matches!(f.name(), "RD" | "RA")) {
            Some((f, _)) if f.name() == "RD" => Some(d),
            Some(_) => Some(a),
            None => None,
        };
        match (i.op.info().group, dest) {
            (ppc::Group::i, Some(r)) => s.push(cr0(r)),
            // FPU record forms copy FPSCR exception bits into CR1, which needs
            // FPSCR modelling first. Leave them untranslated rather than wrong.
            _ => return None,
        }
    }
    Some(s.into_iter().filter(|l| !l.is_empty()).collect())
}

/// VMX / VMX128 arithmetic, permutes and conversions: each becomes a call to
/// the matching `ppc_rt` vector function. Returns false if not translated.
fn emit_vector(i: &Instr, s: &mut Vec<String>) -> bool {
    use Op::*;
    let (vd, va, vb) = (i.vd(), i.va(), i.vb());
    let vc = i.operand("VC").unwrap_or(0);
    let uimm = i.operand("UIMM").unwrap_or(0);
    let simm = i.operand("SIMM").unwrap_or(0);
    let un = |f: &str| format!("c.v[{vd}] = {f}(c.v[{vb}]);");
    let bin = |f: &str| format!("c.v[{vd}] = {f}(c.v[{va}], c.v[{vb}]);");
    let rc = i.rc();
    let vcmp = |f: &str, cr: &str| {
        if rc {
            format!("{{ let m = {f}(c.v[{va}], c.v[{vb}]); c.v[{vd}] = m; c.cr[6] = {cr}(m); }}")
        } else {
            format!("c.v[{vd}] = {f}(c.v[{va}], c.v[{vb}]);")
        }
    };
    let line = match i.op {
        // An integer vector compared with itself is all ones, and one
        // subtracted from itself is zero, whatever it holds.
        vcmpequb | vcmpequh | vcmpequw | vcmpequw128 if va == vb && !rc => format!("c.v[{vd}] = u128::MAX;"),
        vsububm | vsubuhm | vsubuwm if va == vb => format!("c.v[{vd}] = 0;"),
        // ---- floating point
        vaddfp | vaddfp128 => bin("vaddfp"),
        vsubfp | vsubfp128 => bin("vsubfp"),
        vmulfp128 => bin("vmulfp"),
        vmaxfp | vmaxfp128 => bin("vmaxfp"),
        vminfp | vminfp128 => bin("vminfp"),
        vmsum3fp128 => bin("vmsum3fp"),
        vmsum4fp128 => bin("vmsum4fp"),
        // AltiVec order is vD, vA, vC, vB: vA * vC + vB.
        vmaddfp => format!("c.v[{vd}] = vmaddfp(c.v[{va}], c.v[{vc}], c.v[{vb}]);"),
        vnmsubfp => format!("c.v[{vd}] = vnmsubfp(c.v[{va}], c.v[{vc}], c.v[{vb}]);"),
        // VMX128 reuses vD as one input: vD = vA * vB + vD,
        vmaddfp128 => format!("c.v[{vd}] = vmaddfp(c.v[{va}], c.v[{vb}], c.v[{vd}]);"),
        // vD = vA * vD + vB (vD the multiplicand),
        vmaddcfp128 => format!("c.v[{vd}] = vmaddfp(c.v[{va}], c.v[{vd}], c.v[{vb}]);"),
        // and vD = -(vA * vB - vD) (vD the accumulator, as for vmaddfp128). The game's
        // vector normalize shows it: with vD = 0.5, vA = x/2 and vB = y0*y0 it is the
        // Newton step 0.5 - x/2 * y0^2 for 1/sqrt(x).
        vnmsubfp128 => format!("c.v[{vd}] = vnmsubfp(c.v[{va}], c.v[{vb}], c.v[{vd}]);"),
        vrefp | vrefp128 => un("vrefp"),
        vrsqrtefp | vrsqrtefp128 => un("vrsqrtefp"),
        vexptefp | vexptefp128 => un("vexptefp"),
        vlogefp | vlogefp128 => un("vlogefp"),
        vrfin | vrfin128 => un("vrfin"),
        vrfiz | vrfiz128 => un("vrfiz"),
        vrfip | vrfip128 => un("vrfip"),
        vrfim | vrfim128 => un("vrfim"),
        vcfsx | vcsxwfp128 => format!("c.v[{vd}] = vcfsx(c.v[{vb}], {uimm});"),
        vcfux | vcuxwfp128 => format!("c.v[{vd}] = vcfux(c.v[{vb}], {uimm});"),
        vctsxs | vcfpsxws128 => format!("c.v[{vd}] = vctsxs(c.v[{vb}], {uimm});"),
        vctuxs | vcfpuxws128 => format!("c.v[{vd}] = vctuxs(c.v[{vb}], {uimm});"),
        // ---- compares (record forms set CR6)
        vcmpeqfp | vcmpeqfp128 => vcmp("vcmpeqfp", "cr6"),
        vcmpgefp | vcmpgefp128 => vcmp("vcmpgefp", "cr6"),
        vcmpgtfp | vcmpgtfp128 => vcmp("vcmpgtfp", "cr6"),
        vcmpbfp | vcmpbfp128 => vcmp("vcmpbfp", "cr6_bounds"),
        vcmpequb => vcmp("vcmpequb", "cr6"),
        vcmpequh => vcmp("vcmpequh", "cr6"),
        vcmpequw | vcmpequw128 => vcmp("vcmpequw", "cr6"),
        vcmpgtub => vcmp("vcmpgtub", "cr6"),
        vcmpgtuh => vcmp("vcmpgtuh", "cr6"),
        vcmpgtuw => vcmp("vcmpgtuw", "cr6"),
        vcmpgtsb => vcmp("vcmpgtsb", "cr6"),
        vcmpgtsh => vcmp("vcmpgtsh", "cr6"),
        vcmpgtsw => vcmp("vcmpgtsw", "cr6"),
        // ---- integer arithmetic
        vaddubm | vaddubs | vadduhm | vadduhs | vadduwm | vadduws | vaddsbs | vaddshs | vaddsws | vsububm
        | vsububs | vsubuhm | vsubuhs | vsubuwm | vsubuws | vsubsbs | vsubshs | vsubsws | vavgub | vavguh
        | vavguw | vavgsb | vavgsh | vavgsw | vmaxub | vmaxuh | vmaxuw | vmaxsb | vmaxsh | vmaxsw | vminub
        | vminuh | vminuw | vminsb | vminsh | vminsw | vaddcuw | vsubcuw | vslb | vslh | vslw | vsrb | vsrh
        | vsrw | vsrab | vsrah | vsraw | vrlb | vrlh | vrlw | vsl | vsr | vslo | vsro | vmrghb | vmrghh
        | vmrghw | vmrglb | vmrglh | vmrglw | vpkuhum | vpkuwum | vpkuhus | vpkuwus | vpkshss | vpkswss
        | vpkshus | vpkswus => bin(i.op.mnemonic()),
        vslw128 | vsrw128 | vsraw128 | vrlw128 | vslo128 | vsro128 | vmrghw128 | vmrglw128 | vpkuhum128
        | vpkuwum128 | vpkuhus128 | vpkuwus128 | vpkshss128 | vpkswss128 | vpkshus128 | vpkswus128 => {
            bin(i.op.mnemonic().trim_end_matches("128"))
        }
        // ---- permutes, selects, splats
        vperm | vperm128 => format!("c.v[{vd}] = vperm(c.v[{va}], c.v[{vb}], c.v[{vc}]);"),
        vsel => format!("c.v[{vd}] = vsel(c.v[{va}], c.v[{vb}], c.v[{vc}]);"),
        vsel128 => format!("c.v[{vd}] = vsel(c.v[{va}], c.v[{vb}], c.v[{vd}]);"),
        vsldoi | vsldoi128 => {
            format!("c.v[{vd}] = vsldoi(c.v[{va}], c.v[{vb}], {});", i.operand("SHB").unwrap_or(0))
        }
        vspltb => format!("c.v[{vd}] = vspltb(c.v[{vb}], {uimm});"),
        vsplth => format!("c.v[{vd}] = vsplth(c.v[{vb}], {uimm});"),
        vspltw | vspltw128 => format!("c.v[{vd}] = vspltw(c.v[{vb}], {uimm});"),
        vspltisb => format!("c.v[{vd}] = vspltisb({simm});"),
        vspltish => format!("c.v[{vd}] = vspltish({simm});"),
        vspltisw | vspltisw128 => format!("c.v[{vd}] = vspltisw({simm});"),
        vpermwi128 => format!("c.v[{vd}] = vpermwi(c.v[{vb}], {uimm});"),
        vrlimi128 => format!(
            "c.v[{vd}] = vrlimi(c.v[{vd}], c.v[{vb}], {}, {});",
            i.operand("IMM").unwrap_or(0),
            i.operand("z").unwrap_or(0)
        ),
        vupkhsb | vupkhsb128 => un("vupkhsb"),
        vupklsb | vupklsb128 => un("vupklsb"),
        vupkhsh | vupkhsh128 => un("vupkhsh"),
        vupklsh | vupklsh128 => un("vupklsh"),
        // ---- D3D vertex formats (formats the runtime implements; others stay untranslated)
        vpkd3d128 => {
            let imm = i.operand("IMM").unwrap_or(0);
            let (kind, pack, shift) = (imm >> 2, imm & 3, i.operand("z").unwrap_or(0));
            if kind > 5 || pack == 0 {
                return false;
            }
            format!("c.v[{vd}] = vpkd3d(c.v[{vd}], c.v[{vb}], {kind}, {pack}, {shift});")
        }
        vupkd3d128 => {
            if uimm & 3 != 0 || uimm >> 2 > 5 {
                return false;
            }
            format!("c.v[{vd}] = vupkd3d(c.v[{vb}], {});", uimm >> 2)
        }
        _ => return false,
    };
    s.push(line);
    true
}

/// `mtfsf` field mask (bit 7 = field 0) as a 32-bit mask.
fn fxm_mask(fxm: u32) -> u32 {
    (0..8).filter(|n| fxm & (0x80 >> n) != 0).fold(0, |m, n| m | (0xF000_0000 >> (4 * n)))
}

/// The CR fields `fxm` selects (bit 7 = CR0) as an expression for their bits
/// in the 32-bit condition register; the other fields read as zero.
fn cr_fields_word(fxm: u32) -> String {
    let fields: Vec<String> = (0..8)
        .filter(|n| fxm & (0x80 >> n) != 0)
        .map(|n| if n == 7 { format!("c.cr[{n}].nibble()") } else { format!("(c.cr[{n}].nibble() << {})", 28 - 4 * n) })
        .collect();
    if fields.is_empty() { "0u32".into() } else { format!("({})", fields.join(" | ")) }
}

fn mask_expr(mb: u32, me: u32) -> String {
    format!("0x{:X}u64", {
        let a = u64::MAX >> mb;
        let b = u64::MAX << (63 - me);
        if mb <= me { a & b } else { a | b }
    })
}

/// The body of the save/restore helper entered at `entry`, translated: every
/// instruction up to its final `blr`. `None` if `entry` isn't one, or if the
/// helper isn't straight code the emitter translates (then it is called).
fn inlined(p: &Program, entry: u32) -> Option<Vec<Item>> {
    let end = *p.inline.get(&entry)?;
    let mut items = Vec::new();
    for a in (entry..end).step_by(4) {
        let i = Instr::decode(a, (p.word)(a)?)?;
        if a + 4 == end {
            // `blr`, unconditional.
            return (i.op == Op::bclrx && !i.lk() && bo_cond(&i) == "true").then_some(items);
        }
        if matches!(i.op, Op::bx | Op::bcx | Op::bclrx | Op::bcctrx) {
            return None;
        }
        items.push(Item::Code { addr: a, lines: emit_op(&i)?, instr: Some(i) });
    }
    None
}

/// Reads one function into blocks. Block leaders are the entry, intra-function
/// branch targets, switch labels, and every instruction after a branch.
pub fn scan_function(p: &Program, f: &FnDef) -> Body {
    let inside = |a: u32| (f.begin..f.end).contains(&a);
    let in_skip = |a: u32| f.skip.iter().any(|&(s, e)| (s..e).contains(&a));
    let addrs: Vec<u32> = (f.begin..f.end).step_by(4).filter(|&a| !in_skip(a)).collect();

    let mut leaders = BTreeSet::from([f.begin]);
    let mut internal_targets = false;
    // Return addresses of calls to setjmp: where a longjmp resumes.
    let mut resume_sites: Vec<u32> = Vec::new();
    for &a in &addrs {
        let Some(i) = (p.word)(a).and_then(|w| Instr::decode(a, w)) else { continue };
        match i.op {
            Op::bx if i.lk() && p.setjmp.contains(&i.branch_target().unwrap()) => {
                resume_sites.push(a + 4);
                leaders.insert(a + 4);
                internal_targets = true;
            }
            Op::bx | Op::bcx if !i.lk() => {
                let t = i.branch_target().unwrap();
                if inside(t) && !in_skip(t) {
                    leaders.insert(t);
                    internal_targets = true;
                }
                leaders.insert(a + 4);
            }
            Op::bclrx | Op::bcctrx if !i.lk() => {
                if let Some((_, labels, default)) = p.switches.get(&a) {
                    internal_targets = true;
                    leaders.extend(labels.iter().copied().filter(|&l| inside(l)));
                    if let Some(d) = default.filter(|&d| inside(d)) {
                        leaders.insert(d);
                    }
                }
                leaders.insert(a + 4);
            }
            _ => {}
        }
    }
    leaders.retain(|&l| l < f.end && !in_skip(l));
    // Straight-line code only when nothing branches back into the function
    // (a `b` to its own start is a loop, not a tail call).
    let single = leaders.len() == 1 && !internal_targets;

    let leaders: Vec<u32> = leaders.into_iter().collect();
    let mut body =
        Body { begin: f.begin, end: f.end, single, blocks: Vec::new(), resume_sites, instructions: 0, todo_ops: Vec::new() };
    for (n, &start) in leaders.iter().enumerate() {
        let end = leaders.get(n + 1).copied().unwrap_or(f.end);
        let mut items: Vec<Item> = Vec::new();
        let mut term: Option<Term> = None;
        for a in (start..end).step_by(4).filter(|&a| !in_skip(a)) {
            let Some(word) = (p.word)(a) else { break };
            let Some(i) = Instr::decode(a, word) else {
                items.push(Item::Code { addr: a, lines: vec![format!("todo_instr(0x{a:08X}, 0x{word:08X});")], instr: None });
                body.todo_ops.push("<undecodable>");
                continue;
            };
            body.instructions += 1;
            let next = a + 4;
            // A call or branch to a save/restore helper: its instructions, here.
            if i.op == Op::bx
                && let Some(code) = i.branch_target().and_then(|t| inlined(p, t))
            {
                if i.lk() {
                    items.push(Item::Code { addr: a, lines: vec![format!("c.lr = 0x{next:08X};")], instr: None });
                    items.extend(code);
                } else {
                    items.extend(code);
                    term = Some(Term::Exit(Exit::Return));
                    break;
                }
                continue;
            }
            match i.op {
                Op::bx if i.lk() => {
                    let target = i.branch_target().unwrap();
                    // A call to setjmp ends the block: the resume point after
                    // it is a block leader.
                    let setjmp = p.setjmp.contains(&target);
                    items.push(Item::Call { addr: a, target: Target::Direct(target), cond: None, lr: next, setjmp });
                    if setjmp {
                        term = Some(Term::Resume(next));
                    }
                }
                Op::bx => {
                    let t = i.branch_target().unwrap();
                    term = Some(if inside(t) && !single { Term::Jump(t) } else { Term::Exit(Exit::Tail(Target::Direct(t))) });
                }
                Op::bcx => {
                    let t = i.branch_target().unwrap();
                    let cond = bo_cond(&i);
                    if i.lk() {
                        items.push(Item::Call { addr: a, target: Target::Direct(t), cond: Some(cond), lr: next, setjmp: false });
                    } else if inside(t) && !single {
                        term = Some(Term::Branch { cond, taken: t, next });
                    } else {
                        items.push(Item::CondExit { addr: a, cond, exit: Exit::Tail(Target::Direct(t)) });
                    }
                }
                // longjmp has restored the saved registers, r1 and LR: unwind
                // to the frame that called setjmp.
                Op::bclrx if p.longjmp_returns.contains(&a) => term = Some(Term::Exit(Exit::Longjmp)),
                Op::bclrx => {
                    let cond = bo_cond(&i);
                    if i.lk() {
                        items.push(Item::Call { addr: a, target: Target::Lr, cond: Some(cond), lr: next, setjmp: false });
                    } else if cond == "true" {
                        term = Some(Term::Exit(Exit::Return));
                    } else {
                        items.push(Item::CondExit { addr: a, cond, exit: Exit::Return });
                    }
                }
                Op::bcctrx => {
                    let cond = bo_cond(&i);
                    if i.lk() {
                        items.push(Item::Call { addr: a, target: Target::Ctr, cond: Some(cond), lr: next, setjmp: false });
                    } else if let Some((reg, labels, default)) = p.switches.get(&a).filter(|_| !single) {
                        term = Some(Term::Switch { reg: *reg, labels: labels.clone(), default: default.filter(|&d| inside(d)) });
                    } else if cond == "true" {
                        term = Some(Term::Exit(Exit::Tail(Target::Ctr)));
                    } else {
                        items.push(Item::CondExit { addr: a, cond, exit: Exit::Tail(Target::Ctr) });
                    }
                }
                _ => match emit_op(&i) {
                    Some(lines) => items.push(Item::Code { addr: a, lines, instr: Some(i) }),
                    None => {
                        items.push(Item::Code { addr: a, lines: vec![format!("todo_instr(0x{a:08X}, 0x{word:08X});")], instr: None });
                        body.todo_ops.push(i.op.mnemonic());
                    }
                },
            }
            if term.is_some() {
                break;
            }
        }
        let term = term.unwrap_or_else(|| {
            if end < f.end && !single {
                Term::Jump(end)
            } else if f.falls_through {
                Term::Exit(Exit::FallInto(f.end))
            } else {
                Term::Exit(Exit::FellOff(end.saturating_sub(4)))
            }
        });
        body.blocks.push(Block { start, items, term });
    }
    body
}

/// Writes one function's blocks as Rust. With a `plan`, the registers it names
/// are host locals, and the context is read and written only where the plan
/// says (`locals`).
pub fn render(p: &Program, body: &Body, plan: Option<&Plan>) -> Emitted {
    let mut out = String::new();
    let mut callees: BTreeSet<u32> = BTreeSet::new();
    let single = body.single;
    let name = fn_name(body.begin);
    let locals = plan.map(|p| p.locals).unwrap_or(RegSet::EMPTY);
    let saved = plan.map(|p| p.saved).unwrap_or(RegSet::EMPTY);
    let tx = |s: &str| regs::localize(s, &locals);
    // Locals to the context, the context to locals, and the kept slot contents back.
    let stores = |set: RegSet| set.iter().map(|r| format!("{} = {}; ", regs::ctx_name(r), regs::local_name(r))).collect::<String>();
    let loads = |set: RegSet| set.iter().map(|r| format!(" {} = {};", regs::local_name(r), regs::ctx_name(r))).collect::<String>();
    let put_back: String = saved.iter().map(|r| format!("{} = sv_{}; ", regs::ctx_name(r), regs::local_name(r))).collect();
    // Registers taken to be restored without proof: every exit checks them.
    let checked = plan.map(|p| p.checked).unwrap_or(RegSet::EMPTY);
    let check = if checked.is_empty() {
        String::new()
    } else {
        let differs: Vec<String> = checked
            .iter()
            .map(|r| {
                let l = regs::local_name(r);
                if (regs::FPR..regs::CR).contains(&r) { format!("{l}.to_bits() != sv_{l}.to_bits()") } else { format!("{l} != sv_{l}") }
            })
            .collect();
        format!("if {} {{ not_preserved(0x{:08X}, \"{}\"); }} ", differs.join(" || "), body.begin, regs::describe(checked))
    };
    let _ = writeln!(out, "#[unsafe(no_mangle)]\npub unsafe extern \"C-unwind\" fn {name}(c: &mut Ctx, b: *mut u8) {{ unsafe {{");
    if let Some(plan) = plan {
        for r in locals.iter() {
            let (ty, zero) = regs::local_type(r);
            let init = if plan.load.contains(r) { regs::ctx_name(r) } else { zero.to_string() };
            let _ = writeln!(out, "    let mut {}: {ty} = {init};", regs::local_name(r));
        }
        for r in saved.iter() {
            let _ = writeln!(out, "    let sv_{} = {};", regs::local_name(r), regs::ctx_name(r));
        }
    }
    if !single {
        let _ = writeln!(out, "    let mut pc: u32 = 0x{:08X};", body.begin);
        if !body.resume_sites.is_empty() {
            // The dispatch loop runs inside catch_unwind. A longjmp to one of
            // this activation's setjmp calls (same stack pointer, same return
            // address) resumes the loop there; anything else keeps unwinding.
            let _ = writeln!(
                out,
                "    let mut frame: u32 = 0;\n    loop {{\n    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{"
            );
        }
        let _ = writeln!(out, "    loop {{ match pc {{");
    }
    fn line(out: &mut String, s: &str) {
        out.push_str("            ");
        out.push_str(s);
        out.push('\n');
    }
    let mut call = |target: &Target| match *target {
        Target::Direct(t) if p.entries.contains(&t) => {
            callees.insert(t);
            format!("{}(c, b)", fn_name(t))
        }
        Target::Direct(t) => format!("call_indirect(c, b, 0x{t:08X})"),
        Target::Ctr => tx("call_indirect(c, b, c.ctr as u32)"),
        Target::Lr => "call_indirect(c, b, c.lr as u32)".to_string(),
    };
    // An exit: the stores it needs, the kept context slots put back, then the exit.
    let exit = |e: &Exit, flush: RegSet, call: &mut dyn FnMut(&Target) -> String| {
        let before = if matches!(e, Exit::Return | Exit::Tail(_) | Exit::FallInto(_)) {
            format!("{}{check}{put_back}", stores(flush))
        } else {
            String::new()
        };
        match e {
            Exit::Return => format!("{before}return;"),
            Exit::Tail(t) => format!("{before}{}; return;", call(t)),
            Exit::Longjmp => "longjmp_unwind(c); return;".to_string(),
            Exit::FallInto(next) => format!("{before}{}(c, b); return;", fn_name(*next)),
            Exit::FellOff(a) => format!("fell_off(0x{a:08X}); return;"),
        }
    };
    let mut falls_into: Option<u32> = None;
    for (bi, block) in body.blocks.iter().enumerate() {
        if !single {
            let _ = writeln!(out, "        0x{:08X} => {{", block.start);
        }
        for (ii, item) in block.items.iter().enumerate() {
            let site = plan.map(|p| p.sites[bi][ii]).unwrap_or_default();
            match item {
                Item::Code { lines, .. } => {
                    if plan.is_some_and(|p| p.skip[bi][ii]) {
                        continue;
                    }
                    for l in lines {
                        line(&mut out, &tx(l));
                    }
                }
                Item::Call { target, cond, lr, setjmp, .. } => {
                    let (flush, reload) = (stores(site.flush), loads(site.reload));
                    let s = match (target, cond) {
                        // Remember which stack frame this setjmp belongs to,
                        // then continue at the resume point (a block leader).
                        (_, None) if *setjmp => {
                            format!("c.lr = 0x{lr:08X}; frame = c.r[1] as u32; {}; pc = 0x{lr:08X};", call(target))
                        }
                        (_, None) => format!("c.lr = 0x{lr:08X}; {flush}{};{reload}", call(target)),
                        (Target::Lr, Some(cond)) => format!(
                            "if {} {{ let t = c.lr as u32; c.lr = 0x{lr:08X}; {flush}call_indirect(c, b, t);{reload} }}",
                            tx(cond)
                        ),
                        (_, Some(cond)) => format!("if {} {{ c.lr = 0x{lr:08X}; {flush}{};{reload} }}", tx(cond), call(target)),
                    };
                    line(&mut out, &s);
                }
                Item::CondExit { cond, exit: e, .. } => {
                    let s = format!("if {} {{ {} }}", tx(cond), exit(e, site.flush, &mut call));
                    line(&mut out, &s);
                }
            }
        }
        match &block.term {
            Term::Jump(t) => line(&mut out, &format!("pc = 0x{t:08X};")),
            Term::Resume(_) => {}
            Term::Branch { cond, taken, next } => {
                line(&mut out, &format!("pc = if {} {{ 0x{taken:08X} }} else {{ 0x{next:08X} }};", tx(cond)))
            }
            Term::Switch { reg, labels, default } => {
                let mut arms = String::new();
                for (k, l) in labels.iter().enumerate() {
                    let _ = write!(arms, "{k} => 0x{l:08X}, ");
                }
                let fallback = match default {
                    Some(d) => format!("0x{d:08X}"),
                    None => "{ bad_pc(0xFFFF_FFFF); return; }".to_string(),
                };
                line(&mut out, &format!("pc = match {} as u32 {{ {arms}_ => {fallback} }};", tx(&format!("c.r[{reg}]"))));
            }
            Term::Exit(e) => {
                if let Exit::FallInto(next) = e {
                    falls_into = Some(*next);
                }
                let s = exit(e, plan.map(|p| p.exits[bi]).unwrap_or_default(), &mut call);
                line(&mut out, &s);
            }
        }
        if !single {
            out.push_str("        }\n");
        }
    }
    if !single {
        out.push_str("        _ => { bad_pc(pc); return; }\n    } }\n");
        if !body.resume_sites.is_empty() {
            let sites: Vec<String> = body.resume_sites.iter().map(|s| format!("0x{s:08X}")).collect();
            let _ = writeln!(
                out,
                "    }}));\n    match run {{ Ok(()) => return, Err(e) => pc = longjmp_target(c, e, frame, &[{}]) }}\n    }}",
                sites.join(", ")
            );
        }
    }
    out.push_str("} }\n\n");
    callees.extend(falls_into);
    Emitted { source: out, callees, instructions: body.instructions, todo: body.todo_ops.len(), todo_ops: body.todo_ops.clone() }
}

/// Emits one function on its own, with every register in the context.
pub fn emit_function(p: &Program, f: &FnDef) -> Emitted {
    render(p, &scan_function(p, f), None)
}

/// A whole program, translated.
pub struct Unit {
    /// One per function, in the order given.
    pub emitted: Vec<Emitted>,
    /// Functions the indirect-call table must refuse, with the reason (`locals`).
    pub not_indirect: BTreeMap<u32, String>,
    /// What the register analysis found, for the build log.
    pub report: String,
    /// One line per function: how it keeps its registers.
    pub details: String,
    /// Code the register analysis can't translate soundly. The build must stop.
    pub errors: Vec<String>,
}

/// Emits every function of a program. The register analysis runs over all of
/// them together (`locals`).
pub fn emit_program(p: &Program, defs: &[&FnDef], opts: &locals::Options) -> Unit {
    let bodies: Vec<Body> = defs.iter().map(|f| scan_function(p, f)).collect();
    let a = locals::analyse(p, &bodies, opts);
    let emitted = bodies.iter().zip(&a.plans).map(|(body, plan)| render(p, body, plan.as_ref())).collect();
    Unit { emitted, not_indirect: a.not_indirect, report: a.report, details: a.details, errors: a.errors }
}
