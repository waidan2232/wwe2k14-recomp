//! Function discovery: assign every code word to exactly one function or data range.
//!
//! Sources, in priority order:
//! 1. `.pdata` records (authoritative start and length). Records that start at an ABI
//!    save/restore helper are tagged as that helper.
//! 2. ABI helpers without a `.pdata` record.
//! 3. Targets of `bl` instructions found inside already-known functions, repeated
//!    until no new targets appear. `bl` words in data regions are never trusted.
//! 4. A sweep of whatever is still unclaimed.
//!
//! For sources 3 and 4 the extent comes from following control flow:
//! fall-through, branches that stay below the next known start, and the case
//! labels of detected switch tables. Switch tables stored in `.text`, and the
//! exception-handler words in front of SEH functions, are data, not code.
//!
//! A `bl` into the middle of a function becomes a *secondary entry*. MSVC emits
//! these for SEH `__finally` funclets: the unwinder enters at the start, and the
//! parent calls a second entry directly. A secondary entry is an extra callable
//! function running from the target to the end of its container. It overlaps the
//! container on purpose; recompiling the shared tail twice is harmless.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use ppc::Op;

use crate::code::{Code, bo_always, is_call};
use crate::helpers::Helper;
use crate::switches::SwitchTable;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// 16-byte stub for an imported kernel/XAM function. In the file it holds the
    /// import record; the native runtime supplies the implementation.
    ImportThunk,
    Pdata,
    AbiHelper,
    CallTarget,
    Sweep,
    SecondaryEntry,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub begin: u32,
    pub end: u32,
    pub source: Source,
    pub has_exception_handler: bool,
    /// The last instruction can fall through into the next function.
    pub falls_through: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DataKind {
    SwitchTable,
    ExceptionHandlerInfo,
    Padding,
    /// Not a valid instruction.
    Undecodable,
    /// Decodes, but control flow from here runs into invalid words: data that happens to decode.
    InvalidFlow,
}

#[derive(Default)]
pub struct Discovery {
    /// Sorted by start. Secondary entries overlap their containers; nothing else overlaps.
    pub functions: Vec<Function>,
    /// Top-level data between functions: `(begin, end, kind)`, sorted, non-overlapping.
    pub data: Vec<(u32, u32, DataKind)>,
    /// Data embedded inside functions (inline switch tables after their `bctr`).
    /// The recompiler must skip these bytes when translating the function.
    pub embedded: Vec<(u32, u32)>,
    pub problems: Vec<String>,
    /// `(bl address, target)` for calls into the middle of a function.
    pub calls_into_middle: Vec<(u32, u32)>,
}

impl Discovery {
    /// The primary (non-secondary) function containing `addr`.
    pub fn function_at(&self, addr: u32) -> Option<&Function> {
        self.functions
            .iter()
            .filter(|f| f.source != Source::SecondaryEntry)
            .find(|f| (f.begin..f.end).contains(&addr))
    }
}

/// Claimed `[begin, end)` ranges.
#[derive(Default)]
struct Claims(BTreeMap<u32, u32>);

impl Claims {
    fn owner(&self, addr: u32) -> Option<(u32, u32)> {
        self.0.range(..=addr).next_back().filter(|&(_, &end)| addr < end).map(|(&b, &e)| (b, e))
    }

    fn overlaps(&self, begin: u32, end: u32) -> bool {
        self.owner(begin).is_some() || self.0.range(begin..end).next().is_some()
    }

    /// First claimed start strictly after `addr`.
    fn next_start(&self, addr: u32) -> Option<u32> {
        self.0.range(addr + 1..).next().map(|(&b, _)| b)
    }

    fn insert(&mut self, begin: u32, end: u32) {
        self.0.insert(begin, end);
    }
}

