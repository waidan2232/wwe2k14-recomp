//! Guest registers as the recompiler tracks them, and what a piece of emitted
//! Rust does to them.
//!
//! The emitter writes every register access as text over the context
//! (`c.r[31]`, `c.cr[6].eq`, `c.ctr`). The register analysis takes each
//! instruction's reads and writes from that same text, so it cannot disagree
//! with what the code does, and a register kept in a host local is the same
//! text with the access renamed (`r_31`).

/// A set of tracked registers.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct RegSet(pub [u64; 4]);

pub type Reg = usize;

pub const GPR: Reg = 0;
pub const FPR: Reg = 32;
pub const CR: Reg = 64;
pub const CTR: Reg = 72;
pub const XER_CA: Reg = 73;
pub const XER_SO: Reg = 74;
pub const XER_OV: Reg = 75;
pub const RESERVED: Reg = 76;
pub const VR: Reg = 80;
pub const COUNT: Reg = VR + 128;

impl RegSet {
    pub const EMPTY: RegSet = RegSet([0; 4]);

    pub const fn with(mut self, r: Reg) -> RegSet {
        self.0[r / 64] |= 1 << (r % 64);
        self
    }

    pub const fn with_range(mut self, first: Reg, last: Reg) -> RegSet {
        let mut r = first;
        while r <= last {
            self = self.with(r);
            r += 1;
        }
        self
    }

    pub fn insert(&mut self, r: Reg) {
        self.0[r / 64] |= 1 << (r % 64);
    }

    pub fn remove(&mut self, r: Reg) {
        self.0[r / 64] &= !(1 << (r % 64));
    }

    pub const fn contains(&self, r: Reg) -> bool {
        self.0[r / 64] & (1 << (r % 64)) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }

    pub fn len(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub const fn or(self, o: RegSet) -> RegSet {
        RegSet([self.0[0] | o.0[0], self.0[1] | o.0[1], self.0[2] | o.0[2], self.0[3] | o.0[3]])
    }

    pub const fn and(self, o: RegSet) -> RegSet {
        RegSet([self.0[0] & o.0[0], self.0[1] & o.0[1], self.0[2] & o.0[2], self.0[3] & o.0[3]])
    }

    /// The registers of `self` that are not in `o`.
    pub const fn minus(self, o: RegSet) -> RegSet {
        RegSet([self.0[0] & !o.0[0], self.0[1] & !o.0[1], self.0[2] & !o.0[2], self.0[3] & !o.0[3]])
    }

    pub fn is_subset(&self, o: &RegSet) -> bool {
        self.minus(*o).is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Reg> + '_ {
        (0..COUNT).filter(move |&r| self.contains(r))
    }
}

impl std::fmt::Debug for RegSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&describe(*self))
    }
}

/// Registers a function may keep in host locals: everything that is not part
/// of the interface every function and every native shares through the context.
/// Those stay in the context always: the stack pointer `r1`, `r2`, the
/// arguments and results `r3`-`r10`, `f1`-`f13` and `v1`-`v13`, the thread
/// pointer `r13`, and LR, MSR and FPSCR.
pub const LOCALIZABLE: RegSet = RegSet::EMPTY
    .with(GPR)
    .with_range(GPR + 11, GPR + 12)
    .with_range(GPR + 14, GPR + 31)
    .with(FPR)
    .with_range(FPR + 14, FPR + 31)
    .with_range(CR, CR + 7)
    .with(CTR)
    .with(XER_CA)
    .with(XER_SO)
    .with(XER_OV)
    .with(RESERVED)
    .with(VR)
    .with_range(VR + 14, VR + 127);

/// The registers the ABI has a callee preserve: `r14`-`r31`, `f14`-`f31`,
/// `v14`-`v31` and `v64`-`v127`.
pub const SAVED_BY_CALLEE: RegSet = RegSet::EMPTY
    .with_range(GPR + 14, GPR + 31)
    .with_range(FPR + 14, FPR + 31)
    .with_range(VR + 14, VR + 31)
    .with_range(VR + 64, VR + 127);

/// The register as the emitter writes it over the context.
pub fn ctx_name(r: Reg) -> String {
    match r {
        _ if r < FPR => format!("c.r[{}]", r - GPR),
        _ if r < CR => format!("c.f[{}]", r - FPR),
        _ if r < CTR => format!("c.cr[{}]", r - CR),
        CTR => "c.ctr".into(),
        XER_CA => "c.xer_ca".into(),
        XER_SO => "c.xer_so".into(),
        XER_OV => "c.xer_ov".into(),
        RESERVED => "c.reserved".into(),
        _ => format!("c.v[{}]", r - VR),
    }
}

