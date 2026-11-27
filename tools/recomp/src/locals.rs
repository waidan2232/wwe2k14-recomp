//! Registers as host locals.
//!
//! Translated naively, every guest register lives in the context struct, so
//! each call forces every register the caller touched into memory and back,
//! and each function saves and restores the registers the ABI preserves on
//! the guest stack. Here a function keeps the registers it uses in host
//! locals instead, and the context carries a register only where one function
//! really hands it to another.
//!
//! Nothing is assumed about the calling convention. What crosses a call is
//! computed from the code:
//!
//! - `ins`: the registers a function reads before writing them, itself or
//!   through its callees. A caller stores its local to the context before the
//!   call ("flush"); the function loads its local from the context on entry.
//! - `outs`: the registers whose context slot a function may leave changed.
//!   A caller that reads such a register after the call reloads its local.
//! - exit liveness: the registers some caller reads after a function returns.
//!   The function stores those it wrote before it returns.
//!
//! The three depend on one another across the whole program, so they are
//! solved together to a fixed point.
//!
//! **Calls through pointers** (and calls to native code, which may call back
//! through one) can't know their callee, so they use a fixed contract, the
//! ABI's: the callee reads none of these registers from its caller, and
//! returns with the preserved ones (`r14`-`r31`, `f14`-`f31`, `v14`-`v31`,
//! `v64`-`v127`) unchanged. Every function's summary is checked against
//! that contract. One that doesn't meet it (an exception-handling funclet
//! reads its parent's `r31`) can still be called directly, where the caller
//! sees its real summary, but its entry in the indirect-call table is a fatal
//! error that names it.
//!
//! **Preserved registers.** A function that saves a register on entry and
//! restores it at every exit (`std r31, -0x10(r1)` ... `ld r31, -0x10(r1)`, or
//! the same through the `__savegprlr`/`__restgprlr` helpers, which are
//! translated in place) returns it unchanged. When that pattern is proven for
//! a register, the save and the restore are not emitted at all: the local
//! starts fresh, and the caller's own local is untouched by construction. If
//! the function must still put the register in the context for a callee, it
//! keeps the context slot's old contents in a host local and puts them back
//! at every exit. A register whose pattern can't be proven is handled by the
//! general rules above, with its save and restore emitted as ordinary code.
//!
//! Functions that call `setjmp`, and `longjmp` itself, keep every register in
//! the context: a `longjmp` rewrites the registers of a frame that is still
//! running.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ppc::{Instr, Op};

use crate::emit::{Body, Exit, Item, Program, Target, Term};
use crate::regs::{self, Effect, Reg, RegSet, CR, CTR, FPR, GPR, LOCALIZABLE, SAVED_BY_CALLEE, VR, XER_OV, XER_SO};

/// What the program around the functions looks like.
pub struct Options<'a> {
    /// Keep registers in locals at all. Off: every register stays in the context.
    pub enabled: bool,
    /// Functions that can be called through a pointer: by recompiled code
    /// (vtables, callbacks) or by native code.
    pub address_taken: &'a BTreeSet<u32>,
    /// Functions whose caller reads every register when they return (the
    /// instruction tests' entry points).
    pub observed: &'a BTreeSet<u32>,
    /// Entry points that run native code (import thunks, replaced functions).
    /// Native code may call any address-taken function, so a call to one is
    /// treated as a call through a pointer.
    pub native: &'a BTreeSet<u32>,
    /// Functions that are not taken to restore a preserved register without
    /// proof (config `[locals] untrusted`): one that tripped the exit check.
    pub untrusted: &'a BTreeSet<u32>,
    /// Native entry points that never return to their caller (`KeBugCheck`,
    /// `RtlUnwind`, ...).
    pub noreturn: &'a BTreeSet<u32>,
}

/// What a call site needs to know about its callee.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Summary {
    pub ins: RegSet,
    pub outs: RegSet,
}

/// The context traffic around one call or exit.
#[derive(Clone, Copy, Default, Debug)]
pub struct Site {
    /// Locals stored to the context before it.
    pub flush: RegSet,
    /// Locals loaded from the context after it (calls only).
    pub reload: RegSet,
}

/// How one function is rendered with locals.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Registers held in locals.
    pub locals: RegSet,
    /// Locals loaded from the context on entry.
    pub load: RegSet,
    /// Preserved registers whose context slot the function writes (for a
    /// callee): the slot's contents are kept on entry and put back at exits.
    pub saved: RegSet,
    /// Registers the function is taken to preserve without proof (see
    /// `State::trusted`): each exit checks the local against the entry value
    /// and stops the game if they differ.
    pub checked: RegSet,
    /// Per block, per item: a save or restore of a preserved register, not emitted.
    pub skip: Vec<Vec<bool>>,
    /// Per block, per item: the traffic around a call or a conditional exit.
    pub sites: Vec<Vec<Site>>,
    /// Per block: the stores before the exit that ends it.
    pub exits: Vec<RegSet>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Plain,
    /// Stores a preserved register's entry value in the frame.
    Save(Reg),
    /// Loads it back before an exit.
    Restore(Reg),
}

/// One function's fixed facts.
struct Func<'b> {
    body: &'b Body,
    /// Per block, per item: its effect on the registers (for a call or a
    /// conditional exit: its condition and its target register).
    steps: Vec<Vec<Effect>>,
    roles: Vec<Vec<Role>>,
    /// Per block, per item: the preserved register it stores in the stack
    /// frame (`std rN, D(r1)`, `stfd fN, D(r1 or r12)`), if it is such a store.
    stack_stores: Vec<Vec<Option<Reg>>>,
    /// Per block: the terminator's own effect.
    terms: Vec<Effect>,
    index: HashMap<u32, usize>,
    /// The registers it holds in locals: every localizable one its code
    /// mentions. Empty when it keeps everything in the context.
    locals: RegSet,
    /// Localizable registers its code writes, apart from recognised restores.
    written: RegSet,
    /// Registers with a recognised save and restore.
    pattern: RegSet,
    address_taken: bool,
    observed: bool,
}

