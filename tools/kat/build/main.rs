//! Turns the test vectors in `data/` into Rust at build time.
//!
//! Each `.s` file is assembled with our assembler (`ppc::asm`) and laid out in
//! one guest image starting at 0x82010000. Every test entry point (a `test_*`
//! label, or a `bl` target) becomes a function, translated by the same emitter
//! that translates the game (`recomp::emit`). The output is those functions
//! plus a table describing each test's inputs and expected results.
//!
//! Nothing is interpreted: the test runner calls the compiled functions.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ppc::asm::{assemble, parse_number};
use recomp::emit::{FnDef, Program, emit_program, fn_name};
use recomp::locals;

mod game;

const BASE: u32 = 0x8201_0000;

/// Files that need more than the instruction-level harness provides.
const SKIP: &[(&str, &str)] = &[];

enum Item {
    Label(String),
    Insn(String),
    Long(String),
    Note(String),
}

#[derive(Default)]
struct Test {
    file: String,
    name: String,
    entry: u32,
    regs_in: Vec<String>,
    regs_out: Vec<String>,
    mem_in: Vec<(u32, Vec<u8>)>,
    mem_out: Vec<(u32, Vec<u8>)>,
    errors: Vec<String>,
}

fn strip_comment(s: &str) -> &str {
    s.split('#').next().unwrap_or("").trim()
}

/// Reads a file into labels, instructions and annotations, expanding `.macro`s.
fn read_items(src: &str) -> Vec<Item> {
    let mut macros: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        i += 1;
        if let Some(note) = t.strip_prefix("#_") {
            items.push(Item::Note(note.trim().to_string()));
            continue;
        }
        let code = strip_comment(t);
        if code.is_empty() {
            continue;
        }
        if let Some(rest) = code.strip_prefix(".macro") {
            let rest = rest.trim();
            let (name, params) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let params: Vec<String> =
                params.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
            let mut body = Vec::new();
            while i < lines.len() && strip_comment(lines[i]) != ".endm" {
                body.push(lines[i].to_string());
                i += 1;
            }
            i += 1;
            macros.insert(name.to_string(), (params, body));
            continue;
        }
        if let Some(label) = code.strip_suffix(':') {
            items.push(Item::Label(label.trim().to_string()));
            continue;
        }
        let (mnem, args) = code.split_once(char::is_whitespace).unwrap_or((code, ""));
        if let Some((order, body)) = macros.get(mnem) {
            let actual: Vec<&str> = args.split(',').map(str::trim).collect();
            // Substitute longer names first so `\d` doesn't eat `\dest`.
            let mut pairs: Vec<(&String, &str)> = order.iter().zip(actual.iter().copied()).collect();
            pairs.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
            for line in body {
                let mut l = line.clone();
                for (p, a) in &pairs {
                    l = l.replace(&format!("\\{p}"), a);
                }
                let l = strip_comment(&l);
                if !l.is_empty() {
                    items.push(Item::Insn(l.to_string()));
                }
            }
            continue;
        }
        if mnem == ".long" {
            items.push(Item::Long(args.trim().to_string()));
            continue;
        }
        items.push(Item::Insn(code.to_string()));
    }
    items
}

fn hex_bytes(tokens: &[&str]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for t in tokens {
        if t.len() % 2 != 0 {
            return Err(format!("odd-length hex group {t:?}"));
        }
        for k in (0..t.len()).step_by(2) {
            out.push(u8::from_str_radix(&t[k..k + 2], 16).map_err(|e| format!("{t:?}: {e}"))?);
        }
    }
    Ok(out)
}