/// The host local that holds the register.
pub fn local_name(r: Reg) -> String {
    match r {
        _ if r < FPR => format!("r_{}", r - GPR),
        _ if r < CR => format!("f_{}", r - FPR),
        _ if r < CTR => format!("cr_{}", r - CR),
        CTR => "l_ctr".into(),
        XER_CA => "l_ca".into(),
        XER_SO => "l_so".into(),
        XER_OV => "l_ov".into(),
        RESERVED => "l_res".into(),
        _ => format!("v_{}", r - VR),
    }
}

/// The local's Rust type and its zero value.
pub fn local_type(r: Reg) -> (&'static str, &'static str) {
    match r {
        _ if r < FPR => ("u64", "0"),
        _ if r < CR => ("f64", "0.0"),
        _ if r < CTR => ("Cr", "Cr { lt: false, gt: false, eq: false, so: false }"),
        CTR | RESERVED => ("u64", "0"),
        XER_CA | XER_SO | XER_OV => ("bool", "false"),
        _ => ("u128", "0"),
    }
}

/// The register's name in reports.
pub fn name(r: Reg) -> String {
    match r {
        _ if r < FPR => format!("r{}", r - GPR),
        _ if r < CR => format!("f{}", r - FPR),
        _ if r < CTR => format!("cr{}", r - CR),
        CTR => "ctr".into(),
        XER_CA => "ca".into(),
        XER_SO => "so".into(),
        XER_OV => "ov".into(),
        RESERVED => "reserved".into(),
        _ => format!("v{}", r - VR),
    }
}

/// A set as text, with runs of neighbouring registers folded (`r14-r31`).
pub fn describe(s: RegSet) -> String {
    let regs: Vec<Reg> = s.iter().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < regs.len() {
        let mut j = i;
        let class = |r: Reg| [FPR, CR, CTR, VR].iter().filter(|&&b| r >= b).count();
        while j + 1 < regs.len() && regs[j + 1] == regs[j] + 1 && class(regs[j + 1]) == class(regs[i]) && class(regs[i]) != 3 {
            j += 1;
        }
        out.push(if j > i + 1 { format!("{}-{}", name(regs[i]), name(regs[j])) } else { name(regs[i]) });
        i = if j > i + 1 { j + 1 } else { i + 1 };
    }
    if out.is_empty() { "-".into() } else { out.join(" ") }
}

