//! Static analysis of the game executable for the recompiler.
//!
//! Reads the XEX purely as data. Nothing here executes guest code.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use analyse::{BctrKind, Code, DataKind, Source, coverage, functions, helpers, remap, switches};
use xex::Xex;

const USAGE: &str = "\
usage:
  analyse coverage  <default.xex> [--examples N]
  analyse helpers   <default.xex>
  analyse switches  <default.xex> [--examples N]
  analyse functions <default.xex> [--examples N]
  analyse report    <default.xex> <out_dir>      (writes review files; fails if the Phase 1 gate fails)
  analyse dis       <default.xex> <hex address> <count>
  analyse callers   <default.xex> <import name | hex address> [--examples N]   (call sites with N instructions of context)
  analyse r13       <default.xex>                (loads/stores through r13, the per-thread PCR pointer)
  analyse refs      <default.xex> <hex address> [--examples N]   (lis+addi/load/store pairs that form the address)
  analyse gpu       <default.xex> [lo hi]      (functions that build GPU packets or call Vd*; with a range: its API surface)
  analyse fns       <default.xex> <lo> <hi>    (functions in a range: size, callers in/out of the range, outside callees)
  analyse effects   <default.xex> <function>  (stores through its first argument (r3), loads of +0x30 cursors, calls)
  analyse reach     <default.xex> <function> [cut ...]   (config cut points and hardware-facing functions its direct calls reach)
  analyse match     <base.xex> <tu.xex> <out.tsv>    (pair every base function with its title-update counterpart)
  analyse map-data  <base.xex> <tu.xex> <addr ...>   (title-update address of base data addresses, via code references)
  analyse remap     <base.xex> <tu.xex> <repo> <out.tsv>   (map every guest address literal in config/, runtime/, tools/recomp/)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let examples_at = args.iter().position(|a| a == "--examples");
    let examples = examples_at.and_then(|i| args.get(i + 1)).map(|n| n.parse::<usize>()).transpose()?.unwrap_or(16);
    let positional: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|&(i, a)| !a.starts_with("--") && Some(i) != examples_at.map(|p| p + 1))
        .map(|(_, a)| a.as_str())
        .collect();
    if let ["dis", path, addr, count] = positional.as_slice() {
        let xex = load(path)?;
        let code = Code::new(&xex);
        let start = u32::from_str_radix(addr.trim_start_matches("0x"), 16)?;
        for k in 0..count.parse::<u32>()? {
            let a = start + k * 4;
            let w = code.word(a).ok_or("address outside image")?;
            match ppc::Instr::decode(a, w) {
                Some(i) => println!("{a:08X}  {w:08X}  {i}"),
                None => println!("{a:08X}  {w:08X}  <undecodable>"),
            }
        }
        return Ok(());
    }
    if let ["report", path, out] = positional.as_slice() {
        return report(&load(path)?, Path::new(out));
    }
    if let ["callers", path, target] = positional.as_slice() {
        return callers_cmd(&load(path)?, target, examples);
    }
    if let ["refs", path, target] = positional.as_slice() {
        return refs_cmd(&load(path)?, u32::from_str_radix(target.trim_start_matches("0x"), 16)?, examples);
    }
    if let ["r13", path] = positional.as_slice() {
        return r13_cmd(&load(path)?);
    }
    if let ["effects", path, f] = positional.as_slice() {
        return effects_cmd(&load(path)?, u32::from_str_radix(f.trim_start_matches("0x"), 16)?);
    }
    if let ["fns", path, lo, hi] = positional.as_slice() {
        let p = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16);
        return fns_cmd(&load(path)?, p(lo)?, p(hi)?);
    }
    if let ["gpu", path, rest @ ..] = positional.as_slice() {
        let span = match rest {
            [] => None,
            [lo, hi] => Some((u32::from_str_radix(lo.trim_start_matches("0x"), 16)?, u32::from_str_radix(hi.trim_start_matches("0x"), 16)?)),
            _ => return Err(USAGE.into()),
        };
        return gpu_cmd(&load(path)?, span, examples);
    }
    if let ["reach", path, from, cuts @ ..] = positional.as_slice() {
        let p = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16);
        let cuts = cuts.iter().map(|s| p(s)).collect::<Result<BTreeSet<u32>, _>>()?;
        return reach_cmd(&load(path)?, p(from)?, &cuts);
    }
    if let ["match", b, t, out] = positional.as_slice() {
        return match_cmd(&load(b)?, &load(t)?, Path::new(out));
    }
    if let ["map-data", b, t, addrs @ ..] = positional.as_slice() {
        let addrs = addrs.iter().map(|s| parse_hex(s)).collect::<Result<Vec<u32>, _>>()?;
        return map_data_cmd(&load(b)?, &load(t)?, &addrs);
    }
    if let ["remap", b, t, root, out] = positional.as_slice() {
        return remap_cmd(&load(b)?, &load(t)?, Path::new(root), Path::new(out));
    }
    let [cmd, path] = positional.as_slice() else { return Err(USAGE.into()) };
    let xex = load(path)?;
    match *cmd {
        "coverage" => coverage::run(&xex, examples),
        "helpers" => helpers_cmd(&Code::new(&xex)),
        "switches" => switches_cmd(&Code::new(&xex), examples),
        "functions" => functions_cmd(&Code::new(&xex), examples),
        _ => Err(USAGE.into()),
    }
}