/// `r4 0x10`, `f1 1.5`, `v3 [3f800000, ...]`, `cr 0x20000000` as a `Reg` expression.
fn reg_value(spec: &str) -> Result<String, String> {
    let (reg, value) = spec.split_once(char::is_whitespace).ok_or("missing value")?;
    // Some values carry a C suffix (`0x...ull`).
    let value = value.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    let num = |s: &str| s[1..].parse::<u8>().map_err(|_| format!("bad register {s:?}"));
    match reg.chars().next() {
        _ if reg == "cr" => {
            let v = parse_number(value).ok_or_else(|| format!("bad cr value {value:?}"))?;
            Ok(format!("Reg::Cr(0x{:08X})", v as u32))
        }
        Some('r') => {
            let v = parse_number(value).ok_or_else(|| format!("bad value {value:?}"))?;
            Ok(format!("Reg::R({}, 0x{:016X})", num(reg)?, v as u64))
        }
        Some('f') => {
            let bits = if value.starts_with("0x") || value.starts_with("0X") {
                parse_number(value).ok_or_else(|| format!("bad value {value:?}"))? as u64
            } else {
                value.parse::<f64>().map_err(|_| format!("bad float {value:?}"))?.to_bits()
            };
            Ok(format!("Reg::F({}, 0x{bits:016X})", num(reg)?))
        }
        Some('v') => {
            let inner = value.trim_start_matches('[').trim_end_matches(']');
            let words: Vec<u32> = inner
                .split(',')
                .map(|w| u32::from_str_radix(w.trim().trim_start_matches("0x"), 16))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("bad vector {value:?}: {e}"))?;
            if words.len() != 4 {
                return Err(format!("vector needs 4 words: {value:?}"));
            }
            let v = words.iter().fold(0u128, |acc, &w| (acc << 32) | w as u128);
            Ok(format!("Reg::V({}, 0x{v:032X})", num(reg)?))
        }
        _ => Err(format!("unknown register {reg:?}")),
    }
}