/// One register access in emitted text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mention {
    pub reg: Reg,
    /// Byte range of the access (`c.r[31]`, without a field such as `.eq`).
    pub start: usize,
    pub end: usize,
    pub access: Access,
    /// Which statement of the text it is in (statements end at ;).
    pub statement: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    /// The whole register is assigned, on every path through the text.
    Define,
    /// Part of the register is assigned (one CR bit), or the assignment sits
    /// inside an `if`, `else` or `match`: the old value can survive.
    Modify,
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every tracked-register access in `text`. Panics on a context access it
/// doesn't know (a method call such as `c.cr_word()` hides which registers it
/// touches, so the emitter must not write one).
pub fn mentions(text: &str) -> Vec<Mention> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    // For each open brace: is it the body of an `if`, `else`, `match` or loop?
    let mut braces: Vec<bool> = Vec::new();
    let mut pending = false;
    let mut statement = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b';' => {
                statement += 1;
                i += 1;
            }
            b'{' => {
                braces.push(pending);
                pending = false;
                i += 1;
            }
            b'}' => {
                braces.pop();
                i += 1;
            }
            b'"' => {
                // String literals (diagnostics) are not code.
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            x if ident_byte(x) => {
                let start = i;
                while i < b.len() && ident_byte(b[i]) {
                    i += 1;
                }
                let word = &text[start..i];
                if matches!(word, "if" | "else" | "match" | "while" | "for" | "loop") {
                    pending = true;
                    continue;
                }
                if word != "c" || b.get(i) != Some(&b'.') || (start > 0 && b[start - 1] == b'.') {
                    continue;
                }
                // `c.<field>`
                let fstart = i + 1;
                let mut j = fstart;
                while j < b.len() && ident_byte(b[j]) {
                    j += 1;
                }
                let field = &text[fstart..j];
                let indexed = |base: Reg, limit: usize, j: &mut usize| -> Reg {
                    let close = text[*j..].find(']').unwrap_or_else(|| panic!("unterminated index in {text:?}"));
                    let n: usize = text[*j + 1..*j + close]
                        .parse()
                        .unwrap_or_else(|_| panic!("register index is not a literal in {text:?}"));
                    assert!(b[*j] == b'[' && n < limit, "bad register access in {text:?}");
                    *j += close + 1;
                    base + n
                };
                let reg = match field {
                    "r" => indexed(GPR, 32, &mut j),
                    "f" => indexed(FPR, 32, &mut j),
                    "v" => indexed(VR, 128, &mut j),
                    "cr" => indexed(CR, 8, &mut j),
                    "ctr" => CTR,
                    "xer_ca" => XER_CA,
                    "xer_so" => XER_SO,
                    "xer_ov" => XER_OV,
                    "reserved" => RESERVED,
                    // Never held in locals.
                    "lr" | "msr" | "fpscr" => {
                        i = j;
                        continue;
                    }
                    _ => panic!("the emitter wrote a context access the register analysis doesn't know: `c.{field}` in {text:?}"),
                };
                let end = j;
                // A field of the register (`.eq`)?
                let mut k = end;
                let mut part = false;
                if b.get(k) == Some(&b'.') && b.get(k + 1).is_some_and(|x| x.is_ascii_lowercase()) {
                    let mut m = k + 1;
                    while m < b.len() && ident_byte(b[m]) {
                        m += 1;
                    }
                    // A method call reads the register; a field narrows the access.
                    if b.get(m) != Some(&b'(') {
                        part = true;
                        k = m;
                    }
                }
                while b.get(k) == Some(&b' ') {
                    k += 1;
                }
                let assigned = b.get(k) == Some(&b'=') && b.get(k + 1) != Some(&b'=');
                let conditional = pending || braces.iter().any(|&c| c);
                let access = match (assigned, part || conditional) {
                    (false, _) => Access::Read,
                    (true, false) => Access::Define,
                    (true, true) => Access::Modify,
                };
                out.push(Mention { reg, start, end, access, statement });
                i = end;
            }
            _ => i += 1,
        }
    }
    out
}

/// What a piece of code does to the tracked registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effect {
    /// Read (before any write of its own).
    pub uses: RegSet,
    /// Assigned whole on every path, and not read: the old value is dead.
    pub defs: RegSet,
    /// Assigned at all.
    pub writes: RegSet,
}

impl Effect {
    pub fn of(text: &str) -> Effect {
        let mut e = Effect::default();
        // Registers assigned whole by an earlier statement: a later read sees
        // that value, not the one from before the text. Within one statement
        // the right-hand side is read before the assignment happens
        // (c.r[3] = c.r[3] + 1), so its own assignments count only once it ends.
        let mut defined = RegSet::EMPTY;
        let mut in_statement = RegSet::EMPTY;
        let mut statement = 0;
        for m in mentions(text) {
            if m.statement != statement {
                statement = m.statement;
                defined = defined.or(in_statement);
                in_statement = RegSet::EMPTY;
            }
            match m.access {
                Access::Read => {
                    if !defined.contains(m.reg) {
                        e.uses.insert(m.reg);
                    }
                }
                Access::Define => {
                    in_statement.insert(m.reg);
                    e.writes.insert(m.reg);
                }
                Access::Modify => e.writes.insert(m.reg),
            }
        }
        e.defs = defined.or(in_statement).minus(e.uses);
        e
    }

    pub fn then(self, later: Effect) -> Effect {
        Effect {
            uses: self.uses.or(later.uses.minus(self.defs)),
            defs: self.defs.or(later.defs.minus(self.uses)),
            writes: self.writes.or(later.writes),
        }
    }
}