/// What a function's direct calls and tail calls can reach: the cut functions
/// (replaced or fenced off in the config; the search stops at each) and the
/// hardware-facing functions past none of them, each with one path. Indirect
/// calls are not followed.
fn reach_cmd(xex: &Xex, from: u32, cuts: &BTreeSet<u32>) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let census = analyse::gpu::census(xex, &a);
    let mut graph: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (f, t) in analyse::gpu::call_and_tail_edges(xex, &a) {
        graph.entry(f).or_default().push(t);
    }
    let mut parent: BTreeMap<u32, u32> = BTreeMap::from([(from, from)]);
    let mut queue = std::collections::VecDeque::from([from]);
    while let Some(f) = queue.pop_front() {
        if cuts.contains(&f) && f != from {
            continue;
        }
        for &t in graph.get(&f).map(Vec::as_slice).unwrap_or(&[]) {
            if !parent.contains_key(&t) {
                parent.insert(t, f);
                queue.push_back(t);
            }
        }
    }
    let path = |f: u32| {
        let mut p = vec![f];
        while let Some(&up) = parent.get(p.last().unwrap()).filter(|&&up| up != *p.last().unwrap()) {
            p.push(up);
        }
        p.iter().rev().map(|x| format!("{x:08X}")).collect::<Vec<_>>().join(" -> ")
    };
    println!("{} functions reachable from sub_{from:08X} by direct calls", parent.len());
    for &f in parent.keys().filter(|f| cuts.contains(f) && **f != from) {
        println!("cut       sub_{f:08X}  via {}", path(f));
    }
    for &f in parent.keys().filter(|f| census.contains_key(f) && !cuts.contains(f)) {
        println!("hardware  sub_{f:08X}  via {}", path(f));
    }
    Ok(())
}

fn load(path: &str) -> Result<Xex, Box<dyn Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(Xex::parse(&bytes).map_err(|e| format!("{path}: {e}"))?)
}

fn parse_hex(s: &str) -> Result<u32, std::num::ParseIntError> {
    u32::from_str_radix(&s.trim_start_matches("0x").replace('_', ""), 16)
}

struct Remap<'a> {
    base: remap::Image<'a>,
    tu: remap::Image<'a>,
    m: remap::Matching,
    corr: remap::Corr,
    fmap: std::collections::HashMap<u32, u32>,
}

fn build_remap<'a>(b: &'a Xex, t: &'a Xex) -> Result<Remap<'a>, Box<dyn Error>> {
    let base = remap::Image::new(b)?;
    let tu = remap::Image::new(t)?;
    let m = remap::match_functions(&base, &tu);
    let corr = remap::data_correspondence(&base, &tu, &m);
    let fmap = remap::function_map(&base, &tu, &m);
    Ok(Remap { base, tu, m, corr, fmap })
}

fn match_cmd(b: &Xex, t: &Xex, out: &Path) -> Result<(), Box<dyn Error>> {
    let r = build_remap(b, t)?;
    let (base, tu, m) = (&r.base, &r.tu, &r.m);
    let mut tsv = String::from("base_addr\ttu_addr\tbase_len\ttu_len\tmethod\tconfidence\tnote\n");
    let mut by_method: BTreeMap<&str, usize> = BTreeMap::new();
    let mut low = Vec::new();
    let mut unmatched = Vec::new();
    for (i, f) in base.funcs.iter().enumerate() {
        match m.b2t[i] {
            Some((j, method, c)) => {
                let g = &tu.funcs[j];
                *by_method.entry(method.name()).or_default() += 1;
                let note = if m.moved.contains(&i) { "moved" } else { "" };
                writeln!(tsv, "{:08X}\t{:08X}\t0x{:X}\t0x{:X}\t{}\t{c:.3}\t{note}", f.begin, g.begin, f.len(), g.len(), method.name())?;
                if c < 0.6 {
                    low.push((f.begin, g.begin, method.name(), c));
                }
            }
            None => {
                *by_method.entry("unmatched").or_default() += 1;
                writeln!(tsv, "{:08X}\t-\t0x{:X}\t-\tunmatched\t0\t", f.begin, f.len())?;
                unmatched.push(i);
            }
        }
    }
    std::fs::write(out, tsv)?;
    let mut new = String::from("tu_addr\ttu_len\tsource\n");
    let mut new_count = 0;
    for (j, g) in tu.funcs.iter().enumerate().filter(|(j, _)| m.t2b[*j].is_none()) {
        let _ = j;
        new_count += 1;
        writeln!(new, "{:08X}\t0x{:X}\t{:?}", g.begin, g.len(), g.source)?;
    }
    let new_path = out.with_extension("tu_only.tsv");
    std::fs::write(&new_path, new)?;
    println!("base functions {}, update functions {}", base.funcs.len(), tu.funcs.len());
    for (k, n) in &by_method {
        println!("  {k:<11} {n}");
    }
    println!("pairs outside the longest address-ordered chain (function placed elsewhere in the update, e.g. identical-code folding kept another copy): {}", m.out_of_order);
    println!("update functions with no base counterpart: {new_count} (listed in {})", new_path.display());
    // Ambiguous: unmatched base functions whose body occurs more than once.
    let mut dup: BTreeMap<u64, usize> = BTreeMap::new();
    for f in &base.funcs {
        *dup.entry(f.hash).or_default() += 1;
    }
    let ambiguous = unmatched.iter().filter(|&&i| dup[&base.funcs[i].hash] > 1).count();
    println!("unmatched base functions: {} ({ambiguous} with a body that occurs more than once)", unmatched.len());
    for &i in unmatched.iter().take(40) {
        let f = &base.funcs[i];
        println!("  sub_{:08X} len 0x{:X} {:?}{}", f.begin, f.len(), f.source, if dup[&f.hash] > 1 { " (duplicate body)" } else { "" });
    }
    println!("pairs with confidence < 0.6: {}", low.len());
    for (a, b, mth, c) in low.iter().take(40) {
        println!("  sub_{a:08X} -> sub_{b:08X} {mth} {c:.2}");
    }
    println!("data correspondences from code: {} base addresses", r.corr.len());
    Ok(())
}

