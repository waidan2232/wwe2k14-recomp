//! Jump-table (switch) detection.
//!
//! For every `bctr`, work backwards to find what was moved into CTR, building a
//! small expression from the instructions that define each register. Then
//! match the expression against the table shapes MSVC emits for the 360:
//!
//! - absolute:     `ctr = load32(T + sel*4)`
//! - computed:     `ctr = B + (load8(T + sel) << s)`
//! - byte offset:  `ctr = B + load8(T + sel)`
//! - short offset: `ctr = B + load16(T + sel*2)`
//!
//! The case count comes from the `cmplwi crN, sel, max` / `bgt crN, default`
//! guard that precedes the table. The same table kinds as XenonAnalyse (MIT),
//! but found by data flow instead of fixed instruction sequences, so
//! scheduling differences don't break detection.

use ppc::{Instr, Op};

use crate::code::{Code, bo_always, gprs_written, is_branch, is_call, is_mtctr, is_unconditional_transfer};

const WINDOW: u32 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Absolute,
    Computed { shift: u32 },
    ByteOffset,
    ShortOffset,
}

impl Kind {
    pub fn entry_size(self) -> u32 {
        match self {
            Kind::Absolute => 4,
            Kind::ShortOffset => 2,
            Kind::Computed { .. } | Kind::ByteOffset => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SwitchTable {
    /// Address of the `bctr`.
    pub bctr: u32,
    pub kind: Kind,
    /// Address of the table data.
    pub table: u32,
    /// Register compared against the case count (the switch value).
    pub reg: u32,
    /// Target taken when the value is out of range, if the guard branches to one.
    pub default: Option<u32>,
    pub labels: Vec<u32>,
    /// The bounds check is in a predecessor block. The case count was inferred from
    /// an inline table running up to the lowest case label.
    pub inferred_bound: bool,
}

impl SwitchTable {
    pub fn data_range(&self) -> (u32, u32) {
        (self.table, self.table + self.labels.len() as u32 * self.kind.entry_size())
    }
}

/// What a `bctr` (no link) turned out to be.
#[derive(Clone, Debug)]
pub enum BctrKind {
    Switch(SwitchTable),
    /// CTR came from memory through a non-constant address: a tail call through
    /// a function pointer or vtable.
    TailCallViaPointer,
    /// CTR holds a value computed outside the local straight-line code
    /// (typically a function pointer passed in a register).
    TailCallViaRegister,
    /// Indexed load from a pointer table outside code with no bounds check:
    /// a tail call through a dispatch table.
    TailCallViaTable { table: u32 },
    /// A table shape was found, but no `cmplwi`/`bgt` bound before it.
    TableWithoutBound { table: u32 },
    Unknown { reason: String },
}

#[derive(Clone, Debug)]
pub struct BctrSite {
    pub address: u32,
    pub kind: BctrKind,
}

/// A symbolic value built from the instructions that define a register.
#[derive(Clone, Debug)]
enum Val {
    Const(u32),
    /// Register whose definition is outside the window (an input to the table code).
    Input(u32),
    Load { size: u32, addr: Box<Val> },
    Add(Box<Val>, Box<Val>),
    /// Left shift; the last field is the register the shift reads (the table index register).
    Shl(Box<Val>, u32, u32),
    Unknown,
}

/// `a + b` with constant folding, so `lis`+`addi` pairs become a single constant.
fn add(a: Val, b: Val) -> Val {
    match (a, b) {
        (Val::Const(x), Val::Const(y)) => Val::Const(x.wrapping_add(y)),
        (Val::Add(p, q), Val::Const(y)) | (Val::Const(y), Val::Add(p, q)) => match (*p, *q) {
            (Val::Const(x), other) | (other, Val::Const(x)) => add(other, Val::Const(x.wrapping_add(y))),
            (p, q) => Val::Add(Box::new(Val::Add(Box::new(p), Box::new(q))), Box::new(Val::Const(y))),
        },
        (a, b) => Val::Add(Box::new(a), Box::new(b)),
    }
}

struct Window {
    instrs: Vec<Instr>,
}

impl Window {
    /// Straight-line instructions before `end`, stopping at calls or unconditional transfers.
    fn before(code: &Code, end: u32) -> Window {
        let mut instrs = Vec::new();
        let mut addr = end;
        for _ in 0..WINDOW {
            let Some(prev) = addr.checked_sub(4) else { break };
            let Some(i) = code.instr(prev) else { break };
            if is_call(&i) || is_unconditional_transfer(&i) {
                break;
            }
            instrs.push(i);
            addr = prev;
        }
        instrs.reverse();
        Window { instrs }
    }

    /// Value of `reg` just before `instrs[at]`. `opaque` is `(register, address)`:
    /// that register is treated as an input if its definition comes before
    /// `address`. It's the switch selector at its bounds check, and expanding it
    /// would fold its bias into the table base.
    fn eval(&self, reg: u32, at: usize, depth: u32, opaque: Option<(u32, u32)>) -> Val {
        if depth > 12 {
            return Val::Unknown;
        }
        let Some(def) = (0..at).rev().find(|&k| gprs_written(&self.instrs[k]).any(|r| r == reg)) else {
            return Val::Input(reg);
        };
        let i = self.instrs[def];
        if let Some((r, compare_at)) = opaque
            && r == reg
        {
            // Defined before the bounds check, or re-materialized afterwards with the
            // identical instruction (the compiler sometimes recomputes `sel - bias`).
            let rematerialized = (0..def).rev().find(|&k| gprs_written(&self.instrs[k]).any(|w| w == reg)).is_some_and(
                |prev| self.instrs[prev].word == i.word && self.instrs[prev].address < compare_at,
            );
            if i.address < compare_at || rematerialized {
                return Val::Input(reg);
            }
        }
        let rec = |r: u32| Box::new(self.eval(r, def, depth + 1, opaque));
        let val = |r: u32| self.eval(r, def, depth + 1, opaque);
        match i.op {
            Op::addi if i.ra() == 0 => Val::Const(i.simm16() as u32),
            Op::addi => add(val(i.ra()), Val::Const(i.simm16() as u32)),
            Op::addis if i.ra() == 0 => Val::Const((i.simm16() as u32) << 16),
            Op::addis => add(val(i.ra()), Val::Const((i.simm16() as u32) << 16)),
            Op::ori if i.uimm16() == 0 => val(i.rt()), // nop / mr
            Op::ori => match val(i.rt()) {
                Val::Const(c) => Val::Const(c | i.uimm16()),
                _ => Val::Unknown,
            },
            Op::orx if i.rt() == i.rb() => val(i.rt()), // mr
            Op::addx => add(val(i.ra()), val(i.rb())),
            Op::rlwinmx => {
                let (sh, mb, me) = (i.rb(), i.bits(6, 5), i.bits(1, 5));
                let src = rec(i.rt());
                if sh == 0 {
                    *src // clrlwi-style masking of the index
                } else if mb == 0 && me == 31 - sh {
                    Val::Shl(src, sh, i.rt()) // slwi
                } else {
                    Val::Unknown
                }
            }
            Op::lwz | Op::lbz | Op::lhz => {
                let size = match i.op {
                    Op::lwz => 4,
                    Op::lhz => 2,
                    _ => 1,
                };
                let ra = if i.ra() == 0 { Val::Const(0) } else { val(i.ra()) };
                Val::Load { size, addr: Box::new(add(ra, Val::Const(i.simm16() as u32))) }
            }
            Op::lwzx | Op::lbzx | Op::lhzx => {
                let size = match i.op {
                    Op::lwzx => 4,
                    Op::lhzx => 2,
                    _ => 1,
                };
                let ra = if i.ra() == 0 { Val::Const(0) } else { val(i.ra()) };
                Val::Load { size, addr: Box::new(add(ra, val(i.rb()))) }
            }
            _ => Val::Unknown,
        }
    }
}

/// Splits `a + b` into (constant, rest) when exactly one side is constant.
fn split_const(v: &Val) -> Option<(u32, &Val)> {
    match v {
        Val::Add(a, b) => match (a.as_ref(), b.as_ref()) {
            (Val::Const(c), other) | (other, Val::Const(c)) => Some((*c, other)),
            _ => None,
        },
        _ => None,
    }
}

/// `(kind, table, selector register)` if `v` has a jump-table shape.
fn table_shape(v: &Val) -> Option<(Kind, u32, Option<u32>)> {
    let input = |v: &Val| match v {
        Val::Input(r) => Some(*r),
        _ => None,
    };
    match v {
        Val::Load { size: 4, addr } => {
            let (t, idx) = split_const(addr)?;
            match idx {
                Val::Shl(sel, 2, reg) => Some((Kind::Absolute, t, input(sel).or(Some(*reg)))),
                _ => None,
            }
        }
        _ => {
            let (_base, rest) = split_const(v)?;
            match rest {
                Val::Shl(load, shift, _) => match load.as_ref() {
                    Val::Load { size: 1, addr } => {
                        let (t, sel) = split_const(addr)?;
                        Some((Kind::Computed { shift: *shift }, t, input(sel)))
                    }
                    _ => None,
                },
                Val::Load { size: 1, addr } => {
                    let (t, sel) = split_const(addr)?;
                    Some((Kind::ByteOffset, t, input(sel)))
                }
                Val::Load { size: 2, addr } => {
                    let (t, idx) = split_const(addr)?;
                    match idx {
                        Val::Shl(sel, 1, reg) => Some((Kind::ShortOffset, t, input(sel).or(Some(*reg)))),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
    }
}

fn table_base(v: &Val) -> u32 {
    match v {
        Val::Load { .. } => 0,
        _ => split_const(v).map_or(0, |(b, _)| b),
    }
}

/// Whether `i` can change condition-register field `cr`.
fn writes_cr_field(i: &Instr, cr: u32) -> bool {
    match i.op {
        Op::cmp | Op::cmpi | Op::cmpl | Op::cmpli | Op::fcmpu | Op::fcmpo => i.bits(23, 3) == cr,
        Op::mtcrf => i.bits(12, 8) & (0x80 >> cr) != 0,
        Op::crand | Op::crandc | Op::creqv | Op::crnand | Op::crnor | Op::cror | Op::crorc | Op::crxor | Op::mcrf => {
            i.rt() / 4 == cr
        }
        _ if i.rc() => {
            let field = match i.op.info().group {
                ppc::Group::f => 1,
                ppc::Group::v => 6,
                _ => 0,
            };
            field == cr
        }
        _ => false,
    }
}

/// How far above the `bctr` to look for the `cmplwi` that bounds the table.
const BOUND_WINDOW: u32 = 128;

struct Bound {
    reg: u32,
    count: u32,
    default: Option<u32>,
    compare_at: u32,
}

/// Finds the `cmplwi crN, r, max` + `bgt crN, default` guard above `end`.
fn find_bound(code: &Code, end: u32) -> Option<Bound> {
    let mut addr = end;
    let mut guard: Option<(u32, Option<u32>)> = None; // (cr field, default)
    for _ in 0..BOUND_WINDOW {
        addr = addr.checked_sub(4)?;
        let i = code.instr(addr)?;
        if is_call(&i) {
            return None;
        }
        match guard {
            None => {
                // bgt crN, label (branch if CR[4N+1] set) or bgtlr crN.
                let bo = i.rt();
                let bi = i.ra();
                let if_true = bo & 0b11100 == 0b01100;
                if matches!(i.op, Op::bcx | Op::bclrx) && !i.lk() && if_true && bi % 4 == 1 {
                    let default = if i.op == Op::bcx { i.branch_target() } else { None };
                    guard = Some((bi / 4, default));
                } else if is_unconditional_transfer(&i) {
                    return None;
                }
            }
            Some((cr, default)) => {
                if i.op == Op::cmpli && i.bits(23, 3) == cr && !i.bit(21) {
                    return Some(Bound { reg: i.ra(), count: i.uimm16() + 1, default, compare_at: addr });
                }
                if writes_cr_field(&i, cr) || (is_branch(&i) && !matches!(i.op, Op::bcx)) {
                    return None;
                }
            }
        }
    }
    None
}

fn read_labels(code: &Code, kind: Kind, table: u32, base: u32, count: u32) -> Option<Vec<u32>> {
    (0..count)
        .map(|n| match kind {
            Kind::Absolute => code.word(table + n * 4),
            Kind::Computed { shift } => code.u8(table + n).map(|b| base.wrapping_add((b as u32) << shift)),
            Kind::ByteOffset => code.u8(table + n).map(|b| base.wrapping_add(b as u32)),
            Kind::ShortOffset => code.u16(table + n * 2).map(|h| base.wrapping_add(h as u32)),
        })
        .collect()
}

/// For an absolute table placed directly after its `bctr`, reads entries while
/// they're plausible labels (aligned code addresses in the same section, near the
/// `bctr`), and never past the lowest forward label. The table is accepted only if
/// it ends exactly where one of its own labels begins: the case code that follows it.
fn inline_table_labels(code: &Code, bctr: u32, table: u32) -> Option<Vec<u32>> {
    if table != bctr + 4 {
        return None;
    }
    let &(section_begin, section_end) = code.ranges.iter().find(|&&(b, e)| (b..e).contains(&bctr))?;
    let mut labels = Vec::new();
    let mut forward_limit = u32::MAX;
    let mut addr = table;
    while addr < forward_limit && labels.len() <= 4096 {
        let Some(word) = code.word(addr) else { break };
        let plausible = word % 4 == 0 && (section_begin..section_end).contains(&word) && word.abs_diff(bctr) < 0x10_0000;
        if !plausible {
            break;
        }
        if word > bctr {
            forward_limit = forward_limit.min(word);
        }
        labels.push(word);
        addr += 4;
    }
    (!labels.is_empty() && labels.contains(&addr)).then_some(labels)
}

fn classify(code: &Code, bctr: &Instr) -> BctrKind {
    let window = Window::before(code, bctr.address);
    let Some(mtctr_at) = window.instrs.iter().rposition(is_mtctr) else {
        // CTR was set on another path (e.g. a shared tail-call block): an indirect jump through CTR.
        return BctrKind::TailCallViaRegister;
    };
    let src = window.instrs[mtctr_at].rt();
    let bound = find_bound(code, bctr.address);
    let value = window.eval(src, mtctr_at, 0, bound.as_ref().map(|b| (b.reg, b.compare_at)));
    match table_shape(&value) {
        Some((kind, table, sel)) => {
            let Some(Bound { reg, count, default, .. }) = bound else {
                // An unbounded table outside code is a function-pointer dispatch table.
                if !code.is_code(table & !3) {
                    return BctrKind::TailCallViaTable { table };
                }
                if let (Kind::Absolute, Some(sel)) = (kind, sel)
                    && let Some(labels) = inline_table_labels(code, bctr.address, table)
                {
                    return BctrKind::Switch(SwitchTable {
                        bctr: bctr.address,
                        kind,
                        table,
                        reg: sel,
                        default: None,
                        labels,
                        inferred_bound: true,
                    });
                }
                return BctrKind::TableWithoutBound { table };
            };
            if sel != Some(reg) {
                return BctrKind::Unknown {
                    reason: format!("table index r{sel:?} is not the bounds-checked r{reg}"),
                };
            }
            if count > 4096 {
                return BctrKind::Unknown { reason: format!("implausible case count {count}") };
            }
            let Some(labels) = read_labels(code, kind, table, table_base(&value), count) else {
                return BctrKind::Unknown { reason: format!("table at 0x{table:08X} is outside the image") };
            };
            if let Some(bad) = labels.iter().find(|&&l| !code.is_code(l)) {
                return BctrKind::Unknown { reason: format!("label 0x{bad:08X} is not in code") };
            }
            BctrKind::Switch(SwitchTable { bctr: bctr.address, kind, table, reg, default, labels, inferred_bound: false })
        }
        None => match value {
            Val::Load { .. } => BctrKind::TailCallViaPointer,
            Val::Input(_) => BctrKind::TailCallViaRegister,
            other => BctrKind::Unknown { reason: format!("CTR value {other:?}") },
        },
    }
}

/// Classifies every unconditional `bctr` (no link) in the code.
pub fn find_all(code: &Code) -> Vec<BctrSite> {
    code.words()
        .filter_map(|(addr, word)| Instr::decode(addr, word))
        .filter(|i| i.op == Op::bcctrx && !i.lk() && bo_always(i))
        .map(|i| BctrSite { address: i.address, kind: classify(code, &i) })
        .collect()
}
