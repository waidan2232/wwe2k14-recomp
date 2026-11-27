//! Ahead-of-time recompiler: turns the analysed executable into Rust crates.
//!
//! Output, under `<out_dir>`, is a separate Cargo workspace:
//! - `shards/code_NNN`: recompiled functions, split by instruction count. Calls
//!   between shards resolve at link time through `#[no_mangle]` symbols.
//! - `imports`: the list of imported kernel/XAM functions, and one stub per
//!   import thunk that calls the native implementation the kernel installed.
//! - `table`: the sorted guest-address -> function table used for indirect calls.
//! - `game`: the binary. It links everything and hands it to the native kernel
//!   (`runtime/kernel`), which loads the executable's data and boots it.
//!
//! Translation happens once, here. Nothing is interpreted at runtime, and code
//! the analysis couldn't classify stops the build instead of falling back.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use analyse::{Code, Source};
use recomp::emit::{self, FnDef, Program, emit_program, fn_name};
use recomp::locals;
use serde::Deserialize;
use xex::Xex;

const USAGE: &str =
    "usage: recomp <default.xex> <out_dir> [--config FILE] [--shard-instructions N] [--trace-calls on] [--locals on|off]";

/// `config/wwe14.toml`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    nonlocal: Nonlocal,
    #[serde(default)]
    replace: Vec<Replace>,
    #[serde(default)]
    unreachable: Vec<Unreachable>,
    #[serde(default)]
    gpu: GpuGate,
    #[serde(default)]
    locals: Locals,
}

/// `[locals]`: registers as host locals (`recomp::locals`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Locals {
    /// Off: every register stays in the context struct, as in a build made
    /// with `--locals off` (crash reports then show every register).
    #[serde(default = "enabled")]
    enabled: bool,
    /// Functions that are called through a pointer although the analysis
    /// finds no pointer to them (an address computed at run time): treated as
    /// address-taken.
    #[serde(default)]
    indirect: Vec<u32>,
    /// Functions that returned with a preserved register changed although the
    /// analysis took them to restore it (the run-time check named them):
    /// handled by the general rules instead.
    #[serde(default)]
    untrusted: Vec<u32>,
}

fn enabled() -> bool {
    true
}

impl Default for Locals {
    fn default() -> Self {
        Locals { enabled: true, indirect: Vec::new(), untrusted: Vec::new() }
    }
}

/// `[[unreachable]]`: a hardware-facing guest function (it builds GPU command
/// packets, touches MMIO or drives the GPU through its imports) that the port
/// never runs: its callers are replaced natively, or it was never called in a
/// whole session (call tracing). Its body becomes a fatal error naming it, so
/// a path that does reach it fails loudly and gets a native version; for the
/// GPU gate it is a cut point, like a replaced function.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unreachable {
    address: u32,
    /// What it is, for the error.
    note: String,
}

/// `[[replace]]`: a guest function whose callers get a native implementation,
/// installed by `name` at boot. Its original body stays callable as `__imp__sub_X`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replace {
    address: u32,
    name: String,
}

/// `[gpu]`: every function that builds GPU command packets, touches MMIO or
/// calls GPU-facing imports must be unreachable from recompiled code except
/// through a replaced function. With `enforce`, a reachable one stops the build.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GpuGate {
    #[serde(default)]
    enforce: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Nonlocal {
    #[serde(default)]
    setjmp: Vec<u32>,
    #[serde(default)]
    longjmp_returns: Vec<u32>,
}