/// Follows control flow from `start` without leaving `[start, limit)`.
/// Returns the end of the furthest instruction reached and whether some path
/// runs off the end into `limit`.
fn extent(
    code: &Code,
    start: u32,
    limit: u32,
    switch_at: &HashMap<u32, &SwitchTable>,
    embedded: &Claims,
) -> Result<(u32, bool), String> {
    let mut work = vec![start];
    let mut seen = HashSet::new();
    let mut end = start;
    let mut falls_through = false;
    while let Some(mut pc) = work.pop() {
        loop {
            if pc < start || pc >= limit {
                if pc == limit {
                    falls_through = true;
                }
                break;
            }
            if !seen.insert(pc) {
                break;
            }
            if let Some((_, table_end)) = embedded.owner(pc) {
                // Inline switch table: never executed, but part of the function.
                end = end.max(table_end);
                break;
            }
            let word = code.word(pc).ok_or_else(|| format!("0x{pc:08X} outside image"))?;
            if word == 0 {
                break; // padding after a no-return call
            }
            let Some(i) = code.instr(pc) else {
                return Err(format!("undecodable word {word:08X} at 0x{pc:08X} reached from 0x{start:08X}"));
            };
            end = end.max(pc + 4);
            match i.op {
                Op::bx if !i.lk() => {
                    let t = i.branch_target().unwrap();
                    if (start..limit).contains(&t) {
                        work.push(t);
                    }
                    break;
                }
                Op::bcx if !i.lk() => {
                    let t = i.branch_target().unwrap();
                    if (start..limit).contains(&t) {
                        work.push(t);
                    }
                    if bo_always(&i) {
                        break;
                    }
                }
                Op::bclrx if !i.lk() && bo_always(&i) => break,
                Op::bcctrx if !i.lk() => {
                    if let Some(sw) = switch_at.get(&pc) {
                        work.extend(sw.labels.iter().copied().filter(|l| (start..limit).contains(l)));
                        // Count an inline table as part of the function even if no case follows it.
                        if let Some((_, table_end)) = embedded.owner(pc + 4) {
                            end = end.max(table_end.min(limit));
                        }
                    }
                    if bo_always(&i) {
                        break;
                    }
                }
                _ => {}
            }
            pc += 4;
        }
    }
    Ok((end, falls_through))
}

/// Targets of `bl` instructions inside `[begin, end)`, as `(bl address, target)`.
fn calls_in(code: &Code, begin: u32, end: u32) -> Vec<(u32, u32)> {
    (begin..end)
        .step_by(4)
        .filter_map(|a| code.instr(a))
        .filter(|i| i.op == Op::bx && is_call(i))
        .filter_map(|i| i.branch_target().filter(|&t| code.is_code(t)).map(|t| (i.address, t)))
        .collect()
}

struct State<'c, 'a> {
    code: &'c Code<'a>,
    switch_at: HashMap<u32, &'c SwitchTable>,
    /// Inline switch tables (never function boundaries).
    embedded: Claims,
    /// Functions and top-level data.
    claims: Claims,
    functions: Vec<Function>,
    data: Vec<(u32, u32, DataKind)>,
    problems: Vec<String>,
}

impl State<'_, '_> {
    fn section_end(&self, addr: u32) -> u32 {
        self.code.ranges.iter().find(|&&(b, e)| (b..e).contains(&addr)).map(|&(_, e)| e).unwrap()
    }

    /// Claims a function starting at `start` (already claimed as a 4-byte placeholder
    /// or unclaimed), bounded by the next claimed start.
    fn add_function(&mut self, start: u32, source: Source) -> Result<(), String> {
        let section_end = self.section_end(start);
        let limit = self.claims.next_start(start).unwrap_or(section_end).min(section_end);
        match extent(self.code, start, limit, &self.switch_at, &self.embedded) {
            Ok((end, falls_through)) => {
                let end = end.max(start + 4);
                self.claims.insert(start, end);
                self.functions.push(Function { begin: start, end, source, has_exception_handler: false, falls_through });
                Ok(())
            }
            Err(e) => {
                self.claims.0.remove(&start);
                Err(e)
            }
        }
    }
}

