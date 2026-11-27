//! `stfs info <package>` and `stfs extract <package> <out dir>`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let run = || -> Result<(), Box<dyn std::error::Error>> {
        match args.get(1).map(String::as_str) {
            Some(cmd @ ("info" | "extract")) if args.len() >= 3 => {
                let data = std::fs::read(&args[2])?;
                let p = stfs::Package::parse(&data)?;
                println!("{} package, title {:08X}, content type {:08X}: {}", p.magic.trim(), p.title_id, p.content_type, p.display_name);
                for i in 0..p.entries.len() {
                    let e = &p.entries[i];
                    let path = p.path(i);
                    if e.directory {
                        println!("  {path}/");
                        if cmd == "extract" {
                            std::fs::create_dir_all(std::path::Path::new(args.get(3).ok_or("extract needs an output directory")?).join(&path))?;
                        }
                        continue;
                    }
                    println!("  {path} ({} bytes{})", e.size, if e.consecutive { "" } else { ", chained blocks" });
                    if cmd == "extract" {
                        let out = std::path::Path::new(args.get(3).ok_or("extract needs an output directory")?).join(&path);
                        if let Some(dir) = out.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        std::fs::write(&out, p.read(i)?)?;
                    }
                }
                Ok(())
            }
            _ => Err("usage: stfs info <package> | stfs extract <package> <out dir>".into()),
        }
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stfs: {e}");
            ExitCode::FAILURE
        }
    }
}
