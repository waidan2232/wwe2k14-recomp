use std::error::Error;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use disc::{Disc, Entry, ExtractOptions};

const USAGE: &str = "\
usage:
  disc info    <image.iso>
  disc list    <image.iso>
  disc extract <image.iso> <out_dir> [--include-system-update] [--overwrite]";

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
    let Some((cmd, rest)) = args.split_first() else {
        return Err(USAGE.into());
    };
    let positional: Vec<&String> = rest.iter().filter(|a| !a.starts_with("--")).collect();
    let flag = |name: &str| rest.iter().any(|a| a == name);
    match (cmd.as_str(), positional.as_slice()) {
        ("info", [image]) => info(Path::new(image.as_str())),
        ("list", [image]) => list(Path::new(image.as_str())),
        ("extract", [image, out]) => extract(
            Path::new(image.as_str()),
            Path::new(out.as_str()),
            ExtractOptions {
                include_system_update: flag("--include-system-update"),
                overwrite: flag("--overwrite"),
            },
        ),
        _ => Err(USAGE.into()),
    }
}

fn open(image: &Path) -> Result<(Disc<std::fs::File>, Vec<Entry>), Box<dyn Error>> {
    let mut disc = Disc::open(image).map_err(|e| format!("{}: {e}", image.display()))?;
    let entries = disc.entries()?;
    Ok((disc, entries))
}

fn totals<'a>(entries: impl Iterator<Item = &'a Entry>) -> (u64, u64) {
    entries.filter(|e| !e.is_dir).fold((0, 0), |(n, b), e| (n + 1, b + e.size as u64))
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 20) as f64
}

fn info(image: &Path) -> Result<(), Box<dyn Error>> {
    let (disc, entries) = open(image)?;
    let dirs = entries.iter().filter(|e| e.is_dir).count();
    let (files, bytes) = totals(entries.iter());
    let (game_files, game_bytes) = totals(entries.iter().filter(|e| !e.is_system_update()));
    println!("image    : {} ({} bytes)", image.display(), disc.image_len());
    println!(
        "layout   : {} (game partition at 0x{:X})",
        disc.layout(),
        disc.layout().partition_offset()
    );
    println!("contents : {files} files, {dirs} directories, {:.2} GiB", gib(bytes));
    println!("game     : {game_files} files, {:.2} GiB (without $SystemUpdate)", gib(game_bytes));
    println!("top level:");
    for e in entries.iter().filter(|e| !e.path.contains('/')) {
        if e.is_dir {
            let prefix = format!("{}/", e.path);
            let (n, b) = totals(entries.iter().filter(|c| c.path.starts_with(&prefix)));
            println!("  {:<24} {n:>6} files {:>10.1} MiB", format!("{}/", e.path), mib(b));
        } else {
            println!("  {:<24} {:>23.2} MiB", e.path, mib(e.size as u64));
        }
    }
    Ok(())
}

fn list(image: &Path) -> Result<(), Box<dyn Error>> {
    let (_, entries) = open(image)?;
    for e in &entries {
        if e.is_dir {
            println!("{:>12}  {}/", "-", e.path);
        } else {
            println!("{:>12}  {}", e.size, e.path);
        }
    }
    Ok(())
}

fn extract(image: &Path, out: &Path, opts: ExtractOptions) -> Result<(), Box<dyn Error>> {
    let (mut disc, entries) = open(image)?;
    let (files, total) = totals(entries.iter().filter(|e| opts.include_system_update || !e.is_system_update()));
    eprintln!("extracting {files} files ({:.2} GiB) to {}", gib(total), out.display());
    let start = Instant::now();
    let mut done = 0u64;
    let mut next_report = 0u64;
    let report = disc.extract_all(&entries, out, opts, |e, _| {
        done += e.size as u64;
        if done >= next_report {
            eprintln!("  {:>6.2} / {:.2} GiB ({:>3.0}%)  {}", gib(done), gib(total), 100.0 * done as f64 / total.max(1) as f64, e.path);
            next_report = done + (512 << 20);
        }
    })?;
    let secs = start.elapsed().as_secs_f64();
    println!(
        "done: {} written ({:.2} GiB), {} already present, in {secs:.1}s ({:.0} MiB/s)",
        report.files_written,
        gib(report.bytes_written),
        report.files_skipped,
        mib(report.bytes_written) / secs.max(0.001)
    );

    // Check every extracted file against the disc's directory.
    let mut mismatched = Vec::new();
    for e in entries.iter().filter(|e| !e.is_dir && (opts.include_system_update || !e.is_system_update())) {
        let len = std::fs::metadata(e.host_path(out)).map(|m| m.len()).ok();
        if len != Some(e.size as u64) {
            mismatched.push(format!("{} (expected {} bytes, found {len:?})", e.path, e.size));
        }
    }
    if !mismatched.is_empty() {
        return Err(format!("{} files don't match the disc:\n  {}", mismatched.len(), mismatched.join("\n  ")).into());
    }
    println!("verified: all {files} files present with the sizes the disc records");
    Ok(())
}