fn map_data_cmd(b: &Xex, t: &Xex, addrs: &[u32]) -> Result<(), Box<dyn Error>> {
    let r = build_remap(b, t)?;
    for &x in addrs {
        let mp = remap::map_data(&r.base, &r.tu, &r.corr, &r.fmap, x);
        match mp.tu {
            Some(a) => println!("{x:08X} -> {a:08X}  conf {:.2}  {}", mp.confidence, mp.how),
            None => println!("{x:08X} -> ?  {}", mp.how),
        }
    }
    Ok(())
}

fn remap_cmd(b: &Xex, t: &Xex, root: &Path, out: &Path) -> Result<(), Box<dyn Error>> {
    fn walk(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&p, files)?;
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push(p);
            }
        }
        Ok(())
    }
    let mut files = vec![root.join("config").join("wwe14.toml")];
    walk(&root.join("runtime"), &mut files)?;
    walk(&root.join("tools").join("recomp").join("src"), &mut files)?;
    let r = build_remap(b, t)?;
    let first_section = b.sections.iter().map(|s| s.address).min().unwrap_or(b.base_address);
    let mut tsv = String::from("location\tform\tbase\tkind\ttu\tconfidence\thow\n");
    let mut flagged = Vec::new();
    let mut total = 0;
    for path in &files {
        let text = std::fs::read_to_string(path)?;
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        for lit in remap::find_literals(&rel, &text) {
            total += 1;
            let x = lit.value;
            let sec = r.base.section_of(x);
            let (kind, mp) = match sec {
                _ if lit.form == "hex (test code)" => (
                    "test value, not a game address (keep)".to_string(),
                    remap::Mapped { tu: Some(x), confidence: 1.0, how: "in a #[cfg(test)] module".into() },
                ),
                _ if x == b.base_address => (
                    "image base / range bound (keep)".to_string(),
                    remap::Mapped { tu: Some(x), confidence: 1.0, how: "both images load at this base".into() },
                ),
                Some(s) if s.is_code() => remap::map_code(&r.base, &r.tu, &r.m, x),
                Some(s) => (format!("data ({})", s.name), remap::map_data(&r.base, &r.tu, &r.corr, &r.fmap, x)),
                None if x >= b.base_address && x < first_section => {
                    let same = b.bytes_at(x, 4) == t.bytes_at(x, 4);
                    (
                        "image header".to_string(),
                        remap::Mapped {
                            tu: Some(x),
                            confidence: if same { 1.0 } else { 0.5 },
                            how: format!("header bytes {}", if same { "equal" } else { "differ" }),
                        },
                    )
                }
                None => (
                    "not an image address (constant or range bound)".to_string(),
                    remap::Mapped { tu: Some(x), confidence: 1.0, how: "unchanged".into() },
                ),
            };
            let tu = mp.tu.map(|a| format!("0x{a:08X}")).unwrap_or_else(|| "UNMAPPED".into());
            writeln!(tsv, "{}:{}\t{}\t0x{x:08X}\t{kind}\t{tu}\t{:.2}\t{}", lit.file, lit.line, lit.form, mp.confidence, mp.how)?;
            if mp.tu.is_none() || mp.confidence < 0.9 {
                flagged.push(format!("{}:{} 0x{x:08X} {kind} -> {tu} ({:.2}) {}", lit.file, lit.line, mp.confidence, mp.how));
            }
        }
    }
    std::fs::write(out, tsv)?;
    println!("{total} literals in {} files -> {}", files.len(), out.display());
    println!("unmapped or confidence < 0.9: {}", flagged.len());
    for f in &flagged {
        println!("  {f}");
    }
    Ok(())
}

fn helpers_cmd(code: &Code) -> Result<(), Box<dyn Error>> {
    for h in helpers::find(code)? {
        println!(
            "{:<14} 0x{:08X}..0x{:08X}  entries r{}..r{}",
            h.kind.name,
            h.address,
            h.end(),
            h.entries().next().unwrap().0,
            h.entries().last().unwrap().0
        );
    }
    Ok(())
}

fn switches_cmd(code: &Code, examples: usize) -> Result<(), Box<dyn Error>> {
    let sites = switches::find_all(code);
    let mut by_kind: BTreeMap<&str, Vec<&switches::BctrSite>> = BTreeMap::new();
    let mut table_kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut cases = 0usize;
    for s in &sites {
        let key = match &s.kind {
            BctrKind::Switch(t) => {
                *table_kinds.entry(format!("{:?}", t.kind)).or_default() += 1;
                cases += t.labels.len();
                "switch"
            }
            BctrKind::TailCallViaPointer => "tail call via pointer",
            BctrKind::TailCallViaTable { .. } => "tail call via table",
            BctrKind::TailCallViaRegister => "tail call via register",
            BctrKind::TableWithoutBound { .. } => "table without bound",
            BctrKind::Unknown { .. } => "unknown",
        };
        by_kind.entry(key).or_default().push(s);
    }
    println!("unconditional bctr sites: {}", sites.len());
    for (k, v) in &by_kind {
        println!("  {k:<22} {}", v.len());
    }
    println!("switch tables by kind: {table_kinds:?} ({cases} case labels)");
    let inferred = sites.iter().filter(|s| matches!(&s.kind, BctrKind::Switch(t) if t.inferred_bound)).count();
    println!("switch tables with a bound inferred from the inline table's extent: {inferred}");
    for key in ["table without bound", "unknown"] {
        if let Some(list) = by_kind.get(key) {
            println!("{key} (first {examples}):");
            for s in list.iter().take(examples) {
                println!("  0x{:08X}: {:?}", s.address, s.kind);
            }
        }
    }
    if let Some(list) = by_kind.get("switch") {
        println!("sample switch tables:");
        for s in list.iter().take(examples.min(6)) {
            if let BctrKind::Switch(t) = &s.kind {
                let shown: Vec<String> = t.labels.iter().take(6).map(|l| format!("0x{l:08X}")).collect();
                println!(
                    "  bctr 0x{:08X} {:?} table 0x{:08X} r{} default {:?} {} cases: {}{}",
                    t.bctr,
                    t.kind,
                    t.table,
                    t.reg,
                    t.default.map(|d| format!("0x{d:08X}")),
                    t.labels.len(),
                    shown.join(" "),
                    if t.labels.len() > 6 { " ..." } else { "" }
                );
            }
        }
    }
    Ok(())
}