/// Checks the config against the analysis. Every claim must hold.
fn check_config(cfg: &Config, xex: &Xex, a: &analyse::Analysis) -> Result<(), String> {
    let found: BTreeSet<u32> = analyse::stack_switch_returns(xex, &a.discovery).into_iter().collect();
    let listed: BTreeSet<u32> = cfg.nonlocal.longjmp_returns.iter().copied().collect();
    let missing: Vec<String> = found.difference(&listed).map(|x| format!("0x{x:08X}")).collect();
    let extra: Vec<String> = listed.difference(&found).map(|x| format!("0x{x:08X}")).collect();
    if !missing.is_empty() || !extra.is_empty() {
        return Err(format!(
            "config [nonlocal] longjmp_returns doesn't match the analysis: stack-switching returns not listed: [{}]; \
             listed but not found: [{}]",
            missing.join(", "),
            extra.join(", ")
        ));
    }
    if !listed.is_empty() && cfg.nonlocal.setjmp.is_empty() {
        return Err("config lists longjmp returns but no setjmp for them to resume".into());
    }
    for &s in &cfg.nonlocal.setjmp {
        match a.discovery.function_at(s) {
            Some(f) if f.begin == s && f.source != Source::ImportThunk => {}
            _ => return Err(format!("config [nonlocal] setjmp 0x{s:08X} is not a function entry")),
        }
    }
    let mut addrs = BTreeSet::new();
    let mut names = BTreeSet::new();
    for r in &cfg.replace {
        match a.discovery.function_at(r.address) {
            Some(f) if f.begin == r.address && matches!(f.source, Source::Pdata | Source::CallTarget | Source::Sweep) => {}
            _ => return Err(format!("config [[replace]] {} 0x{:08X} is not an ordinary function entry", r.name, r.address)),
        }
        if !r.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || r.name.is_empty() {
            return Err(format!("config [[replace]] name {:?} must be an identifier", r.name));
        }
        if !addrs.insert(r.address) || !names.insert(r.name.as_str()) {
            return Err(format!("config [[replace]] {} 0x{:08X} is listed twice", r.name, r.address));
        }
    }
    for &t in cfg.locals.indirect.iter().chain(&cfg.locals.untrusted) {
        match a.discovery.function_at(t) {
            Some(f) if f.begin == t && f.source != Source::ImportThunk => {}
            _ => return Err(format!("config [locals] 0x{t:08X} is not a function entry")),
        }
    }
    for u in &cfg.unreachable {
        match a.discovery.function_at(u.address) {
            Some(f) if f.begin == u.address && matches!(f.source, Source::Pdata | Source::CallTarget | Source::Sweep) => {}
            _ => return Err(format!("config [[unreachable]] 0x{:08X} is not an ordinary function entry", u.address)),
        }
        if !addrs.insert(u.address) {
            return Err(format!("config [[unreachable]] 0x{:08X} is listed twice or also replaced", u.address));
        }
        if u.note.contains(['"', '\\', '{', '}']) {
            return Err(format!("config [[unreachable]] 0x{:08X}: the note may not contain quotes, backslashes or braces", u.address));
        }
    }
    Ok(())
}

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
    // Flags with a value, then the two positional arguments.
    let mut flags: HashMap<&str, &str> = HashMap::new();
    let mut positional: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(name) = arg.strip_prefix("--") {
            let value = it.next().ok_or(USAGE)?;
            flags.insert(name, value);
        } else {
            positional.push(arg);
        }
    }
    let per_shard: usize = flags.get("shard-instructions").map(|s| s.parse()).transpose()?.unwrap_or(20_000);
    let config_path: PathBuf = flags
        .get("config")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/wwe14.toml"));
    let [xex_path, out] = positional.as_slice() else { return Err(USAGE.into()) };
    let out = Path::new(out);

    let t0 = Instant::now();
    let xex = Xex::parse(&std::fs::read(xex_path)?)?;
    let a = analyse::analyse(&xex)?;
    let cfg: Config = toml::from_str(
        &std::fs::read_to_string(&config_path).map_err(|e| format!("{}: {e}", config_path.display()))?,
    )
    .map_err(|e| format!("{}: {e}", config_path.display()))?;
    check_config(&cfg, &xex, &a)?;
    let unclassified = a.unclassified_bctr().count();
    if unclassified > 0 || !a.discovery.problems.is_empty() {
        return Err(format!(
            "refusing to recompile: {unclassified} unclassified indirect branches, {} analysis problems \
             (run `analyse report`)",
            a.discovery.problems.len()
        )
        .into());
    }
    let code = Code::new(&xex);

    // Functions to emit, keyed by entry address. ABI helpers become one function
    // per entry point (each runs from its entry to the helper's end).
    let mut defs: BTreeMap<u32, FnDef> = BTreeMap::new();
    let mut imports: Vec<(u32, String, String, u16)> = Vec::new();
    let ordinals: HashMap<u32, u16> =
        xex.imports.iter().flat_map(|l| l.functions().map(|r| (r.address, r.ordinal))).collect();
    for f in &a.discovery.functions {
        match f.source {
            Source::ImportThunk => {
                let (lib, name) = a.import_thunks.get(&f.begin).cloned().unwrap_or_default();
                imports.push((f.begin, lib, name, ordinals.get(&f.begin).copied().unwrap_or(0)));
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
    let mut entries: BTreeSet<u32> = defs.keys().copied().collect();
    entries.extend(imports.iter().map(|i| i.0));
    let switches: HashMap<u32, (u32, Vec<u32>, Option<u32>)> =
        a.switch_tables().map(|t| (t.bctr, (t.reg, t.labels.clone(), t.default))).collect();
    let word = |addr: u32| code.word(addr);
    let setjmp: BTreeSet<u32> = cfg.nonlocal.setjmp.iter().copied().collect();
    let longjmp_returns: BTreeSet<u32> = cfg.nonlocal.longjmp_returns.iter().copied().collect();
    // The register save/restore helpers are translated into their callers.
    let inline: BTreeMap<u32, u32> =
        a.helpers.iter().flat_map(|h| h.entries().map(|(_, entry)| (entry, h.end())).collect::<Vec<_>>()).collect();
    let program = Program {
        entries: &entries,
        switches: &switches,
        word: &word,
        setjmp: &setjmp,
        longjmp_returns: &longjmp_returns,
        inline: &inline,
    };

    // Emit every function. A replaced function's body becomes `__imp__sub_X`,
    // and `sub_X` becomes a stub that calls the native implementation.
    let mut replaced: Vec<&Replace> = cfg.replace.iter().collect();
    replaced.sort_by_key(|r| r.address);
    let replace_index: HashMap<u32, usize> = replaced.iter().enumerate().map(|(i, r)| (r.address, i)).collect();
    let unreachable: HashMap<u32, &str> = cfg.unreachable.iter().map(|u| (u.address, u.note.as_str())).collect();
    // `--trace-calls on` (diagnostic builds only): every function marks itself
    // called in the kernel's per-frame table (`kernel::trace`), by its index in
    // address order (`trace_index.txt` maps the indices back).
    let trace_calls = flags.get("trace-calls").is_some_and(|v| *v == "on");
    if trace_calls {
        write_if_changed(out.join("trace_index.txt"), defs.keys().enumerate().map(|(i, a)| format!("{i} {a:08X}\n")).collect::<String>())?;
    }
    // What the register analysis needs to know about the program around the
    // functions. Native code runs behind the import thunks and the replaced
    // functions, and may call a replaced function's original or anything else
    // whose address the program takes.
    let native: BTreeSet<u32> =
        imports.iter().map(|i| i.0).chain(replaced.iter().map(|r| r.address)).chain(unreachable.keys().copied()).collect();
    let mut address_taken = analyse::gpu::address_taken(&xex, &a);
    address_taken.extend(xex.entry_point());
    address_taken.extend(replaced.iter().map(|r| r.address));
    address_taken.extend(cfg.locals.indirect.iter().copied());
    address_taken.retain(|t| !unreachable.contains_key(t));
    let observed = BTreeSet::new();
    let untrusted: BTreeSet<u32> = cfg.locals.untrusted.iter().copied().collect();
    // Imports that never return to their caller: the kernel stops the game
    // or the thread, or unwinds past the caller.
    const NORETURN: [&str; 6] =
        ["KeBugCheck", "KeBugCheckEx", "RtlUnwind", "ExTerminateThread", "HalReturnToFirmware", "XamLoaderTerminateTitle"];
    let noreturn: BTreeSet<u32> = imports.iter().filter(|i| NORETURN.contains(&i.2.as_str())).map(|i| i.0).collect();
    let options = locals::Options {
        // `--locals on|off` overrides the config (for building both kinds side by side).
        enabled: match flags.get("locals") {
            Some(&"on") => true,
            Some(&"off") => false,
            Some(other) => return Err(format!("--locals takes on or off, not {other:?}").into()),
            None => cfg.locals.enabled,
        },
        address_taken: &address_taken,
        observed: &observed,
        native: &native,
        untrusted: &untrusted,
        noreturn: &noreturn,
    };
    let registers_in_locals = options.enabled;
    let unit = emit_program(&program, &defs.values().collect::<Vec<_>>(), &options);
    print!("{}", unit.report);
    write_if_changed(out.join("locals_report.txt"), &unit.report)?;
    write_if_changed(out.join("locals_functions.txt"), &unit.details)?;
    if !unit.errors.is_empty() {
        return Err(format!("the register analysis can't translate this soundly:\n  {}", unit.errors.join("\n  ")).into());
    }
    let not_indirect = unit.not_indirect;
    // Native code calls a replaced function's original directly, not through
    // the table, so the table's refusal would not stop it.
    if let Some(r) = replaced.iter().find(|r| not_indirect.contains_key(&r.address)) {
        return Err(format!(
            "the original of replaced function {} (0x{:08X}) can't be called from native code: {}",
            r.name, r.address, not_indirect[&r.address]
        )
        .into());
    }
    let emitted: Vec<(u32, emit::Emitted)> = defs
        .values()
        .zip(unit.emitted)
        .enumerate()
        .map(|(index, (f, mut e))| {
            if let Some(note) = unreachable.get(&f.begin) {
                // Hardware-facing and never run by the port: its body is a fatal
                // error naming it (the original is not emitted at all).
                let name = fn_name(f.begin);
                e.source = format!(
                    "/// Unreachable (config `[[unreachable]]`): {note}\n#[unsafe(no_mangle)]\npub unsafe extern \"C-unwind\" fn {name}(_c: &mut Ctx, _b: *mut u8) {{\n    \
                     panic!(\"FATAL: {name} ({note}) is hardware-facing and has no native version; the port never expected to run it \
                     (config/wwe14.toml [[unreachable]]). Give it one.\");\n}}\n\n"
                );
                e.callees.clear();
                return (f.begin, e);
            }
            if trace_calls {
                let name = fn_name(f.begin);
                let def = format!("pub unsafe extern \"C-unwind\" fn {name}(c: &mut Ctx, b: *mut u8) {{ unsafe {{\n");
                assert!(e.source.contains(&def), "unexpected function header for {name}");
                e.source = e.source.replacen(&def, &format!("{def}    trace_hit({index});\n"), 1);
            }
            if let Some(&i) = replace_index.get(&f.begin) {
                let name = fn_name(f.begin);
                let def = format!("#[unsafe(no_mangle)]\npub unsafe extern \"C-unwind\" fn {name}(");
                assert!(e.source.contains(&def), "unexpected function header for {name}");
                e.source = e.source.replacen(&def, &format!("#[unsafe(no_mangle)]\npub unsafe extern \"C-unwind\" fn __imp__{name}("), 1);
                e.source.push_str(&format!(
                    "/// Replaced by native code: {}\n#[unsafe(no_mangle)]\npub unsafe extern \"C-unwind\" fn {name}(c: &mut Ctx, b: *mut u8) {{ unsafe {{ call_replacement(c, b, {i}) }} }}\n\n",
                    replaced[i].name
                ));
            }
            (f.begin, e)
        })
        .collect();

    // The GPU gate: hardware-facing code must sit behind replaced functions.
    let census = analyse::gpu::census(&xex, &a);
    // Replaced and unreachable functions are cut points: recompiled code
    // reaches their native version or their fatal error, never their body.
    let replaced_set: BTreeSet<u32> = replaced.iter().map(|r| r.address).chain(unreachable.keys().copied()).collect();
    let reachable = analyse::gpu::reachable_gpu(&xex, &a, &census, &replaced_set);
    let mut report = String::new();
    for (f, path) in &reachable {
        let p: Vec<String> = path.iter().map(|x| format!("{x:08X}")).collect();
        let _ = writeln!(report, "sub_{f:08X}  via {}", p.join(" -> "));
    }
    write_if_changed(out.join("gpu_reachable.txt"), &report)?;
    println!(
        "GPU gate: {} functions build GPU packets, touch MMIO or call GPU imports; {} replaced, {} unreachable; \
         {} still reachable from recompiled code (gpu_reachable.txt)",
        census.len(),
        replaced.len(),
        unreachable.len(),
        reachable.len()
    );
    if cfg.gpu.enforce && !reachable.is_empty() {
        return Err(format!(
            "[gpu] enforce: {} hardware-facing functions are reachable without a native replacement (see gpu_reachable.txt)",
            reachable.len()
        )
        .into());
    }
    let instructions: usize = emitted.iter().map(|(_, e)| e.instructions).sum();
    let todo: usize = emitted.iter().map(|(_, e)| e.todo).sum();
    let mut todo_ops: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, e) in &emitted {
        for op in &e.todo_ops {
            *todo_ops.entry(op).or_default() += 1;
        }
    }

    // Shard by instruction count, in address order.
    let mut shards: Vec<Vec<usize>> = vec![Vec::new()];
    let mut count = 0usize;
    for (n, (_, e)) in emitted.iter().enumerate() {
        if count >= per_shard {
            shards.push(Vec::new());
            count = 0;
        }
        shards.last_mut().unwrap().push(n);
        count += e.instructions.max(1);
    }

    // Write the workspace.
    let rt = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/ppc-rt"))?;
    let rt = rt.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let dep = format!("ppc-rt = {{ path = \"{rt}\" }}\n");
    std::fs::create_dir_all(out.join("shards"))?;
    let shard_names: Vec<String> = (0..shards.len()).map(|n| format!("code_{n:03}")).collect();
    write_if_changed(
        out.join("Cargo.toml"),
        format!(
            "# @generated by recomp. Do not edit.\n[workspace]\nresolver = \"3\"\nmembers = [\"shards/*\", \"imports\", \"table\", \"game\"]\n\n\
             [profile.release]\nopt-level = 2\ndebug = false\nincremental = false\npanic = \"unwind\"\n\n\
             # The native kernel keeps line tables for readable diagnostics.\n\
             [profile.release.package.kernel]\ndebug = \"line-tables-only\"\n\n\
             [profile.release.package.gpu]\ndebug = \"line-tables-only\"\n\n\
             [profile.release.package.audio]\ndebug = \"line-tables-only\"\n\n\
             # The renderer and the host too: the profiler reports their time by source line.\n\
             [profile.release.package.render]\ndebug = \"line-tables-only\"\n\n\
             [profile.release.package.host]\ndebug = \"line-tables-only\"\n\n\
             [profile.dev]\nopt-level = 1\ndebug = false\n"
        ),
    )?;
    // The PPC FPU's fused multiply-add (`fmadd`, about 6,000 sites) is
    // `f64::mul_add`: without FMA in the target, a call into the C runtime's
    // `fma` each time. x86-64-v3 (AVX2, FMA, BMI2: Haswell and Zen on) makes it
    // one instruction with the same result.
    std::fs::create_dir_all(out.join(".cargo"))?;
    write_if_changed(
        out.join(".cargo").join("config.toml"),
        "# @generated by recomp. Do not edit.\n[build]\nrustflags = [\"-C\", \"target-cpu=x86-64-v3\"]\n",
    )?;
    let header = "// @generated by recomp from default.xex. Do not edit.\n\
                  #![allow(unused, unreachable_code, non_snake_case, clippy::all)]\nuse ppc_rt::*;\n\n";
    let mut total_bytes = 0usize;
    for (n, members) in shards.iter().enumerate() {
        let dir = out.join("shards").join(&shard_names[n]);
        std::fs::create_dir_all(&dir)?;
        write_if_changed(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\n{dep}",
                shard_names[n]
            ),
        )?;
        let defined: BTreeSet<u32> = members.iter().map(|&m| emitted[m].0).collect();
        let external: BTreeSet<u32> =
            members.iter().flat_map(|&m| emitted[m].1.callees.iter().copied()).filter(|c| !defined.contains(c)).collect();
        let mut src = String::from(header);
        if trace_calls {
            src.push_str(
                "unsafe extern \"C\" {\n    static WWE14_TRACE_PARITY: core::sync::atomic::AtomicU32;\n    \
                 static WWE14_TRACE_CALLED: [core::sync::atomic::AtomicU8; 2 * 131072];\n}\n\
                 #[inline(always)]\nfn trace_hit(i: usize) {\n    unsafe {\n        \
                 let p = WWE14_TRACE_PARITY.load(core::sync::atomic::Ordering::Relaxed) as usize;\n        \
                 WWE14_TRACE_CALLED[p * 131072 + i].store(1, core::sync::atomic::Ordering::Relaxed);\n    }\n}\n\n",
            );
        }
        if !external.is_empty() {
            src.push_str("unsafe extern \"C-unwind\" {\n");
            for c in &external {
                let _ = writeln!(src, "    fn {}(c: &mut Ctx, b: *mut u8);", fn_name(*c));
            }
            src.push_str("}\n\n");
        }
        for &m in members {
            src.push_str(&emitted[m].1.source);
        }
        total_bytes += src.len();
        write_if_changed(dir.join("lib.rs"), src)?;
    }

    // Import stubs.
    let dir = out.join("imports");
    std::fs::create_dir_all(&dir)?;
    write_if_changed(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"imports\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\n{dep}"),
    )?;
    let mut src = String::from(header);
    let _ = writeln!(src, "/// Every imported function, indexed by its stub's `call_import` index.\npub static IMPORTS: [ImportDef; {}] = [", imports.len());
    for (addr, lib, name, ordinal) in &imports {
        let _ = writeln!(src, "    ImportDef {{ thunk: 0x{addr:08X}, library: {lib:?}, name: {name:?}, ordinal: 0x{ordinal:04X} }},");
    }
    src.push_str("];\n\n");
    for (n, (addr, lib, name, _)) in imports.iter().enumerate() {
        let _ = writeln!(
            src,
            "/// {lib}!{name}\n#[unsafe(no_mangle)]\npub unsafe extern \"C-unwind\" fn {}(c: &mut Ctx, b: *mut u8) {{ unsafe {{ call_import(c, b, {n}) }} }}\n",
            fn_name(*addr)
        );
    }
    write_if_changed(dir.join("lib.rs"), src)?;

    // Function table: every entry point, sorted.
    let dir = out.join("table");
    std::fs::create_dir_all(&dir)?;
    let mut deps = dep.clone();
    deps.push_str("imports = { path = \"../imports\" }\n");
    for s in &shard_names {
        let _ = writeln!(deps, "{s} = {{ path = \"../shards/{s}\" }}");
    }
    write_if_changed(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"table\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\n{deps}"),
    )?;
    let mut src = String::from(header);
    src.push_str("use imports as _;\n");
    for s in &shard_names {
        let _ = writeln!(src, "use {s} as _;");
    }
    src.push_str("\nunsafe extern \"C-unwind\" {\n");
    for e in &entries {
        let _ = writeln!(src, "    fn {}(c: &mut Ctx, b: *mut u8);", fn_name(*e));
    }
    for r in &replaced {
        let _ = writeln!(src, "    fn __imp__{}(c: &mut Ctx, b: *mut u8);", fn_name(r.address));
    }
    src.push_str("}\n\n");
    // Functions a call through a pointer must not reach (`recomp::locals`):
    // the table sends such a call to a fatal error that names them.
    for (t, why) in &not_indirect {
        let _ = writeln!(
            src,
            "unsafe extern \"C-unwind\" fn not_indirect_{t:08X}(_c: &mut Ctx, _b: *mut u8) {{ not_indirect(0x{t:08X}, {why:?}) }}"
        );
    }
    let _ = writeln!(src, "\npub static FUNCTIONS: [(u32, GuestFn); {}] = [", entries.len());
    for e in &entries {
        if not_indirect.contains_key(e) {
            let _ = writeln!(src, "    (0x{e:08X}, not_indirect_{e:08X} as GuestFn),");
        } else {
            let _ = writeln!(src, "    (0x{e:08X}, {} as GuestFn),", fn_name(*e));
        }
    }
    src.push_str("];\n");
    let _ = writeln!(
        src,
        "\n/// Functions replaced by native code (`[[replace]]`), indexed by their stubs' `call_replacement` index.\n\
         pub static REPLACEMENTS: [ReplaceDef; {}] = [",
        replaced.len()
    );
    for r in &replaced {
        let _ = writeln!(
            src,
            "    ReplaceDef {{ address: 0x{:08X}, name: {:?}, original: __imp__{} as GuestFn }},",
            r.address,
            r.name,
            fn_name(r.address)
        );
    }
    src.push_str("];\n");
    write_if_changed(dir.join("lib.rs"), src)?;

    // The game binary: the native kernel boots the recompiled code. The image
    // hash ties the binary to the exact executable it was recompiled from.
    let kernel = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/kernel"))?;
    let kernel = kernel.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let gpu = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/gpu"))?;
    let gpu = gpu.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let audio = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/audio"))?;
    let audio = audio.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let host = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/host"))?;
    let host = host.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let xex_abs = std::fs::canonicalize(xex_path)?;
    let xex_abs = xex_abs.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let dir = out.join("game");
    std::fs::create_dir_all(&dir)?;
    write_if_changed(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"game\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\nbuild = \"build.rs\"\n\n\
             [[bin]]\nname = \"WWE2K14\"\npath = \"main.rs\"\n\n\
             [dependencies]\n{dep}kernel = {{ path = \"{kernel}\" }}\ngpu = {{ path = \"{gpu}\" }}\naudio = {{ path = \"{audio}\" }}\n\
             host = {{ path = \"{host}\" }}\n\
             table = {{ path = \"../table\" }}\nimports = {{ path = \"../imports\" }}\n\n\
             [build-dependencies]\nwinresource = \"0.1.34\"\n"
        ),
    )?;
    // The executable's icon: the title's own (the 64x64 title image from the
    // xex's XDBF block, made into `private/icon/wwe2k14.ico`; the user asked,
    // 2026-11-26). Embedded when the file exists, so the build works without it.
    let icon = std::path::Path::new(xex_path).parent().and_then(|g| g.parent()).map(|p| p.join("icon").join("wwe2k14.ico"));
    let icon = icon.and_then(|p| std::fs::canonicalize(p).ok()).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string());
    write_if_changed(
        dir.join("build.rs"),
        format!(
            "// @generated by recomp. Do not edit.\n\
             fn main() {{\n    let icon: Option<&str> = {icon:?};\n    \
             if let Some(icon) = icon.filter(|p| std::path::Path::new(p).exists()) {{\n        \
             println!(\"cargo:rerun-if-changed={{icon}}\");\n        \
             let mut res = winresource::WindowsResource::new();\n        \
             res.set_icon(icon).set(\"ProductName\", \"WWE 2K14\").set(\"FileDescription\", \"WWE 2K14\");\n        \
             res.compile().expect(\"embedding the icon\");\n    }}\n}}\n"
        ),
    )?;
    write_if_changed(
        dir.join("main.rs"),
        format!(
            "// @generated by recomp. Do not edit.\n\
             // No console: the log goes to WWE14_LOG, a redirect, or logs\\last-run.log (host::portable).\n\
             #![windows_subsystem = \"windows\"]\n\
             fn main() {{\n    host::main(kernel::Program {{\n        functions: &table::FUNCTIONS,\n        \
             imports: &imports::IMPORTS,\n        replacements: &table::REPLACEMENTS,\n        \
             natives: &[gpu::resolve, audio::resolve, kernel::crt::resolve],\n        image_hash: 0x{:016X},\n        default_xex: {xex_abs:?},\n        \
             registers_in_locals: {registers_in_locals},\n    }});\n}}\n",
            kernel_image_hash(&xex.image)
        ),
    )?;
    let _ = std::fs::remove_dir_all(out.join("spike"));

    println!(
        "recompiled {} functions ({instructions} instructions, {} import stubs) into {} shards, {:.1} MiB of Rust, in {:.1}s",
        defs.len(),
        imports.len(),
        shards.len(),
        total_bytes as f64 / (1u64 << 20) as f64,
        t0.elapsed().as_secs_f64()
    );
    println!("instructions emitted as todo_instr: {todo} ({:.2}%)", 100.0 * todo as f64 / instructions.max(1) as f64);
    let mut ops: Vec<(&str, usize)> = todo_ops.into_iter().collect();
    ops.sort_by(|a, b| b.1.cmp(&a.1));
    write_if_changed(
        out.join("todo_ops.txt"),
        ops.iter().map(|(o, n)| format!("{o}	{n}
")).collect::<String>(),
    )?;
    println!(
        "most common untranslated opcodes: {}",
        ops.iter().take(30).map(|(o, n)| format!("{o}={n}")).collect::<Vec<_>>().join(" ")
    );
    Ok(())
}

/// FNV-1a over the decrypted image. The kernel computes the same hash when it
/// loads the executable and refuses to run a different one.
fn kernel_image_hash(image: &[u8]) -> u64 {
    image.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3))
}

/// Writes `contents` only if the file differs, so cargo (which goes by
/// modification time) rebuilds only the shards whose code actually changed.
fn write_if_changed(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let (path, contents) = (path.as_ref(), contents.as_ref());
    if std::fs::read(path).is_ok_and(|old| old == contents) {
        return Ok(());
    }
    std::fs::write(path, contents)
}