fn main() {
    // Xenia's vectors, then our own.
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("data");
    println!("cargo::rerun-if-changed=build/main.rs");
    let mut files: Vec<PathBuf> = Vec::new();
    for sub in ["xenia", "local"] {
        let dir = root.join(sub);
        println!("cargo::rerun-if-changed={}", dir.display());
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "s"))
            .collect();
        found.sort();
        files.extend(found);
    }

    let mut image: BTreeMap<u32, u32> = BTreeMap::new();
    let mut entries: BTreeSet<u32> = BTreeSet::new();
    let mut functions: Vec<FnDef> = Vec::new();
    let mut tests: Vec<Test> = Vec::new();
    let mut skipped: Vec<(String, &str)> = Vec::new();
    // `#_ SETJMP label` / `#_ LONGJMP_RETURN label`: the same facts
    // config/wwe14.toml gives the recompiler for the game.
    let mut setjmp: BTreeSet<u32> = BTreeSet::new();
    let mut longjmp_returns: BTreeSet<u32> = BTreeSet::new();
    // `#_ INLINE label`: a register save/restore helper (straight code up to
    // its `blr`), translated into its callers as the game's are.
    // `#_ ADDRESS_TAKEN label`: a function called through a pointer.
    // `#_ FUNCTION label`: a function reached only by a branch (a tail call).
    let mut inline: BTreeMap<u32, u32> = BTreeMap::new();
    let mut address_taken: BTreeSet<u32> = BTreeSet::new();
    let mut test_entries: BTreeSet<u32> = BTreeSet::new();
    println!("cargo::rerun-if-env-changed=KAT_LOCALS");
    let locals_on = std::env::var("KAT_LOCALS").map_or(true, |v| v != "off");
    let mut addr = BASE;

    for path in &files {
        let file = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if let Some((_, why)) = SKIP.iter().find(|(f, _)| *f == file) {
            skipped.push((file, why));
            continue;
        }
        let items = read_items(&std::fs::read_to_string(path).unwrap());

        // Pass 1: addresses.
        let start = addr;
        let mut labels: HashMap<String, u32> = HashMap::new();
        let mut a = start;
        for it in &items {
            match it {
                Item::Label(l) => {
                    labels.insert(l.clone(), a);
                }
                Item::Insn(_) | Item::Long(_) => a += 4,
                Item::Note(_) => {}
            }
        }
        let end = a;
        addr = (end + 0x100) & !0xFF;

        // Entry points: tests, call targets and annotated labels.
        let mut file_entries: BTreeSet<u32> = BTreeSet::new();
        for it in &items {
            match it {
                Item::Label(l) if l.starts_with("test_") => {
                    file_entries.insert(labels[l]);
                    test_entries.insert(labels[l]);
                }
                Item::Note(n) if n.starts_with("INLINE ") || n.starts_with("ADDRESS_TAKEN ") || n.starts_with("FUNCTION ") => {
                    let (kind, label) = n.split_once(' ').unwrap();
                    let at = *labels.get(label.trim()).unwrap_or_else(|| panic!("{file}: no label {label:?} for {kind}"));
                    file_entries.insert(at);
                    if kind == "ADDRESS_TAKEN" {
                        address_taken.insert(at);
                    } else if kind == "INLINE" {
                        // The helper runs to its first `blr`.
                        let mut a = start;
                        let mut end = None;
                        for other in &items {
                            if let Item::Insn(text) = other {
                                if a >= at && text.trim() == "blr" && end.is_none() {
                                    end = Some(a + 4);
                                }
                                a += 4;
                            } else if let Item::Long(_) = other {
                                a += 4;
                            }
                        }
                        inline.insert(at, end.unwrap_or_else(|| panic!("{file}: no blr after {label}")));
                    }
                }
                Item::Insn(t) => {
                    let (m, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
                    if m == "bl" {
                        if let Some(&target) = labels.get(rest.trim()) {
                            file_entries.insert(target);
                        }
                    }
                }
                _ => {}
            }
        }

        // Pass 2: assemble and collect annotations.
        let resolve = |l: &str| labels.get(l).copied();
        let mut a = start;
        let mut current: Option<usize> = None;
        for it in &items {
            match it {
                Item::Label(l) if l.starts_with("test_") => {
                    tests.push(Test { file: file.clone(), name: l.clone(), entry: labels[l], ..Default::default() });
                    current = Some(tests.len() - 1);
                }
                Item::Label(_) => {}
                Item::Insn(text) => {
                    // `label@h` and `label@l`: the halves of a label's address.
                    let mut text = text.clone();
                    while let Some(at) = text.find('@') {
                        let from = text[..at].rfind(|ch: char| !(ch.is_alphanumeric() || ch == '_')).map_or(0, |i| i + 1);
                        let half = text[at + 1..].chars().next();
                        let value = labels.get(&text[from..at]).copied().zip(half).and_then(|(v, h)| match h {
                            'h' => Some(v >> 16),
                            'l' => Some(v & 0xFFFF),
                            _ => None,
                        });
                        let Some(value) = value else { break };
                        text.replace_range(from..at + 2, &format!("0x{value:X}"));
                    }
                    let text = &text;
                    match assemble(text, a, &resolve) {
                        Ok(w) => {
                            image.insert(a, w);
                        }
                        Err(e) => {
                            if let Some(t) = current {
                                tests[t].errors.push(format!("assembler: {e}"));
                            }
                        }
                    }
                    a += 4;
                }
                Item::Long(text) => {
                    match parse_number(text).map(|v| v as u32).or_else(|| resolve(text)) {
                        Some(w) => {
                            image.insert(a, w);
                        }
                        None => {
                            if let Some(t) = current {
                                tests[t].errors.push(format!(".long {text:?}"));
                            }
                        }
                    }
                    a += 4;
                }
                Item::Note(n) => {
                    let (kind, rest) = n.split_once(char::is_whitespace).unwrap_or((n, ""));
                    let rest = rest.trim();
                    if matches!(kind, "INLINE" | "ADDRESS_TAKEN" | "FUNCTION") {
                        continue;
                    }
                    if matches!(kind, "SETJMP" | "LONGJMP_RETURN") {
                        let at = *labels.get(rest).unwrap_or_else(|| panic!("{file}: no label {rest:?} for {kind}"));
                        if kind == "SETJMP" { setjmp.insert(at) } else { longjmp_returns.insert(at) };
                        continue;
                    }
                    let Some(t) = current else { continue };
                    let t = &mut tests[t];
                    let result = match kind {
                        "REGISTER_IN" => reg_value(rest).map(|r| t.regs_in.push(r)),
                        "REGISTER_OUT" => reg_value(rest).map(|r| t.regs_out.push(r)),
                        "MEMORY_IN" | "MEMORY_OUT" => {
                            let tokens: Vec<&str> = rest.split_whitespace().collect();
                            let at = tokens.first().and_then(|s| u32::from_str_radix(s, 16).ok());
                            match (at, hex_bytes(tokens.get(1..).unwrap_or(&[]))) {
                                (Some(at), Ok(bytes)) => {
                                    if kind == "MEMORY_IN" { t.mem_in.push((at, bytes)) } else { t.mem_out.push((at, bytes)) }
                                    Ok(())
                                }
                                (_, Err(e)) => Err(e),
                                (None, _) => Err(format!("bad address in {rest:?}")),
                            }
                        }
                        other => Err(format!("unknown annotation {other}")),
                    };
                    if let Err(e) = result {
                        t.errors.push(format!("annotation {n:?}: {e}"));
                    }
                }
            }
        }

        let bounds: Vec<u32> = file_entries.iter().copied().chain([end]).collect();
        for w in bounds.windows(2) {
            functions.push(FnDef { begin: w[0], end: w[1], skip: Vec::new(), falls_through: false });
        }
        entries.extend(file_entries);
    }

    // Translate every function with the game's emitter.
    let switches = HashMap::new();
    let word = |a: u32| image.get(&a).copied();
    let program = Program {
        entries: &entries,
        switches: &switches,
        word: &word,
        setjmp: &setjmp,
        longjmp_returns: &longjmp_returns,
        inline: &inline,
    };
    // The harness reads every register after a test returns; it calls the
    // tests directly, with every register it sets already in the context.
    let none = BTreeSet::new();
    let options = locals::Options {
        enabled: locals_on,
        address_taken: &address_taken,
        observed: &test_entries,
        native: &none,
        untrusted: &none,
        noreturn: &none,
    };
    let unit = emit_program(&program, &functions.iter().collect::<Vec<_>>(), &options);
    assert!(unit.errors.is_empty(), "register analysis: {:?}", unit.errors);
    let mut src = String::new();
    let mut todo: HashMap<u32, Vec<&'static str>> = HashMap::new();
    let mut callees: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    for (f, e) in functions.iter().zip(&unit.emitted) {
        src.push_str(&e.source);
        todo.insert(f.begin, e.todo_ops.clone());
        callees.insert(f.begin, e.callees.clone());
    }
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("locals_report.txt"), format!("{}\n{}", unit.report, unit.details)).unwrap();

    for (t, why) in &unit.not_indirect {
        let _ = writeln!(src, "unsafe extern \"C-unwind\" fn not_indirect_{t:08X}(_c: &mut Ctx, _b: *mut u8) {{ not_indirect(0x{t:08X}, {why:?}) }}");
    }
    let _ = writeln!(src, "pub static FUNCTIONS: [(u32, GuestFn); {}] = [", entries.len());
    for e in &entries {
        if unit.not_indirect.contains_key(e) {
            let _ = writeln!(src, "    (0x{e:08X}, not_indirect_{e:08X} as GuestFn),");
        } else {
            let _ = writeln!(src, "    (0x{e:08X}, {} as GuestFn),", fn_name(*e));
        }
    }
    src.push_str("];\n\npub static TESTS: &[Test] = &[\n");
    for t in &tests {
        // Untranslated instructions in the test or anything it calls.
        let mut ops: BTreeSet<String> = BTreeSet::new();
        let mut stack = vec![t.entry];
        let mut seen = BTreeSet::new();
        while let Some(f) = stack.pop() {
            if !seen.insert(f) {
                continue;
            }
            ops.extend(todo.get(&f).into_iter().flatten().map(|s| s.to_string()));
            stack.extend(callees.get(&f).into_iter().flatten().copied());
        }
        let list = |v: &[String]| v.join(", ");
        let mem = |v: &[(u32, Vec<u8>)]| {
            v.iter().map(|(a, b)| format!("(0x{a:08X}, &{b:?})")).collect::<Vec<_>>().join(", ")
        };
        let _ = writeln!(
            src,
            "    Test {{ file: {:?}, name: {:?}, entry: {}, todo: &{:?}, errors: &{:?}, regs_in: &[{}], regs_out: &[{}], mem_in: &[{}], mem_out: &[{}] }},",
            t.file,
            t.name,
            fn_name(t.entry),
            ops.iter().collect::<Vec<_>>(),
            t.errors,
            list(&t.regs_in),
            list(&t.regs_out),
            mem(&t.mem_in),
            mem(&t.mem_out),
        );
    }
    src.push_str("];\n\npub static SKIPPED: &[(&str, &str)] = &[\n");
    for (f, why) in &skipped {
        let _ = writeln!(src, "    ({f:?}, {why:?}),");
    }
    src.push_str("];\n");

    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("kat.rs");
    std::fs::write(out, src).unwrap();

    // Game functions for the differential test, when the executable is there
    // (it is the user's own copy; without it the test has nothing to run).
    let xex = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../private/game/default.xex");
    println!("cargo::rerun-if-changed={}", xex.display());
    println!("cargo::rerun-if-env-changed=KAT_GAME_FUNCTIONS");
    let wanted = std::env::var("KAT_GAME_FUNCTIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("game.rs");
    std::fs::write(out, game::generate(&xex, wanted)).unwrap();
}