fn functions_cmd(code: &Code, examples: usize) -> Result<(), Box<dyn Error>> {
    let pdata = code.xex.pdata()?;
    let helpers = helpers::find(code)?;
    let tables: Vec<switches::SwitchTable> = switches::find_all(code)
        .into_iter()
        .filter_map(|s| match s.kind {
            BctrKind::Switch(t) => Some(t),
            _ => None,
        })
        .collect();
    let thunks: Vec<u32> = code
        .xex
        .imports
        .iter()
        .flat_map(|l| l.functions().map(|r| r.address))
        .collect();
    let d = functions::discover(code, &pdata, &helpers, &tables, &thunks);
    if let Some(f) = d.functions.iter().find(|f| f.falls_through) {
        println!("falls through: 0x{:08X}..0x{:08X} ({:?})", f.begin, f.end, f.source);
    }

    let mut by_source: BTreeMap<Source, (usize, u64)> = BTreeMap::new();
    for f in &d.functions {
        let e = by_source.entry(f.source).or_default();
        e.0 += 1;
        e.1 += (f.end - f.begin) as u64;
    }
    println!("functions: {}", d.functions.len());
    for (s, (n, bytes)) in &by_source {
        println!("  {:<12} {n:>6} functions, {:>9} bytes", format!("{s:?}"), bytes);
    }
    let falls = d.functions.iter().filter(|f| f.falls_through).count();
    println!("  fall through into the next function: {falls}");

    let mut data_by_kind: BTreeMap<DataKind, (usize, u64)> = BTreeMap::new();
    for &(b, e, k) in &d.data {
        let x = data_by_kind.entry(k).or_default();
        x.0 += 1;
        x.1 += (e - b) as u64;
    }
    println!("data inside code sections:");
    for (k, (n, bytes)) in &data_by_kind {
        println!("  {:<20} {n:>6} ranges, {bytes:>8} bytes", format!("{k:?}"));
    }

    // Every word of every code section must be owned exactly once (secondary
    // entries deliberately overlap their containers and are excluded).
    let mut ranges: Vec<(u32, u32, &str)> = d
        .functions
        .iter()
        .filter(|f| f.source != Source::SecondaryEntry)
        .map(|f| (f.begin, f.end, "function"))
        .collect();
    ranges.extend(d.data.iter().map(|&(b, e, _)| (b, e, "data")));
    ranges.sort();
    let mut overlaps = Vec::new();
    for w in ranges.windows(2) {
        if w[0].1 > w[1].0 {
            overlaps.push(format!("{} 0x{:08X}..0x{:08X} overlaps {} 0x{:08X}..", w[0].2, w[0].0, w[0].1, w[1].2, w[1].0));
        }
    }
    let total: u64 = code.ranges.iter().map(|&(b, e)| (e - b) as u64).sum();
    let owned: u64 = ranges.iter().map(|&(b, e, _)| (e - b) as u64).sum();
    let mut unowned = 0u64;
    for (addr, _) in code.words() {
        let i = ranges.partition_point(|r| r.0 <= addr);
        if i == 0 || addr >= ranges[i - 1].1 {
            unowned += 4;
        }
    }
    println!("ownership: {total} code bytes, {owned} owned, {unowned} unowned, {} overlaps", overlaps.len());
    for o in overlaps.iter().take(examples) {
        println!("  {o}");
    }

    if !d.problems.is_empty() {
        println!("problems: {} (first {examples})", d.problems.len());
        for p in d.problems.iter().take(examples) {
            println!("  {p}");
        }
    }
    if !d.calls_into_middle.is_empty() {
        println!("calls into the middle of a function: {} (first {examples})", d.calls_into_middle.len());
        for (from, to) in d.calls_into_middle.iter().take(examples) {
            let owner = d.function_at(*to).map(|f| format!("0x{:08X}", f.begin)).unwrap_or_default();
            println!("  bl at 0x{from:08X} -> 0x{to:08X} (inside {owner})");
        }
    }
    // Code pointers stored in data sections (vtables, callback tables, ...).
    // .pdata and .reloc hold code addresses by construction, so they're skipped.
    let mut pointer_targets: BTreeMap<u32, u32> = BTreeMap::new();
    for s in code.xex.sections.iter().filter(|s| !s.is_code() && s.name != ".pdata" && s.name != ".reloc") {
        for a in (s.address..s.end().saturating_sub(3)).step_by(4) {
            if let Some(w) = code.word(a)
                && code.is_code(w)
            {
                *pointer_targets.entry(w).or_default() += 1;
            }
        }
    }
    let starts: BTreeMap<u32, Source> = d.functions.iter().map(|f| (f.begin, f.source)).collect();
    let mut by_target: BTreeMap<String, usize> = BTreeMap::new();
    for &t in pointer_targets.keys() {
        let key = match starts.get(&t) {
            Some(s) => format!("start of {s:?} function"),
            None if d.function_at(t).is_some() => "middle of a function".to_string(),
            None => "code-section data".to_string(),
        };
        *by_target.entry(key).or_default() += 1;
    }
    println!("code addresses stored in data sections: {} distinct targets", pointer_targets.len());
    for (k, n) in &by_target {
        println!("  {k:<32} {n}");
    }
    let mut middle_by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    let mut middle_examples = Vec::new();
    for &t in pointer_targets.keys().filter(|t| !starts.contains_key(t)) {
        let Some(f) = d.function_at(t) else { continue };
        let kind = if f.has_exception_handler { "inside SEH function" } else { "inside non-SEH function" };
        *middle_by_kind.entry(kind).or_default() += 1;
        if kind == "inside non-SEH function" && middle_examples.len() < examples {
            middle_examples.push(format!("0x{t:08X} (in 0x{:08X} {:?})", f.begin, f.source));
        }
    }
    println!("  mid-function targets: {middle_by_kind:?}");
    if !middle_examples.is_empty() {
        println!("  e.g. {}", middle_examples.join(", "));
    }
    let sweep: Vec<&functions::Function> = d.functions.iter().filter(|f| f.source == Source::Sweep).collect();
    let referenced = sweep.iter().filter(|f| pointer_targets.contains_key(&f.begin)).count();
    println!(
        "swept functions referenced from data: {referenced} of {} ({} unreferenced: dead code or computed addresses)",
        sweep.len(),
        sweep.len() - referenced
    );

    let invalid: Vec<String> = d
        .data
        .iter()
        .filter(|r| matches!(r.2, DataKind::Undecodable | DataKind::InvalidFlow))
        .take(examples)
        .map(|r| format!("0x{:08X}..0x{:08X} {:?}", r.0, r.1, r.2))
        .collect();
    if !invalid.is_empty() {
        println!("non-code ranges (first {examples}): {}", invalid.join(", "));
    }
    Ok(())
}

