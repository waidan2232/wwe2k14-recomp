//! pac: inspect and unpack the game's .pac archives.
//!
//!   pac info <file>...        format, XCompress header, decompressed size and inner format
//!   pac unpack <file> <out>   write the file with its XCompress wrapper removed
//!   pac census <dir>          inner formats of every file under a directory

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pac::{Kind, kind, unwrap, xcompress_header};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn magic(d: &[u8]) -> String {
    d.iter().take(4).map(|&b| if b.is_ascii_graphic() { b as char } else { '.' }).collect::<String>()
        + &format!(" ({})", d.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("info") if args.len() >= 3 => {
            let mut ok = true;
            for f in &args[2..] {
                let data = std::fs::read(f).expect("read");
                print!("{f}: {} bytes, {:?}", data.len(), kind(&data));
                if kind(&data) == Kind::XCompress {
                    let h = xcompress_header(&data).unwrap();
                    print!(", window {:#x}, flags {:#x}, table {:?}", h.window, h.flags, h.table);
                }
                match unwrap(data) {
                    Ok(d) => println!(" -> {} bytes, inner {:?} {}", d.len(), kind(&d), magic(&d)),
                    Err(e) => {
                        println!(" -> error: {e}");
                        ok = false;
                    }
                }
            }
            if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some("unpack") if args.len() == 4 => match unwrap(std::fs::read(&args[2]).expect("read")) {
            Ok(d) => {
                std::fs::write(&args[3], d).expect("write");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        Some("census") if args.len() == 3 => {
            let mut all = Vec::new();
            files(Path::new(&args[2]), &mut all);
            let mut inner: BTreeMap<String, (usize, u64, u64)> = BTreeMap::new();
            let mut failed = 0;
            for p in &all {
                let data = std::fs::read(p).expect("read");
                let (outer, packed) = (kind(&data), data.len() as u64);
                match unwrap(data) {
                    Ok(d) => {
                        let key = format!("{outer:?} -> {}", magic(&d));
                        let e = inner.entry(key).or_default();
                        e.0 += 1;
                        e.1 += packed;
                        e.2 += d.len() as u64;
                    }
                    Err(e) => {
                        eprintln!("{}: {e}", p.display());
                        failed += 1;
                    }
                }
            }
            for (k, (n, packed, plain)) in &inner {
                println!("{n:5}  {k:40} {:8.1} MB packed {:8.1} MB plain", *packed as f64 / 1e6, *plain as f64 / 1e6);
            }
            println!("{} files, {failed} failed", all.len());
            if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some("bpe") if args.len() == 3 => {
            let d = unwrap(std::fs::read(&args[2]).expect("read")).expect("unwrap");
            let (mut ok, mut bad) = (0, 0);
            let mut o = 0;
            while let Some(p) = d[o..].windows(4).position(|w| w == b"BPE ") {
                let at = o + p;
                match pac::bpe_expand(&d[at..]) {
                    Ok(x) => {
                        ok += 1;
                        if ok <= 5 {
                            println!("{at:#x}: {} bytes, starts {}", x.len(), magic(&x));
                        }
                    }
                    Err(e) => {
                        bad += 1;
                        if bad <= 5 {
                            println!("{at:#x}: {e}");
                        }
                    }
                }
                o = at + 4;
            }
            println!("{ok} expanded, {bad} failed");
            ExitCode::SUCCESS
        }
        Some("grep") if args.len() == 4 => {
            let hex = args[3].replace(' ', "");
            let pat: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex pattern")).collect();
            let mut all = Vec::new();
            files(Path::new(&args[2]), &mut all);
            let mut hits = 0;
            for p in &all {
                let Ok(d) = unwrap(std::fs::read(p).expect("read")) else { continue };
                for (i, w) in d.windows(pat.len()).enumerate() {
                    if w == &pat[..] {
                        println!("{}: {i:#x}", p.display());
                        hits += 1;
                    }
                }
            }
            println!("{hits} hits");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: pac info <file>... | unpack <file> <out> | census <dir> | grep <dir> <hex>");
            ExitCode::FAILURE
        }
    }
}