/// Size of an import thunk in the image.
pub const IMPORT_THUNK_SIZE: u32 = 16;

pub fn discover(
    code: &Code,
    pdata: &[xex::RuntimeFunction],
    helpers: &[Helper],
    switches: &[SwitchTable],
    import_thunks: &[u32],
) -> Discovery {
    let mut embedded = Claims::default();
    for sw in switches {
        let (b, e) = sw.data_range();
        if code.is_code(b & !3) {
            embedded.insert(b & !3, (e + 3) & !3);
        }
    }
    let mut st = State {
        code,
        switch_at: switches.iter().map(|s| (s.bctr, s)).collect(),
        embedded,
        claims: Claims::default(),
        functions: Vec::new(),
        data: Vec::new(),
        problems: Vec::new(),
    };
    let helper_at: HashMap<u32, &Helper> = helpers.iter().map(|h| (h.address, h)).collect();
    let in_helper = |t: u32| helpers.iter().any(|h| (h.address..h.end()).contains(&t));

    // 0. Import thunks
    for &t in import_thunks {
        let end = t + IMPORT_THUNK_SIZE;
        if !code.is_code(t) || st.claims.overlaps(t, end) {
            st.problems.push(format!("import thunk 0x{t:08X} is outside code or overlaps another"));
            continue;
        }
        st.claims.insert(t, end);
        st.functions.push(Function {
            begin: t,
            end,
            source: Source::ImportThunk,
            has_exception_handler: false,
            falls_through: false,
        });
    }

    // 1. .pdata (helpers that have records are tagged as helpers)
    for f in pdata {
        let (source, end) = match helper_at.get(&f.begin) {
            Some(h) => (Source::AbiHelper, h.end()),
            None => (Source::Pdata, f.end()),
        };
        if st.claims.overlaps(f.begin, end) {
            st.problems.push(format!(".pdata function 0x{:08X}..0x{end:08X} overlaps another", f.begin));
            continue;
        }
        st.claims.insert(f.begin, end);
        st.functions.push(Function {
            begin: f.begin,
            end,
            source,
            has_exception_handler: f.has_exception_handler && source == Source::Pdata,
            falls_through: false,
        });
        if f.has_exception_handler && source == Source::Pdata {
            // Two words in front of the function: handler address and handler data.
            let (b, e) = (f.begin - 8, f.begin);
            if code.word(b).is_some_and(|w| code.is_code(w)) && !st.claims.overlaps(b, e) {
                st.claims.insert(b, e);
                st.data.push((b, e, DataKind::ExceptionHandlerInfo));
            } else {
                st.problems.push(format!("SEH function 0x{:08X}: no handler address at 0x{b:08X}", f.begin));
            }
        }
    }

    // 2. ABI helpers without .pdata records
    for h in helpers {
        if st.functions.iter().any(|f| f.begin == h.address) {
            continue;
        }
        if st.claims.overlaps(h.address, h.end()) {
            st.problems.push(format!("{} at 0x{:08X} overlaps a .pdata function", h.kind.name, h.address));
            continue;
        }
        st.claims.insert(h.address, h.end());
        st.functions.push(Function {
            begin: h.address,
            end: h.end(),
            source: Source::AbiHelper,
            has_exception_handler: false,
            falls_through: false,
        });
    }

    // 3. Call targets, harvested only from code inside known functions, to a fixed point.
    let mut scanned = 0usize;
    let mut middle: BTreeSet<(u32, u32)> = BTreeSet::new();
    loop {
        let mut new_targets = BTreeSet::new();
        for f in st.functions[scanned..].iter().filter(|f| f.source != Source::ImportThunk) {
            for (from, t) in calls_in(code, f.begin, f.end) {
                match st.claims.owner(t) {
                    Some((b, _)) if b != t => {
                        if !in_helper(t) {
                            middle.insert((from, t));
                        }
                    }
                    Some(_) => {}
                    None => {
                        new_targets.insert(t);
                    }
                }
            }
        }
        scanned = st.functions.len();
        if new_targets.is_empty() {
            break;
        }
        // Placeholders first, so each extent stops at the next known start.
        for &t in &new_targets {
            st.claims.insert(t, t + 4);
        }
        for t in new_targets {
            if let Err(e) = st.add_function(t, Source::CallTarget) {
                st.problems.push(format!("call target 0x{t:08X}: {e}"));
            }
        }
    }

    // 4. Sweep unclaimed words.
    for (sb, se) in code.ranges.clone() {
        let mut addr = sb;
        while addr < se {
            if let Some((_, end)) = st.claims.owner(addr) {
                addr = end;
                continue;
            }
            if let Some((_, end)) = st.embedded.owner(addr) {
                // A switch table not inside any function: top-level data.
                st.data.push((addr, end, DataKind::SwitchTable));
                st.claims.insert(addr, end);
                addr = end;
                continue;
            }
            let word = code.word(addr).unwrap_or(0);
            if word == 0 {
                let run_end = (addr..se)
                    .step_by(4)
                    .find(|&a| code.word(a) != Some(0) || st.claims.owner(a).is_some())
                    .unwrap_or(se);
                st.data.push((addr, run_end, DataKind::Padding));
                st.claims.insert(addr, run_end);
                addr = run_end;
                continue;
            }
            let kind = if code.instr(addr).is_none() {
                DataKind::Undecodable
            } else if st.add_function(addr, Source::Sweep).is_ok() {
                addr = st.claims.owner(addr).map(|(_, e)| e).unwrap_or(addr + 4);
                continue;
            } else {
                DataKind::InvalidFlow
            };
            st.data.push((addr, addr + 4, kind));
            st.claims.insert(addr, addr + 4);
            addr += 4;
        }
    }

    // Calls made by swept functions into the middle of others.
    let swept: Vec<(u32, u32)> =
        st.functions.iter().filter(|f| f.source == Source::Sweep).map(|f| (f.begin, f.end)).collect();
    for (b, e) in swept {
        for (from, t) in calls_in(code, b, e) {
            if let Some((ob, _)) = st.claims.owner(t)
                && ob != t
                && !in_helper(t)
                && st.functions.iter().any(|f| f.begin == ob)
            {
                middle.insert((from, t));
            }
        }
    }

    // 5. Secondary entries for calls into the middle of a function.
    let mut out = Discovery::default();
    let mut secondary: BTreeMap<u32, u32> = BTreeMap::new();
    for &(from, t) in &middle {
        let container = st.functions.iter().find(|f| (f.begin..f.end).contains(&t)).map(|f| f.end);
        match container {
            Some(end) => {
                secondary.insert(t, end);
            }
            None => st.problems.push(format!("bl at 0x{from:08X} targets 0x{t:08X}, which isn't inside a function")),
        }
        out.calls_into_middle.push((from, t));
    }
    for (begin, end) in secondary {
        st.functions.push(Function {
            begin,
            end,
            source: Source::SecondaryEntry,
            has_exception_handler: false,
            falls_through: false,
        });
    }

    st.functions.sort_by_key(|f| (f.begin, f.source));
    st.data.sort();
    let mut merged: Vec<(u32, u32, DataKind)> = Vec::new();
    for d in st.data {
        match merged.last_mut() {
            Some(last) if last.1 == d.0 && last.2 == d.2 => last.1 = d.1,
            _ => merged.push(d),
        }
    }
    out.functions = st.functions;
    out.data = merged;
    out.embedded = st.embedded.0.iter().map(|(&b, &e)| (b, e)).collect();
    out.problems = st.problems;
    out
}
