//! Game functions translated twice, for the differential test (`tests/game.rs`).
//!
//! The register analysis (`recomp::locals`) decides, over the whole program,
//! which registers each function keeps in host locals and where the context
//! must be written or read back. The instruction vectors exercise that on
//! small programs. This puts real game code through it: a few thousand of the
//! game's functions, with everything they call, are translated once with
//! every register in the context and once with locals, and the test runs both
//! on the same inputs and compares every register and the memory they wrote.
//!
//! Only functions that are sure to end are taken: no loops (every branch goes
//! forward), no calls through pointers, no imports, no recursion. Nothing of
//! the game is interpreted: both translations are compiled into the test.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use analyse::{Code, Source};
use recomp::emit::{Body, Exit, FnDef, Item, Program, Target, Term, emit_program, fn_name, scan_function};
use recomp::locals;

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Unknown,
    Visiting,
    Yes,
    No,
}

/// The functions a body calls or tail-calls directly, or `None` if it does
/// anything the test can't run to an end.
fn callees(body: &Body) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    let leave = |e: &Exit, out: &mut Vec<u32>| match e {
        Exit::Return | Exit::FellOff(_) => true,
        Exit::Tail(Target::Direct(t)) => {
            out.push(*t);
            true
        }
        Exit::FallInto(next) => {
            out.push(*next);
            true
        }
        Exit::Tail(_) | Exit::Longjmp => false,
    };
    for block in &body.blocks {
        for item in &block.items {
            match item {
                Item::Code { lines, .. } => {
                    // The time base differs between two runs; an untranslated instruction stops both.
                    if lines.iter().any(|l| l.contains("timebase()") || l.contains("todo_instr")) {
                        return None;
                    }
                }
                Item::Call { target: Target::Direct(t), setjmp: false, .. } => out.push(*t),
                Item::Call { .. } => return None,
                Item::CondExit { exit, .. } => {
                    if !leave(exit, &mut out) {
                        return None;
                    }
                }
            }
        }
        let forward = |t: &u32| *t > block.start;
        match &block.term {
            Term::Jump(t) if forward(t) => {}
            Term::Branch { taken, next, .. } if forward(taken) && forward(next) => {}
            Term::Exit(e) => {
                if !leave(e, &mut out) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Rust source for the test's table of game functions, both translations.
/// With no executable to read, the table is empty.
pub fn generate(xex_path: &Path, wanted: usize) -> String {
    let empty = "pub static GAME: &[(u32, GuestFn, GuestFn)] = &[];\n".to_string();
    let Ok(bytes) = std::fs::read(xex_path) else { return empty };
    let xex = xex::Xex::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", xex_path.display()));
    let a = analyse::analyse(&xex).unwrap_or_else(|e| panic!("{}: {e}", xex_path.display()));
    let code = Code::new(&xex);

    // The same function list the recompiler builds.
    let mut defs: BTreeMap<u32, FnDef> = BTreeMap::new();
    let mut imports: BTreeSet<u32> = BTreeSet::new();
    for f in &a.discovery.functions {
        match f.source {
            Source::ImportThunk => {
                imports.insert(f.begin);
            }
            Source::AbiHelper => {
                let h = a.helpers.iter().find(|h| h.address == f.begin).expect("helper");
                for (_, entry) in h.entries() {
                    defs.insert(entry, FnDef { begin: entry, end: h.end(), skip: Vec::new(), falls_through: false });
                }
            }
            _ => {
                let skip: Vec<(u32, u32)> =
                    a.discovery.embedded.iter().copied().filter(|&(s, e)| s >= f.begin && e <= f.end).collect();
                defs.insert(f.begin, FnDef { begin: f.begin, end: f.end, skip, falls_through: f.falls_through });
            }
        }
    }
    let all: BTreeSet<u32> = defs.keys().copied().chain(imports.iter().copied()).collect();
    let switches: HashMap<u32, (u32, Vec<u32>, Option<u32>)> =
        a.switch_tables().map(|t| (t.bctr, (t.reg, t.labels.clone(), t.default))).collect();
    let inline: BTreeMap<u32, u32> =
        a.helpers.iter().flat_map(|h| h.entries().map(|(_, entry)| (entry, h.end())).collect::<Vec<_>>()).collect();
    let word = |addr: u32| code.word(addr);
    let none = BTreeSet::new();
    let scan = Program { entries: &all, switches: &switches, word: &word, setjmp: &none, longjmp_returns: &none, inline: &inline };

    // Which functions can the test run to an end, with everything they call?
    let order: Vec<u32> = defs.keys().copied().collect();
    let direct: HashMap<u32, Option<Vec<u32>>> =
        order.iter().map(|&f| (f, callees(&scan_function(&scan, &defs[&f])))).collect();
    let mut fit: HashMap<u32, Fit> = order.iter().map(|&f| (f, Fit::Unknown)).collect();
    fn visit(f: u32, direct: &HashMap<u32, Option<Vec<u32>>>, fit: &mut HashMap<u32, Fit>, depth: u32) -> bool {
        match fit.get(&f).copied() {
            Some(Fit::Yes) => return true,
            // An import, a function outside the list, a cycle, or a chain too deep to bother with.
            None | Some(Fit::No) | Some(Fit::Visiting) => return false,
            Some(Fit::Unknown) => {}
        }
        if depth > 24 {
            return false;
        }
        fit.insert(f, Fit::Visiting);
        let ok = match &direct[&f] {
            Some(callees) => callees.iter().all(|&c| visit(c, direct, fit, depth + 1)),
            None => false,
        };
        fit.insert(f, if ok { Fit::Yes } else { Fit::No });
        ok
    }
    for &f in &order {
        visit(f, &direct, &mut fit, 0);
    }

    // Roots, spread evenly over the image; the helpers' own entries are not
    // functions anyone calls with a caller's registers.
    let candidates: Vec<u32> = order
        .iter()
        .copied()
        .filter(|f| fit[f] == Fit::Yes && !inline.contains_key(f) && defs[f].end - defs[f].begin >= 6 * 4)
        .collect();
    let step = (candidates.len() / wanted.max(1)).max(1);
    let roots: BTreeSet<u32> = candidates.iter().copied().step_by(step).take(wanted).collect();
    let mut closure: BTreeSet<u32> = BTreeSet::new();
    let mut work: Vec<u32> = roots.iter().copied().collect();
    while let Some(f) = work.pop() {
        if closure.insert(f) {
            work.extend(direct[&f].iter().flatten().copied());
        }
    }

    // Both translations of the closure. The test reads every register after a
    // root returns, as the harness does for the instruction tests.
    let program =
        Program { entries: &closure, switches: &switches, word: &word, setjmp: &none, longjmp_returns: &none, inline: &inline };
    let list: Vec<&FnDef> = closure.iter().map(|f| &defs[f]).collect();
    let mut src = String::new();
    for (enabled, prefix) in [(false, "c_sub_8"), (true, "l_sub_8")] {
        let options = locals::Options {
            enabled,
            address_taken: &none,
            observed: &roots,
            native: &none,
            untrusted: &none,
            noreturn: &none,
        };
        let unit = emit_program(&program, &list, &options);
        assert!(unit.errors.is_empty(), "register analysis: {:?}", unit.errors);
        for e in &unit.emitted {
            src.push_str(&e.source.replace("sub_8", prefix));
        }
    }
    let _ = writeln!(
        src,
        "/// Game functions for the differential test: address, translation with registers in the\n\
         /// context, translation with registers in locals. {} roots of {} that qualify, {} functions with callees.\n\
         pub static GAME: &[(u32, GuestFn, GuestFn)] = &[",
        roots.len(),
        candidates.len(),
        closure.len()
    );
    for r in &roots {
        let name = fn_name(*r);
        let _ = writeln!(src, "    (0x{r:08X}, c_{name} as GuestFn, l_{name} as GuestFn),");
    }
    src.push_str("];\n");
    src
}