/// Writes review files and checks the Phase 1 gate:
/// every code byte owned exactly once, and no unclassified `bctr`.
fn report(xex: &Xex, out: &Path) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let d = &a.discovery;
    std::fs::create_dir_all(out)?;

    // Switch tables in XenonRecomp's TOML layout (plus our extra fields), for review and diffing.
    let mut s = String::from("# Generated by `analyse report`. Regenerate rather than editing.\n\n");
    for t in a.switch_tables() {
        writeln!(s, "[[switch]]\nbase = 0x{:X}\nr = {}\ndefault = 0x{:X}", t.bctr, t.reg, t.default.unwrap_or(0))?;
        writeln!(s, "kind = \"{:?}\"\ntable = 0x{:X}\ninferred_bound = {}", t.kind, t.table, t.inferred_bound)?;
        writeln!(s, "labels = [")?;
        for l in &t.labels {
            writeln!(s, "    0x{l:X},")?;
        }
        s.push_str("]\n\n");
    }
    std::fs::write(out.join("switch_tables.toml"), s)?;

    // Function list: begin, end, source, flags, name (import thunks and helpers).
    let helper_names: BTreeMap<u32, &str> = a.helpers.iter().map(|h| (h.address, h.kind.name)).collect();
    let mut s = String::from("# begin\tend\tsource\tflags\tname\n");
    for f in &d.functions {
        let mut flags = Vec::new();
        if f.has_exception_handler {
            flags.push("seh");
        }
        if f.falls_through {
            flags.push("falls_through");
        }
        let name = match f.source {
            Source::ImportThunk => a.import_thunks.get(&f.begin).map(|(l, n)| format!("{l}!{n}")).unwrap_or_default(),
            Source::AbiHelper => helper_names.get(&f.begin).map(|n| format!("__{n}")).unwrap_or_default(),
            _ => String::new(),
        };
        writeln!(s, "{:08X}\t{:08X}\t{:?}\t{}\t{name}", f.begin, f.end, f.source, flags.join(","))?;
    }
    std::fs::write(out.join("functions.tsv"), s)?;

    // Indirect branches that aren't switches.
    let mut s = String::from("# bctr\tkind\n");
    for site in &a.bctr_sites {
        let kind = match &site.kind {
            BctrKind::Switch(_) => continue,
            BctrKind::TailCallViaPointer => "tail call via pointer".to_string(),
            BctrKind::TailCallViaRegister => "tail call via register".to_string(),
            BctrKind::TailCallViaTable { table } => format!("tail call via table 0x{table:08X}"),
            BctrKind::TableWithoutBound { table } => format!("UNCLASSIFIED: table 0x{table:08X} without bound"),
            BctrKind::Unknown { reason } => format!("UNCLASSIFIED: {reason}"),
        };
        writeln!(s, "{:08X}\t{kind}", site.address)?;
    }
    std::fs::write(out.join("indirect_branches.tsv"), s)?;

    // Functions that touch the MMIO window: candidates for native replacement.
    let mmio = analyse::mmio_functions(xex, d);
    let mut s = String::from("# Functions loading an MMIO-window address (0x7FC0xxxx..0x7FFExxxx).\n");
    s.push_str("# Under the no-emulation rule, every one must be replaced natively or proven unreachable.\n");
    for (f, sites) in &mmio {
        let list: Vec<String> = sites.iter().map(|(a, hi)| format!("0x{a:08X}:lis 0x{hi:04X}")).collect();
        writeln!(s, "{f:08X}\t{}", list.join(" "))?;
    }
    std::fs::write(out.join("mmio_functions.tsv"), s)?;

    // SEH functions, and longjmp candidates (callers of RtlUnwind).
    let mut s = String::from("# Functions with an SEH handler (__try/__except/__finally)\n");
    for f in d.functions.iter().filter(|f| f.has_exception_handler) {
        let handler = xex.bytes_at(f.begin - 8, 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0);
        writeln!(s, "{:08X}\t{:08X}\thandler {handler:08X}", f.begin, f.end)?;
    }
    s.push_str("\n# Secondary entry points (SEH __finally funclets called directly)\n");
    for f in d.functions.iter().filter(|f| f.source == Source::SecondaryEntry) {
        writeln!(s, "{:08X}\t{:08X}", f.begin, f.end)?;
    }
    let rtl_unwind = a.import_thunks.iter().find(|(_, (_, n))| n == "RtlUnwind").map(|(&t, _)| t);
    s.push_str("\n# Callers of RtlUnwind (longjmp candidates; setjmp is usually the next function)\n");
    if let Some(t) = rtl_unwind {
        for (f, at) in analyse::callers_of(xex, d, t) {
            let next = d.functions.iter().find(|g| g.begin > f && g.source != Source::SecondaryEntry).map(|g| g.begin);
            writeln!(s, "{f:08X}\tcalls RtlUnwind at {at:08X}\tnext function {:08X}", next.unwrap_or(0))?;
        }
    }
    std::fs::write(out.join("seh_and_longjmp.txt"), s)?;

    // Gate.
    let mut ranges: Vec<(u32, u32)> =
        d.functions.iter().filter(|f| f.source != Source::SecondaryEntry).map(|f| (f.begin, f.end)).collect();
    ranges.extend(d.data.iter().map(|&(b, e, _)| (b, e)));
    ranges.sort();
    let overlaps = ranges.windows(2).filter(|w| w[0].1 > w[1].0).count();
    let code = Code::new(xex);
    let total: u64 = code.ranges.iter().map(|&(b, e)| (e - b) as u64).sum();
    let owned: u64 = ranges.iter().map(|&(b, e)| (e - b) as u64).sum();
    let unclassified = a.unclassified_bctr().count();
    let stray_embedded =
        d.embedded.iter().filter(|&&(b, e)| d.function_at(b).is_none_or(|f| e > f.end)).count();
    let escaping_switches = a
        .switch_tables()
        .filter(|t| {
            let owner = d.function_at(t.bctr);
            owner.is_none_or(|f| t.labels.iter().any(|l| !(f.begin..f.end).contains(l)))
        })
        .count();
    let undecodable: u64 =
        d.data.iter().filter(|r| matches!(r.2, DataKind::Undecodable | DataKind::InvalidFlow)).map(|r| (r.1 - r.0) as u64).sum();
    let mut by_source: BTreeMap<Source, usize> = BTreeMap::new();
    for f in &d.functions {
        *by_source.entry(f.source).or_default() += 1;
    }
    let summary = format!(
        "functions: {} {:?}\nswitch tables: {} ({} labels), {} inline in code, {} not inside one function, {} escaping their function\n\
         indirect branches: {} total, {} unclassified\n\
         code bytes: {total}, owned: {owned}, overlaps: {overlaps}, undecodable/invalid data: {undecodable}\n\
         MMIO functions: {}\nSEH functions: {}\nproblems: {}\n",
        d.functions.len(),
        by_source,
        a.switch_tables().count(),
        a.switch_tables().map(|t| t.labels.len()).sum::<usize>(),
        d.embedded.len(),
        stray_embedded,
        escaping_switches,
        a.bctr_sites.len(),
        unclassified,
        mmio.len(),
        d.functions.iter().filter(|f| f.has_exception_handler).count(),
        d.problems.len(),
    );
    std::fs::write(out.join("summary.txt"), &summary)?;
    print!("{summary}");
    println!("wrote switch_tables.toml, functions.tsv, indirect_branches.tsv, mmio_functions.tsv, seh_and_longjmp.txt, summary.txt to {}", out.display());

    let pass = owned == total
        && overlaps == 0
        && unclassified == 0
        && undecodable == 0
        && stray_embedded == 0
        && escaping_switches == 0
        && d.problems.is_empty();
    if pass {
        println!("PHASE 1 GATE: PASS");
        Ok(())
    } else {
        Err("PHASE 1 GATE: FAIL (see summary above)".into())
    }
}