/// The preserved register an instruction stores in the stack frame, if any.
fn stack_store(i: &Instr) -> Option<Reg> {
    match i.op {
        Op::std if i.ra() == 1 && i.rt() >= 14 => Some(GPR + i.rt() as usize),
        Op::stfd if i.rt() >= 14 && (i.ra() == 1 || i.ra() == 12) => Some(FPR + i.rt() as usize),
        // As the `__savevmx` helpers do it: `stvx vN, r11, r12`, with r12 the frame.
        Op::stvx | Op::stvxl | Op::stvx128 | Op::stvxl128
            if matches!(i.vd(), 14..=31 | 64..=127) && [i.ra(), i.rb()].iter().any(|&r| r == 1 || r == 12) =>
        {
            Some(VR + i.vd() as usize)
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Flow {
    /// Registers whose value is read later (by this function or, after it
    /// returns, by a caller).
    live: RegSet,
    /// Registers that must hold a valid value here: read, or stored to the
    /// context, before this function writes them.
    need: RegSet,
    /// Trusted registers stored to the stack frame before this function
    /// writes them: their entry value is saved.
    stored: RegSet,
}

impl Flow {
    fn join(self, o: Flow) -> Flow {
        Flow { live: self.live.or(o.live), need: self.need.or(o.need), stored: self.stored.or(o.stored) }
    }

    fn apply(&mut self, fx: Effect) {
        self.live = fx.uses.or(self.live.minus(fx.defs));
        self.need = fx.uses.or(self.need.minus(fx.defs));
        self.stored = self.stored.minus(fx.defs);
    }
}

/// The solver's state: one entry per function, plus the union for calls
/// through pointers.
struct State {
    summary: Vec<Summary>,
    /// Registers some caller reads after the function returns.
    exit_live: Vec<RegSet>,
    /// Preserved registers still handled by their pattern.
    kept: Vec<RegSet>,
    /// Preserved registers whose save and restore couldn't be proven, but
    /// which the function's own code reads before writing only to store them
    /// in its stack frame, and which no tail callee takes or changes. The
    /// function is taken to restore them, as the ABI says, and that is
    /// checked: each exit compares the register with its entry value and
    /// stops the game if they differ. So callers needn't supply or reload it.
    trusted: Vec<RegSet>,
    /// The contract for calls through pointers (`INDIRECT`).
    indirect: Summary,
    /// Registers read after some call through a pointer or to native code:
    /// any function may be the callee, so every function's exits honour them.
    indirect_exit_live: RegSet,
    /// The state for asking what a function's own code does: no caller reads
    /// anything after it returns, not even the test harness.
    own_code: bool,
}

/// What a call through a pointer assumes of its callee, and what every
/// function is checked against before it gets a real entry in the
/// indirect-call table: the ABI. It reads none of the localizable registers
/// from its caller (the sticky overflow bit aside, which nothing in the game
/// sets), and it returns with the registers the ABI preserves unchanged.
const INDIRECT: Summary = Summary {
    ins: RegSet::EMPTY.with(XER_SO),
    outs: LOCALIZABLE.minus(
        RegSet::EMPTY
            .with_range(GPR + 14, GPR + 31)
            .with_range(FPR + 14, FPR + 31)
            .with_range(VR + 14, VR + 31)
            .with_range(VR + 64, VR + 127)
            .with(XER_SO)
            .with(XER_OV),
    ),
};

/// Registers the contract check leaves out: the vector registers and `cr1`.
///
/// The game's hand-written vector code (audio and animation kernels, which
/// are reached through pointers) reads vector registers it hasn't written:
/// history lanes on a loop's first pass, lanes it masks off afterwards. To the
/// analysis that is a register taken from the caller, and by that measure
/// about fifty address-taken kernels would get fatal table entries. They get
/// real entries instead, and read whatever the context holds. That is what
/// they read on the console too, as far as any caller compiled to the ABI
/// could tell: no such caller passes a vector register through a pointer
/// outside the convention, or expects one back. (Supplying them properly, by
/// taking the union over the address-taken functions, was tried: it puts
/// about 300,000 vector stores and loads back around calls.)
///
/// The general registers are checked: one that a function takes from its
/// caller (`r12` for an exception-handling funclet) is a real protocol, and
/// a call through a pointer would not carry it.
const UNCHECKED: RegSet = RegSet::EMPTY.with_range(VR, VR + 127).with(CR + 1).and(LOCALIZABLE);

/// One pass over a function with the current state.
#[derive(Default)]
struct Pass {
    need_in: RegSet,
    /// Trusted registers whose entry value reaches a store to the stack frame.
    stored_in: RegSet,
    /// Registers a tail callee known by address takes or changes.
    tail_touches: RegSet,
    /// Preserved registers a tail callee reads: the entry value, passed on.
    tail_kept_in: RegSet,
    /// Locals stored to the context somewhere.
    flushed: RegSet,
    /// Context slots callees may change (calls that return here).
    callee_outs: RegSet,
    /// Context slots tail callees may change.
    tail_outs: RegSet,
    /// Liveness after each call, for the callee's exits (`None`: through a
    /// pointer), with the call's address (0 for a tail call).
    after: Vec<(Option<usize>, RegSet, u32)>,
    plan: Plan,
}

struct Solver<'a> {
    /// Per function: can it return to its caller at all? A function that
    /// always ends in a fatal error or a throw can't, and what its caller's
    /// code does after the call is never reached: the compiler lays unrelated
    /// code there, whose register reads would otherwise look like uses of
    /// what the callee left.
    returns: Vec<bool>,
    funcs: Vec<Func<'a>>,
    by_addr: HashMap<u32, usize>,
    setjmp: &'a BTreeSet<u32>,
    native: &'a BTreeSet<u32>,
    noreturn: &'a BTreeSet<u32>,
}

fn real_exit(e: &Exit) -> bool {
    matches!(e, Exit::Return | Exit::Tail(_) | Exit::FallInto(_))
}

/// A D-form or DS-form load or store: its base register, displacement and size in bytes.
fn memory_access(i: &Instr) -> Option<(u32, i64, i64)> {
    use Op::*;
    let d = i.simm16() as i64;
    match i.op {
        lbz | lbzu | stb | stbu => Some((i.ra(), d, 1)),
        lhz | lhzu | lha | lhau | sth | sthu => Some((i.ra(), d, 2)),
        lwz | lwzu | stw | stwu | lfs | lfsu | stfs | stfsu => Some((i.ra(), d, 4)),
        lfd | lfdu | stfd | stfdu => Some((i.ra(), d, 8)),
        lwa => Some((i.ra(), i.operand("ds")?, 4)),
        ld | ldu | std | stdu => Some((i.ra(), i.operand("ds")?, 8)),
        _ => None,
    }
}

/// The registers a function provably saves on entry and restores at every
/// exit, with the items that do it.
///
/// The proof is local and strict; anything it can't follow leaves the
/// register to the general rules:
/// - The saves are in the entry block, which nothing branches back to, before
///   any call and before the register is written: `std rN, D(r1)` or
///   `stfd fN, D(r1 or r12)`, with `r1` and `r12` (after `addi r12, r1, K`)
///   tracked relative to the stack pointer on entry.
/// - The function changes `r1` only by one `stwu r1, -S(r1)` in the entry
///   block and by `addi r1, r1, S` in blocks that then leave the function. So
///   after that `addi` (or anywhere, in a function that never moves `r1`) the
///   stack pointer is the entry one.
/// - Before every exit, in the exit's own block, the register is loaded back
///   from the slot it was saved to, and nothing touches it afterwards.
/// - No other load or store through `r1` can reach the slot.
fn recognise(body: &Body, steps: &[Vec<Effect>]) -> (RegSet, Vec<(usize, usize, Role)>) {
    let none = (RegSet::EMPTY, Vec::new());
    let Some(entry) = body.blocks.first().filter(|b| b.start == body.begin) else { return none };
    let back_to_entry = body.blocks.iter().any(|b| match &b.term {
        Term::Jump(t) | Term::Resume(t) => *t == body.begin,
        Term::Branch { taken, next, .. } => *taken == body.begin || *next == body.begin,
        Term::Switch { labels, default, .. } => labels.contains(&body.begin) || *default == Some(body.begin),
        Term::Exit(_) => false,
    });
    if back_to_entry {
        return none;
    }
    let r1 = GPR + 1;
    let r12 = GPR + 12;

    // The prologue.
    let mut sp: Option<i64> = Some(0);
    let mut base12: Option<i64> = None;
    let mut frame: Option<i64> = None;
    let mut push_at: Option<usize> = None;
    let mut written = RegSet::EMPTY;
    let mut saves: BTreeMap<Reg, (usize, i64)> = BTreeMap::new();
    for (ii, item) in entry.items.iter().enumerate() {
        let Item::Code { instr, .. } = item else { break };
        let fx = steps[0][ii];
        let mut tracked = false;
        if let Some(i) = instr {
            let d = i.simm16() as i64;
            match i.op {
                Op::std if i.ra() == 1 && i.rt() >= 14 => {
                    let reg = GPR + i.rt() as usize;
                    if let (Some(s), Some(ds)) = (sp, i.operand("ds"))
                        && !written.contains(reg)
                    {
                        saves.entry(reg).or_insert((ii, s + ds));
                    }
                }
                Op::stfd if i.rt() >= 14 && (i.ra() == 1 || i.ra() == 12) => {
                    let reg = FPR + i.rt() as usize;
                    if let Some(s) = if i.ra() == 1 { sp } else { base12 }
                        && !written.contains(reg)
                    {
                        saves.entry(reg).or_insert((ii, s + d));
                    }
                }
                Op::stwu if i.rt() == 1 && i.ra() == 1 => {
                    tracked = true;
                    if frame.is_none() && sp == Some(0) && d < 0 {
                        frame = Some(-d);
                        sp = Some(d);
                        push_at = Some(ii);
                    } else {
                        sp = None;
                    }
                }
                Op::addi if i.rt() == 12 && i.ra() == 1 => {
                    tracked = true;
                    base12 = sp.map(|s| s + d);
                }
                _ => {}
            }
        }
        if !tracked {
            if fx.writes.contains(r1) {
                sp = None;
            }
            if fx.writes.contains(r12) {
                base12 = None;
            }
        }
        written = written.or(fx.writes);
    }
    if saves.is_empty() {
        return none;
    }

    // Every other change of r1 is the matching pop, in a block that then leaves.
    for (bi, block) in body.blocks.iter().enumerate() {
        for (ii, item) in block.items.iter().enumerate() {
            if !steps[bi][ii].writes.contains(r1) || (bi == 0 && Some(ii) == push_at) {
                continue;
            }
            let pop = matches!(item, Item::Code { instr: Some(i), .. }
                if i.op == Op::addi && i.rt() == 1 && i.ra() == 1 && frame == Some(i.simm16() as i64));
            if !pop || !matches!(&block.term, Term::Exit(e) if real_exit(e)) {
                return none;
            }
        }
    }

    // The restores before each exit.
    let mut regs = saves.keys().fold(RegSet::EMPTY, |s, &r| s.with(r));
    let mut restores: Vec<(usize, usize, Reg)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let leaves = matches!(&block.term, Term::Exit(e) if real_exit(e))
            || block.items.iter().any(|i| matches!(i, Item::CondExit { exit, .. } if real_exit(exit)));
        if !leaves {
            continue;
        }
        let mut sp: Option<i64> = if frame.is_none() { Some(0) } else { None };
        let mut base12: Option<i64> = None;
        // Register -> (item, slot) of its load, while nothing has touched it since.
        let mut loaded: BTreeMap<Reg, (usize, i64)> = BTreeMap::new();
        let at_exit = |loaded: &BTreeMap<Reg, (usize, i64)>, regs: &mut RegSet, restores: &mut Vec<(usize, usize, Reg)>| {
            for r in regs.iter().collect::<Vec<_>>() {
                match loaded.get(&r) {
                    Some(&(ii, slot)) if slot == saves[&r].1 => restores.push((bi, ii, r)),
                    _ => regs.remove(r),
                }
            }
        };
        for (ii, item) in block.items.iter().enumerate() {
            let fx = steps[bi][ii];
            match item {
                Item::Code { instr, .. } => {
                    let mut restored: Option<(Reg, i64)> = None;
                    let mut tracked = false;
                    if let Some(i) = instr {
                        let d = i.simm16() as i64;
                        match i.op {
                            Op::addi if i.rt() == 1 && i.ra() == 1 => {
                                // The pop (checked above): from the body's frame to the entry stack pointer.
                                sp = if sp.is_none() { Some(0) } else { None };
                                loaded.clear();
                            }
                            Op::ld if i.ra() == 1 && i.rt() >= 14 => {
                                if let (Some(s), Some(ds)) = (sp, i.operand("ds")) {
                                    restored = Some((GPR + i.rt() as usize, s + ds));
                                }
                            }
                            Op::lfd if i.rt() >= 14 && (i.ra() == 1 || i.ra() == 12) => {
                                if let Some(s) = if i.ra() == 1 { sp } else { base12 } {
                                    restored = Some((FPR + i.rt() as usize, s + d));
                                }
                            }
                            Op::addi if i.rt() == 12 && i.ra() == 1 => {
                                tracked = true;
                                base12 = sp.map(|s| s + d);
                            }
                            _ => {}
                        }
                    }
                    if !tracked && fx.writes.contains(r12) {
                        base12 = None;
                    }
                    for r in fx.uses.or(fx.writes).iter() {
                        if restored.map(|x| x.0) != Some(r) {
                            loaded.remove(&r);
                        }
                    }
                    if let Some((r, slot)) = restored {
                        loaded.insert(r, (ii, slot));
                    }
                }
                Item::Call { .. } => {
                    loaded.clear();
                    base12 = None;
                }
                Item::CondExit { exit, .. } => {
                    for r in fx.uses.or(fx.writes).iter() {
                        loaded.remove(&r);
                    }
                    if real_exit(exit) {
                        at_exit(&loaded, &mut regs, &mut restores);
                    }
                }
            }
        }
        if matches!(&block.term, Term::Exit(e) if real_exit(e)) {
            at_exit(&loaded, &mut regs, &mut restores);
        }
    }

    // Nothing else reaches a save slot through r1. An access `D(r1)` is at
    // `D` from the entry stack pointer before the push and after the pop, and
    // at `D - frame` in between; both are checked wherever it sits.
    let mut role_of: HashMap<(usize, usize), Reg> = HashMap::new();
    for (&r, &(ii, _)) in &saves {
        role_of.insert((0, ii), r);
    }
    for &(bi, ii, r) in &restores {
        role_of.insert((bi, ii), r);
    }
    for (bi, block) in body.blocks.iter().enumerate() {
        for (ii, item) in block.items.iter().enumerate() {
            let Item::Code { instr: Some(i), .. } = item else { continue };
            let Some((1, d, size)) = memory_access(i) else { continue };
            let own = role_of.get(&(bi, ii)).copied();
            for r in regs.iter().collect::<Vec<_>>() {
                if own == Some(r) {
                    continue;
                }
                let slot = saves[&r].1;
                let reaches = |at: i64| at < slot + 8 && at + size > slot;
                if reaches(d) || frame.is_some_and(|f| reaches(d - f)) {
                    regs.remove(r);
                }
            }
        }
    }

    let mut roles: Vec<(usize, usize, Role)> = Vec::new();
    for (&r, &(ii, _)) in &saves {
        if regs.contains(r) {
            roles.push((0, ii, Role::Save(r)));
        }
    }
    for &(bi, ii, r) in &restores {
        if regs.contains(r) {
            roles.push((bi, ii, Role::Restore(r)));
        }
    }
    (regs, roles)
}

impl<'b> Func<'b> {
    fn new(body: &'b Body, opts: &Options) -> Func<'b> {
        let mut steps: Vec<Vec<Effect>> = Vec::with_capacity(body.blocks.len());
        let mut terms: Vec<Effect> = Vec::with_capacity(body.blocks.len());
        let target_use = |t: &Target| match t {
            Target::Ctr => Effect { uses: RegSet::EMPTY.with(CTR), ..Effect::default() },
            _ => Effect::default(),
        };
        let exit_use = |e: &Exit| match e {
            Exit::Tail(t) => target_use(t),
            _ => Effect::default(),
        };
        for block in &body.blocks {
            steps.push(
                block
                    .items
                    .iter()
                    .map(|item| match item {
                        Item::Code { lines, .. } => Effect::of(&lines.join(" ")),
                        Item::Call { target, cond, .. } => {
                            cond.as_deref().map(Effect::of).unwrap_or_default().then(target_use(target))
                        }
                        Item::CondExit { cond, exit, .. } => Effect::of(cond).then(exit_use(exit)),
                    })
                    .collect(),
            );
            terms.push(match &block.term {
                Term::Jump(_) | Term::Resume(_) => Effect::default(),
                Term::Branch { cond, .. } => Effect::of(cond),
                Term::Switch { reg, .. } => Effect { uses: RegSet::EMPTY.with(GPR + *reg as usize), ..Effect::default() },
                Term::Exit(e) => exit_use(e),
            });
        }
        let index: HashMap<u32, usize> = body.blocks.iter().enumerate().map(|(i, b)| (b.start, i)).collect();
        let all_ctx = !opts.enabled
            || !body.resume_sites.is_empty()
            || body.blocks.iter().any(|b| matches!(b.term, Term::Exit(Exit::Longjmp)));
        let (pattern, role_list) = recognise(body, &steps);
        let pattern = pattern.and(SAVED_BY_CALLEE);
        debug_assert!(pattern.is_subset(&RegSet::EMPTY.with_range(GPR + 14, GPR + 31).with_range(FPR + 14, FPR + 31)));
        let mut roles: Vec<Vec<Role>> = body.blocks.iter().map(|b| vec![Role::Plain; b.items.len()]).collect();
        for (bi, ii, role) in role_list {
            roles[bi][ii] = role;
        }
        let stack_stores: Vec<Vec<Option<Reg>>> = body
            .blocks
            .iter()
            .map(|b| b.items.iter().map(|item| match item {
                Item::Code { instr: Some(i), .. } => stack_store(i),
                _ => None,
            }).collect())
            .collect();
        let mut mentioned = RegSet::EMPTY;
        let mut written = RegSet::EMPTY;
        for (bi, block) in steps.iter().enumerate() {
            for (ii, fx) in block.iter().enumerate() {
                mentioned = mentioned.or(fx.uses).or(fx.writes);
                if !matches!(roles[bi][ii], Role::Restore(_)) {
                    written = written.or(fx.writes);
                }
            }
            mentioned = mentioned.or(terms[bi].uses).or(terms[bi].writes);
            written = written.or(terms[bi].writes);
        }
        Func {
            locals: if all_ctx { RegSet::EMPTY } else { mentioned.and(LOCALIZABLE) },
            written: written.and(LOCALIZABLE),
            pattern,
            address_taken: opts.address_taken.contains(&body.begin),
            observed: opts.observed.contains(&body.begin),
            body,
            steps,
            roles,
            stack_stores,
            terms,
            index,
        }
    }

    /// The registers a restore writes, for those no longer handled by their pattern.
    fn restored(&self, kept: RegSet) -> RegSet {
        let mut out = RegSet::EMPTY;
        for roles in &self.roles {
            for role in roles {
                if let Role::Restore(r) = role
                    && !kept.contains(*r)
                {
                    out.insert(*r);
                }
            }
        }
        out
    }
}

impl Solver<'_> {
    /// Does a call to `target` come back?
    fn comes_back(&self, target: &Target) -> bool {
        match *target {
            Target::Direct(t) if self.noreturn.contains(&t) => false,
            Target::Direct(t) if !self.native.contains(&t) => self.by_addr.get(&t).is_none_or(|&f| self.returns[f]),
            _ => true,
        }
    }

    /// Finds the functions that can return: the least set in which a
    /// function has a path from its entry to a return, or to a tail call of a
    /// function in the set, that passes no unconditional call to a function
    /// outside it.
    fn find_returns(&mut self) {
        loop {
            let mut changed = false;
            for fi in 0..self.funcs.len() {
                if self.returns[fi] {
                    continue;
                }
                let f = &self.funcs[fi];
                let leaves = |e: &Exit| match e {
                    Exit::Return => true,
                    Exit::Tail(t) => self.comes_back(t),
                    Exit::FallInto(next) => self.comes_back(&Target::Direct(*next)),
                    Exit::Longjmp | Exit::FellOff(_) => false,
                };
                let mut seen = vec![false; f.body.blocks.len()];
                let mut work = vec![0usize];
                let mut returns = false;
                'blocks: while let Some(bi) = work.pop() {
                    if bi >= seen.len() || std::mem::replace(&mut seen[bi], true) {
                        continue;
                    }
                    let block = &f.body.blocks[bi];
                    for item in &block.items {
                        match item {
                            Item::Call { target, cond: None, .. } if !self.comes_back(target) => continue 'blocks,
                            Item::CondExit { exit, .. } if leaves(exit) => returns = true,
                            _ => {}
                        }
                    }
                    let mut go = |t: &u32| work.extend(f.index.get(t));
                    match &block.term {
                        Term::Jump(t) | Term::Resume(t) => go(t),
                        Term::Branch { taken, next, .. } => {
                            go(taken);
                            go(next);
                        }
                        Term::Switch { labels, default, .. } => labels.iter().chain(default.iter()).for_each(go),
                        Term::Exit(e) => returns |= leaves(e),
                    }
                }
                if returns {
                    self.returns[fi] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn callee(&self, st: &State, target: &Target) -> (Summary, Option<usize>) {
        match *target {
            Target::Direct(t) if !self.native.contains(&t) => match self.by_addr.get(&t) {
                // `setjmp` copies the preserved registers into its buffer for
                // `longjmp` to put back. That is not a use of the caller's
                // values: frames above keep theirs in host locals, which the
                // unwind leaves alone, and the function that calls `setjmp`
                // keeps its own in the context.
                Some(&f) if self.setjmp.contains(&t) => (Summary { ins: RegSet::EMPTY, outs: st.summary[f].outs }, Some(f)),
                Some(&f) => (st.summary[f], Some(f)),
                None => (st.indirect, None),
            },
            _ => (st.indirect, None),
        }
    }

    fn exit_live(&self, st: &State, fi: usize) -> RegSet {
        if self.funcs[fi].observed && !st.own_code {
            LOCALIZABLE
        } else {
            st.exit_live[fi].or(st.indirect_exit_live).and(LOCALIZABLE)
        }
    }

    /// Solves one function's liveness against the current state.
    fn pass(&self, st: &State, fi: usize) -> Pass {
        let f = &self.funcs[fi];
        // Registers the function returns unchanged: proven (their save and
        // restore are not emitted) or trusted (checked at each exit).
        let trusted = st.trusted[fi];
        let kept = st.kept[fi].or(trusted);
        let proven = st.kept[fi];
        let locals = f.locals;
        let written = f.written.or(f.restored(proven));
        let exit_live = self.exit_live(st, fi);
        // What a return stores: the locals it wrote that a caller may read.
        let flush_exit = locals.and(written).and(exit_live).minus(kept);
        let n = f.body.blocks.len();
        let mut at_start = vec![Flow::default(); n];
        let mut out = Pass::default();
        out.plan.sites = f.body.blocks.iter().map(|b| vec![Site::default(); b.items.len()]).collect();
        out.plan.exits = vec![RegSet::EMPTY; n];

        // Flow at an exit, and what it stores first.
        let exit_flow = |e: &Exit, out: &mut Pass| -> (Flow, RegSet) {
            let tail = |target: &Target, out: &mut Pass| {
                let (s, callee) = self.callee(st, target);
                let flush = locals.and(written).and(s.ins.or(exit_live)).minus(kept);
                out.tail_kept_in = out.tail_kept_in.or(s.ins.and(kept));
                if callee.is_some() {
                    out.tail_touches = out.tail_touches.or(s.ins).or(s.outs);
                }
                out.tail_outs = out.tail_outs.or(s.outs);
                out.flushed = out.flushed.or(flush);
                out.after.push((callee, exit_live, 0));
                (Flow { live: s.ins.or(exit_live), need: s.ins.minus(kept).or(flush), stored: RegSet::EMPTY }, flush)
            };
            match e {
                Exit::Return => {
                    out.flushed = out.flushed.or(flush_exit);
                    (Flow { live: exit_live, need: flush_exit, stored: RegSet::EMPTY }, flush_exit)
                }
                Exit::Tail(t) => tail(t, out),
                Exit::FallInto(next) => tail(&Target::Direct(*next), out),
                Exit::Longjmp | Exit::FellOff(_) => (Flow::default(), RegSet::EMPTY),
            }
        };
        let block_flow = |bi: usize, at_start: &[Flow], out: &mut Pass| -> Flow {
            let block = &f.body.blocks[bi];
            let succ = |t: &u32| f.index.get(t).map(|&i| at_start[i]).unwrap_or_default();
            let mut flow = match &block.term {
                Term::Jump(t) | Term::Resume(t) => succ(t),
                Term::Branch { taken, next, .. } => succ(taken).join(succ(next)),
                Term::Switch { labels, default, .. } => {
                    labels.iter().chain(default.iter()).fold(Flow::default(), |a, l| a.join(succ(l)))
                }
                Term::Exit(e) => {
                    let (flow, flush) = exit_flow(e, out);
                    out.plan.exits[bi] = flush;
                    flow
                }
            };
            flow.apply(f.terms[bi]);
            for ii in (0..block.items.len()).rev() {
                let fx = f.steps[bi][ii];
                match &block.items[ii] {
                    Item::Code { .. } => {
                        if let Role::Save(r) | Role::Restore(r) = f.roles[bi][ii]
                            && proven.contains(r)
                        {
                            continue;
                        }
                        match f.stack_stores[bi][ii] {
                            // Storing a trusted register in the frame is not a use of the caller's value.
                            Some(r) if trusted.contains(r) => {
                                flow.apply(Effect { uses: fx.uses.minus(RegSet::EMPTY.with(r)), ..fx });
                                flow.stored.insert(r);
                            }
                            _ => flow.apply(fx),
                        }
                    }
                    Item::Call { target, addr, cond, .. } => {
                        let (s, callee) = self.callee(st, target);
                        let comes_back = cond.is_some() || self.comes_back(target);
                        if !comes_back {
                            // Nothing after this call is reached through it.
                            flow = Flow::default();
                        }
                        let after = flow.live;
                        // The callee reads it, or may change it while this
                        // function (or a caller) still wants the value: the
                        // context must hold it going in.
                        let flush = locals.and(written).and(s.ins.or(s.outs.and(after)));
                        let reload = locals.and(s.outs).and(after);
                        flow.live = after.or(s.ins);
                        flow.need = flow.need.or(s.ins).or(flush);
                        flow.apply(fx);
                        out.plan.sites[bi][ii] = Site { flush, reload };
                        out.flushed = out.flushed.or(flush);
                        if comes_back {
                            out.callee_outs = out.callee_outs.or(s.outs);
                            out.after.push((callee, after, *addr));
                        }
                    }
                    Item::CondExit { exit, .. } => {
                        let (leaving, flush) = exit_flow(exit, out);
                        out.plan.sites[bi][ii] = Site { flush, reload: RegSet::EMPTY };
                        flow = flow.join(leaving);
                        flow.apply(fx);
                    }
                }
            }
            flow
        };

        let mut scratch = Pass::default();
        scratch.plan.sites = out.plan.sites.clone();
        scratch.plan.exits = out.plan.exits.clone();
        loop {
            let mut changed = false;
            for bi in (0..n).rev() {
                scratch.after.clear();
                let flow = block_flow(bi, &at_start, &mut scratch);
                if flow != at_start[bi] {
                    at_start[bi] = flow;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // With the flows settled, one more walk records the sites.
        for bi in 0..n {
            block_flow(bi, &at_start, &mut out);
        }
        out.need_in = at_start.first().map(|f| f.need).unwrap_or_default();
        out.stored_in = at_start.first().map(|f| f.stored).unwrap_or_default();
        out.plan.locals = locals;
        out.plan.load = out.need_in.and(locals);
        out
    }

    /// The function's summary from a pass.
    fn summarise(&self, fi: usize, proven: RegSet, trusted: RegSet, p: &Pass) -> Summary {
        let f = &self.funcs[fi];
        let kept = proven.or(trusted);
        // A function without locals writes the context directly.
        let own = if f.locals.is_empty() { f.written.or(f.restored(proven)) } else { p.flushed };
        Summary {
            ins: p.need_in.or(p.tail_kept_in).and(LOCALIZABLE),
            outs: own.or(p.callee_outs).minus(kept).or(p.tail_outs).and(LOCALIZABLE),
        }
    }
}

/// The result for the whole program.
pub struct Analysis {
    /// One plan per function, in the order given (`None`: no locals).
    pub plans: Vec<Option<Plan>>,
    /// Functions a call through a pointer must not reach, with the reason: they
    /// take or return registers that such a call doesn't carry, and nothing in
    /// the program takes their address.
    pub not_indirect: BTreeMap<u32, String>,
    pub report: String,
    /// One line per function: what it takes from and returns to its callers
    /// (in all, and from its own code alone), and how it keeps its registers.
    pub details: String,
    /// Code the analysis can't translate soundly; the build must stop.
    pub errors: Vec<String>,
}

pub fn analyse<'a>(p: &'a Program, bodies: &'a [Body], opts: &'a Options) -> Analysis {
    let funcs: Vec<Func> = bodies.iter().map(|b| Func::new(b, opts)).collect();
    let by_addr: HashMap<u32, usize> = funcs.iter().enumerate().map(|(i, f)| (f.body.begin, i)).collect();
    let n = funcs.len();
    let mut solver =
        Solver { returns: vec![false; n], funcs, by_addr, setjmp: p.setjmp, native: opts.native, noreturn: opts.noreturn };
    if opts.enabled {
        solver.find_returns();
    }
    let solver = solver;
    let mut st = State {
        summary: vec![Summary::default(); n],
        exit_live: vec![RegSet::EMPTY; n],
        kept: solver.funcs.iter().map(|f| f.pattern).collect(),
        // To start with: every preserved register the function stores in its
        // frame somewhere, unless its save and restore are proven outright.
        trusted: solver
            .funcs
            .iter()
            .map(|f| {
                let stored = f.stack_stores.iter().flatten().flatten().fold(RegSet::EMPTY, |s, &r| s.with(r));
                if f.locals.is_empty() || opts.untrusted.contains(&f.body.begin) {
                    RegSet::EMPTY
                } else {
                    stored.and(SAVED_BY_CALLEE).minus(f.pattern)
                }
            })
            .collect(),
        indirect: INDIRECT,
        indirect_exit_live: RegSet::EMPTY,
        own_code: false,
    };
    if !opts.enabled {
        return Analysis {
            plans: vec![None; n],
            not_indirect: BTreeMap::new(),
            report: "registers stay in the context\n".into(),
            details: String::new(),
            errors: Vec::new(),
        };
    }

    let mut rounds = 0;
    loop {
        rounds += 1;
        let mut changed = false;
        // Alternate directions: summaries travel up the call graph and exit
        // liveness down it, and neither follows address order.
        let order: Box<dyn Iterator<Item = usize>> = if rounds % 2 == 1 { Box::new(0..n) } else { Box::new((0..n).rev()) };
        for fi in order {
            let pass = solver.pass(&st, fi);
            // A preserved register the function reads before writing it is
            // not just saved: the caller's value is used.
            let kept = st.kept[fi].minus(pass.need_in);
            // Trusted only while the entry value is stored in the frame before
            // any other use, and no tail callee known by address takes or
            // changes the register (a function split in two hands its
            // registers to its other half that way).
            let trusted = st.trusted[fi].and(pass.stored_in).minus(pass.need_in).minus(pass.tail_touches);
            let summary = solver.summarise(fi, kept, trusted, &pass);
            let summary = Summary { ins: st.summary[fi].ins.or(summary.ins), outs: st.summary[fi].outs.or(summary.outs) };
            if kept != st.kept[fi] || trusted != st.trusted[fi] || summary != st.summary[fi] {
                st.kept[fi] = kept;
                st.trusted[fi] = trusted;
                st.summary[fi] = summary;
                changed = true;
            }
            for (callee, live, _) in pass.after {
                // A register the ABI lets a callee change is not followed
                // into an unknown callee: no caller can count on its value
                // after a call through a pointer (a function whose own code
                // reads one there stops the build, below).
                let (live, slot) = match callee {
                    Some(g) => (live.and(LOCALIZABLE), &mut st.exit_live[g]),
                    None => (live.and(LOCALIZABLE).minus(INDIRECT.outs), &mut st.indirect_exit_live),
                };
                let u = slot.or(live);
                changed |= u != *slot;
                *slot = u;
            }
        }
        if !changed {
            break;
        }
        assert!(rounds < 500, "the register analysis did not settle");
    }

    // What each function takes from its caller by its own code alone (every
    // callee taken to read and change nothing): where an unexpected register
    // in a summary comes from.
    let blank = State {
        summary: vec![Summary::default(); n],
        exit_live: vec![RegSet::EMPTY; n],
        kept: solver.funcs.iter().map(|f| f.pattern).collect(),
        trusted: vec![RegSet::EMPTY; n],
        indirect: INDIRECT,
        indirect_exit_live: RegSet::EMPTY,
        own_code: true,
    };
    let mut details = String::new();
    let mut errors: Vec<String> = Vec::new();

    // Final plans.
    let mut plans: Vec<Option<Plan>> = Vec::with_capacity(n);
    let mut not_indirect: BTreeMap<u32, String> = BTreeMap::new();
    let mut with_locals = 0usize;
    let mut kept_regs = 0usize;
    let mut saved_regs = 0usize;
    let mut trusted_regs = 0usize;
    let mut pattern_regs = 0usize;
    let mut loads = 0usize;
    let mut flushes = 0usize;
    let mut reloads = 0usize;
    let mut calls = 0usize;
    let mut exposed: BTreeMap<String, usize> = BTreeMap::new();
    let mut read_after: BTreeMap<Reg, Vec<(u32, u32)>> = BTreeMap::new();
    let mut taken_refused = 0usize;
    let mut refused_kinds: BTreeMap<String, usize> = BTreeMap::new();
    for fi in 0..n {
        let f = &solver.funcs[fi];
        let pass = solver.pass(&st, fi);
        let kept = st.kept[fi];
        let trusted = st.trusted[fi];
        assert!(kept.minus(pass.need_in) == kept && trusted.and(pass.stored_in).minus(pass.need_in).minus(pass.tail_touches) == trusted);
        assert!(solver.summarise(fi, kept, trusted, &pass).ins.is_subset(&st.summary[fi].ins));
        let sum = st.summary[fi];
        {
            use std::fmt::Write as _;
            let own_pass = solver.pass(&blank, fi);
            // Registers a call may change by the ABI that this function's own code reads after one.
            for (callee, live, at) in &own_pass.after {
                if callee.is_none() && *at != 0 && !live.and(INDIRECT.outs).is_empty() {
                    errors.push(format!(
                        "sub_{:08X} reads {} after the call through a pointer (or to native code) at 0x{at:08X}, which may change it",
                        f.body.begin,
                        regs::describe(live.and(INDIRECT.outs))
                    ));
                }
                for r in live.and(INDIRECT.outs).iter() {
                    let sites = read_after.entry(r).or_default();
                    if sites.last().map(|s| s.0) != Some(f.body.begin) {
                        sites.push((f.body.begin, *at));
                    }
                }
            }
            let own = own_pass.need_in.and(LOCALIZABLE);
            let _ = writeln!(
                details,
                "sub_{:08X}{} takes [{}] own [{}] returns [{}] kept [{}] slot-kept [{}] trusted [{}]",
                f.body.begin,
                if f.address_taken { " &" } else { "" },
                regs::describe(sum.ins),
                regs::describe(own),
                regs::describe(sum.outs),
                regs::describe(kept),
                regs::describe(kept.and(pass.flushed.or(pass.callee_outs)).and(f.locals)),
                regs::describe(trusted),
            );
        }
        let ins = sum.ins.minus(st.indirect.ins).minus(UNCHECKED);
        let outs = sum.outs.minus(st.indirect.outs).minus(UNCHECKED);
        if !ins.is_empty() || !outs.is_empty() {
            let mut why = Vec::new();
            if f.address_taken {
                taken_refused += 1;
                let key = format!("takes {}; returns {}", regs::describe(ins), regs::describe(outs));
                *refused_kinds.entry(key).or_default() += 1;
            }
            if !ins.is_empty() {
                why.push(format!("it takes {} from its caller", regs::describe(ins)));
            }
            if !outs.is_empty() {
                why.push(format!("it returns {} to its caller", regs::describe(outs)));
            }
            not_indirect.insert(f.body.begin, why.join("; "));
        }
        if !sum.ins.is_empty() {
            *exposed.entry(regs::describe(sum.ins)).or_default() += 1;
        }
        pattern_regs += f.pattern.len();
        if f.locals.is_empty() {
            plans.push(None);
            continue;
        }
        with_locals += 1;
        let mut plan = pass.plan;
        // A preserved register whose context slot is written here (stored for
        // a callee, or changed by one) gets the slot's contents put back.
        plan.saved = kept.and(pass.flushed.or(pass.callee_outs)).or(trusted).and(plan.locals);
        plan.checked = trusted.and(plan.locals);
        plan.load = plan.load.or(plan.checked);
        trusted_regs += plan.checked.len();
        plan.skip = f
            .roles
            .iter()
            .map(|roles| roles.iter().map(|role| matches!(role, Role::Save(r) | Role::Restore(r) if kept.contains(*r))).collect())
            .collect();
        kept_regs += kept.and(plan.locals).len();
        saved_regs += plan.saved.len();
        loads += plan.load.len();
        for (bi, block) in f.body.blocks.iter().enumerate() {
            for (ii, item) in block.items.iter().enumerate() {
                if let Item::Call { .. } = item {
                    calls += 1;
                    flushes += plan.sites[bi][ii].flush.len();
                    reloads += plan.sites[bi][ii].reload.len();
                }
            }
        }
        plans.push(Some(plan));
    }

    let mut report = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(report, "register analysis: {n} functions, settled in {rounds} rounds");
    let _ = writeln!(report, "  {with_locals} keep registers in locals; {} keep everything in the context", n - with_locals);
    let _ = writeln!(report, "  {} never return to their caller", solver.returns.iter().filter(|r| !**r).count());
    let _ = writeln!(
        report,
        "  preserved registers: {pattern_regs} save/restore patterns recognised; {kept_regs} need no save at all, \
         of which {saved_regs} keep the context slot's contents across the function; \
         {trusted_regs} more are taken to be restored and checked at each exit"
    );
    let _ = writeln!(report, "  context traffic left: {loads} loads on entry; at {calls} call sites, {flushes} stores before and {reloads} loads after");
    let _ = writeln!(report, "  calls through pointers and to native code: the callee may read {}", regs::describe(st.indirect.ins));
    let _ = writeln!(report, "    and may change {}", regs::describe(st.indirect.outs));
    let _ = writeln!(report, "    read after such a call, somewhere: {}", regs::describe(st.indirect_exit_live));
    let _ = writeln!(
        report,
        "  {} functions don't meet the contract for calls through pointers (fatal table entries); the program takes the address of {taken_refused}:",
        not_indirect.len()
    );
    let mut kinds: Vec<(&String, &usize)> = refused_kinds.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in kinds.iter().take(25) {
        let _ = writeln!(report, "    {count:>6}  {kind}");
    }
    let _ = writeln!(report, "  registers the ABI lets a call change that some function's own code reads after a call (functions, examples):");
    for (r, fns) in &read_after {
        let examples: Vec<String> = fns.iter().take(40).map(|(f, at)| format!("sub_{f:08X}@{at:08X}")).collect();
        let _ = writeln!(report, "    {:<8} {:>6}  {}", regs::name(*r), fns.len(), examples.join(" "));
    }
    let mut by_count: Vec<(&String, &usize)> = exposed.iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(a.1));
    let _ = writeln!(report, "  registers functions take from their callers (functions, registers):");
    for (set, count) in by_count.iter().take(40) {
        let _ = writeln!(report, "    {count:>6}  {set}");
    }
    Analysis { plans, not_indirect, report, details, errors }
}