/// `text` with every access to a register in `locals` renamed to its host local.
pub fn localize(text: &str, locals: &RegSet) -> String {
    if locals.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for m in mentions(text) {
        if locals.contains(m.reg) {
            out.push_str(&text[at..m.start]);
            out.push_str(&local_name(m.reg));
            at = m.end;
        }
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(regs: &[Reg]) -> RegSet {
        regs.iter().fold(RegSet::EMPTY, |s, &r| s.with(r))
    }

    #[test]
    fn reads_and_whole_writes() {
        let e = Effect::of("{ let e = ea(c.r[4], 0x00000008); c.r[31] = ld32(b, e) as u64; }");
        assert_eq!((e.uses, e.defs, e.writes), (set(&[4]), set(&[31]), set(&[31])));
        // A register read and written by the same instruction is not dead before it.
        let e = Effect::of("c.r[11] = c.r[11].wrapping_add(0x1u64);");
        assert_eq!((e.uses, e.defs, e.writes), (set(&[11]), set(&[]), set(&[11])));
        // A record form reads back the register it has just written: the old value is still dead.
        let e = Effect::of("c.r[11] = rotl32(c.r[4], 0) & 0x80000000u64; c.cr[0] = cmp_s(c.r[11] as i32 as i64, 0, c.xer_so);");
        assert_eq!((e.uses, e.defs), (set(&[4, XER_SO]), set(&[11, CR])));
        // But a write inside a condition doesn't cover a later read.
        let e = Effect::of("if c.cr[6].eq { c.r[11] = 0; } c.r[3] = c.r[11];");
        assert_eq!((e.uses, e.defs), (set(&[CR + 6, 11]), set(&[3])));
        let e = Effect::of("c.cr[6] = cmp_s(c.r[11] as i32 as i64, 0, c.xer_so);");
        assert_eq!((e.uses, e.defs), (set(&[11, XER_SO]), set(&[CR + 6])));
        // `==` is a comparison, not an assignment.
        let e = Effect::of("let v = c.cr[0].eq == c.cr[1].lt;");
        assert_eq!((e.uses, e.writes), (set(&[CR, CR + 1]), set(&[])));
    }

    #[test]
    fn partial_and_conditional_writes_do_not_kill() {
        let e = Effect::of("{ let v = c.cr[0].lt | c.cr[1].gt; c.cr[7].eq = v; }");
        assert_eq!((e.uses, e.defs, e.writes), (set(&[CR, CR + 1]), set(&[]), set(&[CR + 7])));
        let e = Effect::of("if c.cr[6].eq { c.r[12] = 5; }");
        assert_eq!((e.uses, e.defs, e.writes), (set(&[CR + 6]), set(&[]), set(&[12])));
        // An `if` expression on the right-hand side doesn't make the assignment conditional,
        // and neither does one in an earlier statement.
        let e = Effect::of("{ let n = c.r[5] & 63; c.r[3] = if n >= 32 { 0 } else { 1 }; c.xer_ca = false; }");
        assert_eq!((e.uses, e.defs), (set(&[5]), set(&[3, XER_CA])));
        // The decrement in a branch condition is unconditional, but CTR is read too.
        let e = Effect::of("{ c.ctr = c.ctr.wrapping_sub(1); c.ctr as u32 != 0 } && !c.cr[0].eq");
        assert_eq!((e.uses, e.defs, e.writes), (set(&[CTR, CR]), set(&[]), set(&[CTR])));
    }

    #[test]
    fn untracked_state_and_strings_are_skipped() {
        let e = Effect::of("c.lr = c.r[12]; c.msr = 0; set_fpscr(c, c.fpscr); trap(\"c.r[31] = 1\");");
        assert_eq!((e.uses, e.writes), (set(&[12]), set(&[])));
    }

    #[test]
    #[should_panic(expected = "doesn't know")]
    fn unknown_context_access_is_fatal() {
        mentions("c.r[3] = c.cr_word() as u64;");
    }

    #[test]
    fn localize_renames_only_the_chosen_registers() {
        let locals = set(&[31, CR + 6, CTR, VR + 64]);
        assert_eq!(
            localize("c.cr[6] = cmp_u(c.r[31] as u32 as u64, c.r[3] as u32 as u64, c.xer_so);", &locals),
            "cr_6 = cmp_u(r_31 as u32 as u64, c.r[3] as u32 as u64, c.xer_so);"
        );
        assert_eq!(localize("if !c.cr[6].eq { c.v[64] = vor(c.v[1], c.v[64]); }", &locals), "if !cr_6.eq { v_64 = vor(c.v[1], v_64); }");
        assert_eq!(localize("call_indirect(c, b, c.ctr as u32)", &locals), "call_indirect(c, b, l_ctr as u32)");
    }

    #[test]
    fn sets_describe_runs() {
        let s = RegSet::EMPTY.with_range(14, 31).with(11).with(CTR).with(CR + 6).with(VR + 64).with(VR + 65);
        assert_eq!(describe(s), "r11 r14-r31 cr6 ctr v64 v65");
        assert!(LOCALIZABLE.contains(GPR + 12) && !LOCALIZABLE.contains(GPR + 3) && !LOCALIZABLE.contains(VR + 1));
    }
}