/// Call sites of an import (by name) or a function (by address), each with the
/// instructions leading up to the call, to check argument conventions.
fn callers_cmd(xex: &Xex, target: &str, context: usize) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let code = Code::new(xex);
    let addr = match u32::from_str_radix(target.trim_start_matches("0x"), 16) {
        Ok(v) if target.starts_with("0x") || target.len() == 8 => v,
        _ => *a
            .import_thunks
            .iter()
            .find(|(_, (_, n))| n == target)
            .map(|(t, _)| t)
            .ok_or_else(|| format!("no import named {target}"))?,
    };
    let sites = analyse::callers_of(xex, &a.discovery, addr);
    println!("{} call sites of 0x{addr:08X}", sites.len());
    for (f, site) in sites {
        println!("-- in sub_{f:08X}");
        let from = site.saturating_sub(4 * context as u32).max(f);
        let mut p = from;
        while p <= site {
            if let Some(i) = code.instr(p) {
                println!("  {p:08X}  {i}");
            }
            p += 4;
        }
    }
    Ok(())
}

/// Histogram of `r13`-relative accesses: which PCR fields the game reads and writes.
fn r13_cmd(xex: &Xex) -> Result<(), Box<dyn Error>> {
    let code = Code::new(xex);
    let a = analyse::analyse(xex)?;
    let in_code = |addr: u32| {
        a.discovery.function_at(addr).is_some_and(|f| f.source != Source::ImportThunk)
            && !a.discovery.embedded.iter().any(|&(s, e)| addr >= s && addr < e)
    };
    let mut seen: BTreeMap<(i32, String), Vec<u32>> = BTreeMap::new();
    let mut other: BTreeMap<String, usize> = BTreeMap::new();
    for (addr, word) in code.words() {
        let Some(i) = ppc::Instr::decode(addr, word) else { continue };
        if !in_code(addr) {
            continue;
        }
        let t = i.op.info().disasm.unwrap_or("");
        let uses_ra = t.contains("[RA]") || t.contains("[RA0]");
        if !uses_ra || i.ra() != 13 {
            continue;
        }
        let m = i.op.mnemonic().to_string();
        match i.operand("d").or_else(|| i.operand("ds")) {
            Some(d) => {
                seen.entry((d as i32, m)).or_default().push(addr);
            }
            None => *other.entry(format!("{i}")).or_default() += 1,
        }
    }
    for ((d, m), sites) in &seen {
        let shown: Vec<String> = sites.iter().take(12).map(|s| format!("{s:08X}")).collect();
        println!("{m:<8} {d:#06x}(r13)  x{:<6} at {}", sites.len(), shown.join(" "));
    }
    for (s, n) in &other {
        println!("{s}  x{n}");
    }
    Ok(())
}

