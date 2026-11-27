//! xenos-shader-recomp: offline Xenos microcode → WGSL.
//!
//!   xenos-shader-recomp dis <file.bin> [vs|ps]
//!       Disassemble raw microcode (stage from a vs_/ps_ file name prefix).
//!   xenos-shader-recomp translate <in-dir> <out-dir>
//!       Translate every vs_*.bin / ps_*.bin, validate with naga, and write
//!       <name>.wgsl plus <name>.refl.txt. Exits non-zero if any shader fails.
//!   xenos-shader-recomp scan <file-or-dir> <out-dir>
//!       Extract the shader containers in a file or every file under a directory
//!       (the xex image, the .pac archives, unwrapped with tools/pac) as
//!       raw microcode named by identity (`ucode::identity`: the hash with
//!       D3D-patched vertex-fetch fields masked).
//!   xenos-shader-recomp match <shader.bin> <dir>
//!       The static shader closest to a harvested one, and the words that differ.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use xenos_shader_recomp::{Stage, containers as containers_in, translate, ucode, validate};

fn stage_of(name: &str, arg: Option<&str>) -> Option<Stage> {
    match arg {
        Some("vs") => return Some(Stage::Vertex),
        Some("ps") => return Some(Stage::Pixel),
        _ => {}
    }
    if name.starts_with("vs_") {
        Some(Stage::Vertex)
    } else if name.starts_with("ps_") {
        Some(Stage::Pixel)
    } else {
        None
    }
}

/// A file, or every file under a directory.
fn walk(p: &Path, out: &mut Vec<std::path::PathBuf>) {
    if p.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(p).expect("read dir").flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            walk(&e, out);
        }
    } else {
        out.push(p.to_path_buf());
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: xenos-shader-recomp dis <file.bin> [vs|ps] | translate <in-dir> <out-dir> | scan <file-or-dir> <out-dir> | id <file.bin>... | match <file.bin> <dir>";
    match args.get(1).map(String::as_str) {
        Some("id") if args.len() >= 3 => {
            for f in &args[2..] {
                println!("{:016x}  {f}", ucode::identity(&fs::read(f).expect("read shader")));
            }
            ExitCode::SUCCESS
        }
        Some("dis") if args.len() >= 3 => {
            let path = Path::new(&args[2]);
            let name = path.file_name().unwrap().to_string_lossy();
            let Some(stage) = stage_of(&name, args.get(3).map(String::as_str)) else {
                eprintln!("can't tell the stage of {name}: pass vs or ps");
                return ExitCode::FAILURE;
            };
            let code = fs::read(path).expect("read shader");
            print!("{}", ucode::disassemble(&code, stage == Stage::Vertex));
            ExitCode::SUCCESS
        }
        Some("translate") if args.len() == 4 => {
            let (inp, out) = (Path::new(&args[2]), Path::new(&args[3]));
            fs::create_dir_all(out).expect("create output dir");
            let mut entries: Vec<_> = fs::read_dir(inp).expect("read input dir").flatten().map(|e| e.path()).collect();
            entries.sort();
            let (mut ok, mut failed) = (0, 0);
            for path in entries {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".bin") else { continue };
                let Some(stage) = stage_of(&name, None) else { continue };
                let code = fs::read(&path).expect("read shader");
                let result = translate(&code, stage).and_then(|t| match validate(&t.wgsl) {
                    Ok(()) => Ok(t),
                    Err(e) => {
                        let _ = fs::write(out.join(format!("{stem}.invalid.wgsl")), &t.wgsl);
                        Err(format!("naga: {e}"))
                    }
                });
                match result {
                    Ok(t) => {
                        fs::write(out.join(format!("{stem}.wgsl")), &t.wgsl).expect("write wgsl");
                        fs::write(out.join(format!("{stem}.refl.txt")), format!("{:#?}\n", t.reflection)).expect("write reflection");
                        ok += 1;
                    }
                    Err(e) => {
                        eprintln!("{name}: {e}");
                        failed += 1;
                    }
                }
            }
            println!("translated {ok}, failed {failed}");
            if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some("scan") if args.len() == 4 => {
            let out = Path::new(&args[3]);
            fs::create_dir_all(out).expect("create output dir");
            let mut inputs = Vec::new();
            walk(Path::new(&args[2]), &mut inputs);
            let mut containers = 0;
            // Identity covers the microcode only: literal constants are loaded
            // into the constant file at run time, as D3D does when it sets a
            // shader, so shaders differing only in literals share a translation.
            let mut unique = std::collections::BTreeSet::new();
            for path in &inputs {
                // Game archives are unwrapped from XCompress first (tools/pac).
                let data = match pac::unwrap(fs::read(path).expect("read file")) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("{}: {e}", path.display());
                        return ExitCode::FAILURE;
                    }
                };
                // Shader containers, also inside BPE-compressed archive entries.
                let mut buffers = vec![data];
                let (mut found, mut new) = (0, 0);
                while let Some(buf) = buffers.pop() {
                    for c in containers_in(&buf) {
                        let code = &buf[c.code.clone()];
                        let prefix = if c.stage == Stage::Vertex { "vs" } else { "ps" };
                        let stem = format!("{prefix}_{:016x}", ucode::identity(code));
                        if unique.insert(stem.clone()) {
                            fs::write(out.join(format!("{stem}.bin")), code).expect("write shader");
                            new += 1;
                        }
                        found += 1;
                    }
                    buffers.extend(pac::find_bpe(&buf).into_iter().map(|(_, d)| d));
                }
                containers += found;
                if found > 0 {
                    println!("{}: {found} containers, {new} new", path.display());
                }
            }
            println!("{} files, {containers} containers, {} unique shaders", inputs.len(), unique.len());
            ExitCode::SUCCESS
        }
        Some("match") if args.len() == 4 => {
            // Which static shader a harvested (D3D-patched) one came from, and
            // which bits differ: the fields D3D's vertex-fetch patcher writes.
            let code = fs::read(&args[2]).expect("read shader");
            let mut best: Option<(usize, std::path::PathBuf, Vec<u8>)> = None;
            for e in fs::read_dir(&args[3]).expect("read dir").flatten() {
                let other = fs::read(e.path()).expect("read");
                if other.len() != code.len() {
                    continue;
                }
                let diff = code.chunks(4).zip(other.chunks(4)).filter(|(a, b)| a != b).count();
                if best.as_ref().is_none_or(|b| diff < b.0) {
                    best = Some((diff, e.path(), other));
                }
            }
            let Some((diff, path, other)) = best else {
                println!("no static shader of {} bytes", code.len());
                return ExitCode::FAILURE;
            };
            println!("closest: {} ({diff} words differ)", path.display());
            let w = |d: &[u8], i: usize| u32::from_be_bytes(d[4 * i..4 * i + 4].try_into().unwrap());
            for i in 0..code.len() / 4 {
                let (a, b) = (w(&code, i), w(&other, i));
                if a != b {
                    println!("  slot {:3} word {}: harvested {a:08X} static {b:08X} xor {:08X}", i / 3, i % 3, a ^ b);
                }
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{usage}");
            ExitCode::FAILURE
        }
    }
}