/// Instructions that form `target` from a `lis rX, hi` and a following
/// `addi`/load/store with displacement through rX in the same function: the
/// usual way code addresses a global. Shows `context` instructions after each.
fn refs_cmd(xex: &Xex, target: u32, context: usize) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let code = Code::new(xex);
    let mut found = 0;
    for f in &a.discovery.functions {
        if f.source == Source::ImportThunk || f.source == Source::SecondaryEntry {
            continue;
        }
        // Last `lis` value per register, forgotten after 64 instructions.
        let mut hi: [Option<(u32, u32)>; 32] = [None; 32];
        let mut p = f.begin;
        while p < f.end {
            if let Some(i) = code.instr(p) {
                let d = i.operand("d").or_else(|| i.operand("ds")).or_else(|| (i.op == ppc::Op::addi).then(|| i.simm16() as i64));
                if i.ra() != 0
                    && let (Some(d), Some((h, at))) = (d, hi[i.ra() as usize])
                    && p - at < 256
                    && h.wrapping_add(d as i32 as u32) == target
                    && i.op != ppc::Op::addis
                {
                    found += 1;
                    println!("-- sub_{:08X} at {p:08X}", f.begin);
                    for k in 0..=context as u32 {
                        if let Some(n) = code.instr(p + 4 * k) {
                            println!("  {:08X}  {n}", p + 4 * k);
                        }
                    }
                }
                if i.op == ppc::Op::addis && i.ra() == 0 {
                    hi[i.rt() as usize] = Some(((i.simm16() as u32) << 16, p));
                } else {
                    for r in analyse::code::gprs_written(&i) {
                        if !(i.op == ppc::Op::addis && r == i.rt()) {
                            hi[r as usize] = None;
                        }
                    }
                }
            }
            p += 4;
        }
    }
    println!("{found} references to 0x{target:08X}");
    Ok(())
}

/// GPU census: every function that forms PM4 packet headers, touches MMIO or
/// calls GPU-facing imports. With a `[lo, hi)` range (a library's code), also
/// lists the range's functions that code outside it calls: its API surface.
fn gpu_cmd(xex: &Xex, span: Option<(u32, u32)>, examples: usize) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let census = analyse::gpu::census(xex, &a);
    let mut total: BTreeMap<u32, usize> = BTreeMap::new();
    println!("{} functions with GPU evidence (PM4 type-3 headers, MMIO, GPU imports):", census.len());
    for (f, u) in &census {
        for (op, n) in &u.packets {
            *total.entry(*op).or_default() += n;
        }
        let packets: Vec<String> = u
            .packets
            .iter()
            .map(|(op, n)| format!("{}x{n}", analyse::gpu::opcode_name(*op).unwrap_or("?")))
            .collect();
        let inside = span.is_none_or(|(lo, hi)| (lo..hi).contains(f));
        println!(
            "  {}sub_{f:08X}  {}{}{}",
            if inside { "" } else { "OUTSIDE " },
            packets.join(" "),
            if u.mmio > 0 { format!(" mmio x{}", u.mmio) } else { String::new() },
            if u.imports.is_empty() { String::new() } else { format!(" calls {}", u.imports.iter().cloned().collect::<Vec<_>>().join(",")) }
        );
    }
    println!("packet totals:");
    for (op, n) in &total {
        println!("  0x{op:02X} {:<20} {n}", analyse::gpu::opcode_name(*op).unwrap_or("?"));
    }
    let reachable = analyse::gpu::reachable_gpu(xex, &a, &census, &BTreeSet::new());
    println!("reachable from recompiled code with nothing replaced: {} (one path each):", reachable.len());
    for (f, path) in reachable.iter().take(examples.max(16)) {
        let p: Vec<String> = path.iter().map(|x| format!("{x:08X}")).collect();
        println!("  sub_{f:08X}  via {}", p.join(" -> "));
    }
    let Some((lo, hi)) = span else { return Ok(()) };
    let inside = |x: u32| (lo..hi).contains(&x);
    let edges = analyse::gpu::call_edges(xex, &a);
    let mut surface: BTreeMap<u32, (usize, BTreeSet<u32>)> = BTreeMap::new();
    let mut outgoing: BTreeMap<u32, usize> = BTreeMap::new();
    for &(caller, callee) in &edges {
        if inside(callee) && !inside(caller) {
            let e = surface.entry(callee).or_default();
            e.0 += 1;
            e.1.insert(caller);
        }
        if inside(caller) && !inside(callee) && !a.import_thunks.contains_key(&callee) {
            *outgoing.entry(callee).or_default() += 1;
        }
    }
    let fns = a.discovery.functions.iter().filter(|f| inside(f.begin) && f.source != Source::SecondaryEntry).count();
    println!("\nrange 0x{lo:08X}..0x{hi:08X}: {fns} functions; {} called from outside ({} call sites):", surface.len(), surface.values().map(|e| e.0).sum::<usize>());
    let mut by_count: Vec<(&u32, &(usize, BTreeSet<u32>))> = surface.iter().collect();
    by_count.sort_by(|x, y| y.1.0.cmp(&x.1.0).then(x.0.cmp(y.0)));
    for (f, (n, callers)) in by_count {
        let sample: Vec<String> = callers.iter().take(examples.min(4)).map(|c| format!("{c:08X}")).collect();
        println!("  sub_{f:08X}  {n:>5} sites from {:>4} functions  e.g. {}", callers.len(), sample.join(" "));
    }
    println!("\ncalls from the range to code outside it (helpers, CRT, or a range boundary that is wrong):");
    for (f, n) in &outgoing {
        println!("  sub_{f:08X} x{n}");
    }
    Ok(())
}

/// Functions in `[lo, hi)`: size, direct callers from inside/outside the range
/// (with the outside callers' address span), and the outside functions each calls.
/// Used to find where one statically linked library ends and the next begins.
fn fns_cmd(xex: &Xex, lo: u32, hi: u32) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let edges = analyse::gpu::call_edges(xex, &a);
    let inside = |x: u32| (lo..hi).contains(&x);
    let mut callers: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut callees: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for &(from, to) in &edges {
        callers.entry(to).or_default().push(from);
        callees.entry(from).or_default().insert(to);
    }
    for f in a.discovery.functions.iter().filter(|f| inside(f.begin) && f.source != Source::SecondaryEntry) {
        let cs = callers.get(&f.begin).map(Vec::as_slice).unwrap_or(&[]);
        let (ins, outs): (Vec<u32>, Vec<u32>) = cs.iter().partition(|&&c| inside(c));
        let out_span = match (outs.iter().min(), outs.iter().max()) {
            (Some(a), Some(b)) => format!(" out {:08X}..{:08X}", a, b),
            _ => String::new(),
        };
        let ext: Vec<String> = callees
            .get(&f.begin)
            .map(|s| {
                s.iter()
                    .filter(|&&t| !inside(t))
                    .map(|t| a.import_thunks.get(t).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("{t:08X}")))
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "{:08X} {:>5}B  callers in {:>3} out {:>3}{out_span}  calls-out [{}]",
            f.begin,
            f.end - f.begin,
            ins.len(),
            outs.len(),
            ext.join(" ")
        );
    }
    Ok(())
}

/// What a function does to the object passed in r3 (for D3D, the device):
/// every store through a register still holding that pointer, every load from
/// it, and every call. Registers are followed through `or rX, rY, rY` copies
/// within the function; a call clobbers the volatile ones (r3-r12).
fn effects_cmd(xex: &Xex, f: u32) -> Result<(), Box<dyn Error>> {
    let a = analyse::analyse(xex)?;
    let code = Code::new(xex);
    let func = a.discovery.function_at(f).filter(|x| x.begin == f).ok_or("not a function entry")?;
    let mut holds = [false; 32];
    holds[3] = true;
    let mut stores: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    let mut loads: BTreeMap<i64, usize> = BTreeMap::new();
    let mut calls: Vec<String> = Vec::new();
    let mut p = func.begin;
    while p < func.end {
        let Some(i) = code.instr(p) else {
            p += 4;
            continue;
        };
        let m = i.op.mnemonic();
        let d = i.operand("d").or_else(|| i.operand("ds"));
        // `or rA, rS, rS` (mr): copies the pointer.
        if i.op == ppc::Op::orx && i.rt() == i.rb() {
            holds[i.ra() as usize] = holds[i.rt() as usize];
        } else if let Some(d) = d
            && i.ra() != 0
            && holds[i.ra() as usize]
        {
            if m.starts_with("st") {
                stores.entry(d).or_default().push(format!("{m}@{p:08X}"));
            } else if m.starts_with('l') {
                *loads.entry(d).or_default() += 1;
            }
            for r in analyse::code::gprs_written(&i) {
                if !(m.starts_with("st") && r == i.ra() && !m.ends_with('u')) {
                    holds[r as usize] = false;
                }
            }
        } else {
            for r in analyse::code::gprs_written(&i) {
                holds[r as usize] = false;
            }
        }
        if analyse::code::is_call(&i) {
            let t = i.branch_target();
            if t.is_some_and(|t| a.helpers.iter().any(|h| (h.address..h.end()).contains(&t))) {
                p += 4;
                continue;
            }
            calls.push(match t.and_then(|t| a.import_thunks.get(&t)) {
                Some((_, n)) => format!("{p:08X}: {n}"),
                None => format!("{p:08X}: {}", t.map_or("indirect".into(), |t| format!("sub_{t:08X}"))),
            });
            // The register save/restore helpers keep the argument registers.
            let helper = t.is_some_and(|t| a.helpers.iter().any(|h| (h.address..h.end()).contains(&t)));
            if !helper {
                for r in 3..=12 {
                    holds[r] = false;
                }
            }
        }
        p += 4;
    }
    println!("sub_{f:08X} ({} bytes)", func.end - func.begin);
    println!("stores through r3's object:");
    for (off, sites) in &stores {
        println!("  +0x{off:04X}  {}", sites.join(" "));
    }
    let l: Vec<String> = loads.keys().map(|o| format!("+0x{o:X}")).collect();
    println!("loads from it: {}", l.join(" "));
    println!("calls:");
    for c in &calls {
        println!("  {c}");
    }
    Ok(())
}
