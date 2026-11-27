use std::error::Error;
use std::process::ExitCode;

use xex::{Compression, Xex, key};

const USAGE: &str = "\
usage:
  xex info       <file.xex>
  xex imports    <file.xex>
  xex sections   <file.xex>
  xex dump-image <file.xex> <out.bin>
  xex patch-info <file.xexp>
  xex apply-patch <base.xex> <update.xexp> <out.xex>
  xex verify-pages <file.xex>
  xex diff       <a.xex> <b.xex>";

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
    match args {
        [cmd, path] if cmd == "info" => info(&load(path)?),
        [cmd, path] if cmd == "imports" => imports(&load(path)?),
        [cmd, path] if cmd == "sections" => sections(&load(path)?),
        [cmd, path, out] if cmd == "dump-image" => {
            let x = load(path)?;
            std::fs::write(out, &x.image).map_err(|e| format!("{out}: {e}"))?;
            println!("wrote {} bytes (image at 0x{:08X}) to {out}", x.image.len(), x.base_address);
            Ok(())
        }
        [cmd, path] if cmd == "patch-info" => patch_info(path),
        [cmd, base, patch, out] if cmd == "apply-patch" => apply_patch(base, patch, out),
        [cmd, path] if cmd == "verify-pages" => {
            let (ok, total) = load(path)?.verify_pages();
            println!("page hash chain: {ok} of {total} links match");
            if ok == total { Ok(()) } else { Err("image does not match its signed page hashes".into()) }
        }
        [cmd, a, b] if cmd == "diff" => diff(&load(a)?, &load(b)?),
        _ => Err(USAGE.into()),
    }
}

fn load(path: &str) -> Result<Xex, Box<dyn Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(Xex::parse(&bytes).map_err(|e| format!("{path}: {e}"))?)
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 20) as f64
}

fn info(x: &Xex) -> Result<(), Box<dyn Error>> {
    let h = &x.header;
    if let Some(name) = h.original_pe_name() {
        println!("original PE name  : {name}");
    }
    if let Some(e) = h.execution_info()? {
        println!("title ID          : {:08X}", e.title_id);
        println!("media ID          : {:08X}", e.media_id);
        println!("version           : {} (base {})", e.version, e.base_version);
        println!("disc              : {}/{}", e.disc_number, e.disc_count);
    }
    let compression = match &x.file_format.compression {
        Compression::None => "none".to_string(),
        Compression::Basic { blocks } => format!("basic ({} blocks)", blocks.len()),
        Compression::Lzx { window_size, .. } => format!("LZX (window 0x{window_size:X})"),
        Compression::Delta => "delta".to_string(),
    };
    println!("encryption        : {:?} (decrypted with {:?} key)", x.file_format.encryption, x.key);
    println!("compression       : {compression}");
    println!(
        "image             : 0x{:08X}..0x{:08X} ({:.2} MiB)",
        x.base_address,
        x.base_address as u64 + x.image.len() as u64,
        mib(x.image.len() as u64)
    );
    if let Some(ep) = x.entry_point() {
        println!("entry point       : 0x{ep:08X}");
    }
    for (label, value) in [
        ("default stack     ", h.default_stack_size()),
        ("default heap      ", h.default_heap_size()),
        ("title workspace   ", h.title_workspace_size()),
        ("system flags      ", h.system_flags()),
    ] {
        if let Some(v) = value {
            println!("{label}: 0x{v:X}");
        }
    }
    if let Some(tls) = h.tls_info()? {
        println!(
            "TLS               : {} slots, template at 0x{:08X} ({} bytes data, {} raw)",
            tls.slot_count, tls.raw_data_address, tls.data_size, tls.raw_data_size
        );
    }

    let code: u64 = x.code_sections().map(|s| s.virtual_size as u64).sum();
    println!(
        "sections          : {} ({} code sections, {:.2} MiB of code, ~{} instructions)",
        x.sections.len(),
        x.code_sections().count(),
        mib(code),
        thousands(code / 4)
    );

    let pdata = x.pdata()?;
    let covered: u64 = pdata.iter().map(|f| f.length as u64).sum();
    let with_eh = pdata.iter().filter(|f| f.has_exception_handler).count();
    println!(
        ".pdata functions  : {} covering {:.2} MiB, {with_eh} with exception handlers",
        thousands(pdata.len() as u64),
        mib(covered)
    );
    if let Some(largest) = pdata.iter().max_by_key(|f| f.length) {
        println!("largest function  : 0x{:X} bytes at 0x{:08X}", largest.length, largest.begin);
    }

    println!("imports:");
    for lib in &x.imports {
        let unnamed = lib.records.iter().filter(|r| r.name.is_none()).count();
        println!(
            "  {:<14} {} (min {}): {} functions, {} variables{}",
            lib.name,
            lib.version,
            lib.min_version,
            lib.functions().count(),
            lib.variables().len(),
            if unnamed > 0 { format!(", {unnamed} records with unknown ordinals") } else { String::new() }
        );
    }

    let libs = h.static_libraries()?;
    if !libs.is_empty() {
        println!("static libraries:");
        for l in libs {
            println!("  {:<9} {}.{}.{} (qfe/flags 0x{:04X})", l.name, l.major, l.minor, l.build, l.qfe_and_flags);
        }
    }
    let resources = h.resources()?;
    if !resources.is_empty() {
        println!("resources:");
        for r in resources {
            println!("  {:<9} 0x{:08X} ({} bytes)", r.name, r.address, r.size);
        }
    }
    println!("page regions ({} KiB pages):", x.security.page_size() / 1024);
    for r in x.security.regions() {
        println!("  {:<10} 0x{:08X}..0x{:08X} ({:.2} MiB)", r.kind.to_string(), r.start, r.end, mib((r.end - r.start) as u64));
    }
    let unknown: Vec<String> = h
        .optional
        .iter()
        .filter(|o| key::name(o.key).is_none())
        .map(|o| format!("0x{:08X}", o.key))
        .collect();
    if !unknown.is_empty() {
        println!("unrecognized optional headers: {}", unknown.join(", "));
    }
    Ok(())
}

fn imports(x: &Xex) -> Result<(), Box<dyn Error>> {
    for lib in &x.imports {
        let mut functions: Vec<_> = lib.functions().collect();
        functions.sort_by_key(|r| r.ordinal);
        println!("{} {}: {} functions", lib.name, lib.version, functions.len());
        for r in functions {
            println!("  fn   0x{:04X} {:<48} thunk 0x{:08X}", r.ordinal, r.display_name(), r.address);
        }
        let mut variables = lib.variables();
        variables.sort_by_key(|r| r.ordinal);
        println!("{} {}: {} variables", lib.name, lib.version, variables.len());
        for r in variables {
            println!("  var  0x{:04X} {:<48} slot  0x{:08X}", r.ordinal, r.display_name(), r.address);
        }
    }
    Ok(())
}

fn sections(x: &Xex) -> Result<(), Box<dyn Error>> {
    println!("{:<9} {:<23} {:>10} {:>10}  flags", "name", "range", "virt size", "raw size");
    for s in &x.sections {
        println!(
            "{:<9} 0x{:08X}..0x{:08X} {:>10} {:>10}  0x{:08X}{}",
            s.name,
            s.address,
            s.end(),
            s.virtual_size,
            s.raw_size,
            s.characteristics,
            if s.is_code() { " code" } else if s.is_writable() { " writable" } else { "" }
        );
    }
    Ok(())
}

fn patch_info(path: &str) -> Result<(), Box<dyn Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let p = xex::DeltaPatch::parse(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let d = &p.descriptor;
    if let Some(e) = p.header.execution_info()? {
        println!("title ID          : {:08X}", e.title_id);
        println!("version           : {} (base {})", e.version, e.base_version);
    }
    println!("patch             : {} -> {}", d.source_version, d.target_version);
    println!("LZX window        : 0x{:X}", p.window_size);
    println!("target image size : 0x{:X}", p.security.image_size);
    println!("target header size: 0x{:X}", d.size_of_target_headers);
    println!(
        "header copy       : 0x{:X} bytes from 0x{:X} to 0x{:X}",
        d.headers_source_size, d.headers_source_offset, d.headers_target_offset
    );
    println!(
        "image copy        : 0x{:X} bytes from 0x{:X} to 0x{:X}",
        d.image_source_size, d.image_source_offset, d.image_target_offset
    );
    println!("base image key    : {:02X?}", d.source_image_key);
    println!("header patch list : {} bytes", d.header_patches.len());
    Ok(())
}

fn apply_patch(base: &str, patch: &str, out: &str) -> Result<(), Box<dyn Error>> {
    let b = std::fs::read(base).map_err(|e| format!("{base}: {e}"))?;
    let p = std::fs::read(patch).map_err(|e| format!("{patch}: {e}"))?;
    let (xex_bytes, s) = xex::apply_to_file(&b, &p)?;
    let x = Xex::parse(&xex_bytes).map_err(|e| format!("patched result does not load: {e}"))?;
    std::fs::write(out, &xex_bytes).map_err(|e| format!("{out}: {e}"))?;
    println!(
        "applied {} header records; {} image blocks, {} image records: {} LZX ({} -> {} bytes), {} copies, {} zero fills",
        s.header_records, s.image_blocks, s.image_records, s.lzx_records, s.lzx_input_bytes, s.lzx_output_bytes,
        s.copy_records, s.zero_records
    );
    println!(
        "LZX streams ended with {} unused input bytes; page hash chain: {} of {} links match",
        s.lzx_unused_bytes, s.pages_verified.0, s.pages_verified.1
    );
    if let Some(e) = x.header.execution_info()? {
        println!("result version {} (base {}); image 0x{:X} bytes", e.version, e.base_version, x.image.len());
    }
    println!("wrote {} bytes to {out}", xex_bytes.len());
    Ok(())
}

fn diff(a: &Xex, b: &Xex) -> Result<(), Box<dyn Error>> {
    const PAGE: usize = 0x1000;
    let n = a.image.len().max(b.image.len());
    let at = |img: &[u8], i: usize| img.get(i).copied();
    let mut pages = 0usize;
    let mut bytes = 0usize;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for page in (0..n).step_by(PAGE) {
        let mut changed = false;
        for i in page..(page + PAGE).min(n) {
            if at(&a.image, i) != at(&b.image, i) {
                changed = true;
                bytes += 1;
                match ranges.last_mut() {
                    // Merge differences closer than 64 bytes into one range.
                    Some(r) if i - r.1 <= 64 => r.1 = i + 1,
                    _ => ranges.push((i, i + 1)),
                }
            }
        }
        pages += changed as usize;
    }
    println!(
        "images: 0x{:X} vs 0x{:X} bytes; {pages} of {} 4 KiB pages differ, {bytes} bytes differ, {} ranges",
        a.image.len(),
        b.image.len(),
        n.div_ceil(PAGE),
        ranges.len()
    );
    let section_of = |addr: u32| {
        b.sections.iter().find(|s| s.contains(addr)).map(|s| s.name.clone()).unwrap_or_else(|| "(headers/gap)".into())
    };
    // Per-section totals.
    let mut per: Vec<(String, usize, usize)> = Vec::new();
    for &(s, e) in &ranges {
        let name = section_of(b.base_address + s as u32);
        let diff_bytes = (s..e).filter(|&i| at(&a.image, i) != at(&b.image, i)).count();
        match per.iter_mut().find(|p| p.0 == name) {
            Some(p) => {
                p.1 += 1;
                p.2 += diff_bytes;
            }
            None => per.push((name, 1, diff_bytes)),
        }
    }
    println!("by section (ranges, differing bytes):");
    for (name, r, d) in &per {
        println!("  {name:<14} {r:>7} {d:>10}");
    }
    let sa: Vec<_> = a.sections.iter().map(|s| (s.name.as_str(), s.address, s.virtual_size)).collect();
    let sb: Vec<_> = b.sections.iter().map(|s| (s.name.as_str(), s.address, s.virtual_size)).collect();
    if sa == sb {
        println!("section tables identical");
    } else {
        println!("section tables differ:");
        for s in &b.sections {
            let old = a.sections.iter().find(|o| o.name == s.name);
            println!(
                "  {:<9} 0x{:08X}+0x{:X}  (was {})",
                s.name,
                s.address,
                s.virtual_size,
                old.map_or("absent".into(), |o| format!("0x{:08X}+0x{:X}", o.address, o.virtual_size))
            );
        }
    }
    // Function table comparison: how far do functions move?
    let (pa, pb) = (a.pdata()?, b.pdata()?);
    let same = pa.iter().zip(&pb).take_while(|(x, y)| x.begin == y.begin && x.length == y.length).count();
    let set_a: std::collections::HashSet<(u32, u32)> = pa.iter().map(|f| (f.begin, f.length)).collect();
    let unchanged = pb.iter().filter(|f| set_a.contains(&(f.begin, f.length))).count();
    println!(
        ".pdata: {} -> {} functions; first {same} identical (diverging at 0x{:08X}); {unchanged} keep address and length",
        pa.len(),
        pb.len(),
        pb.get(same).map_or(0, |f| f.begin)
    );
    // Shift histogram: pair functions in order after the divergence by length.
    let mut shifts: Vec<(i64, usize, u32, u32)> = Vec::new(); // (delta, count, first addr, last addr)
    let (mut i, mut j) = (same, same);
    while i < pa.len() && j < pb.len() {
        if pa[i].length == pb[j].length {
            let delta = pb[j].begin as i64 - pa[i].begin as i64;
            match shifts.last_mut() {
                Some(s) if s.0 == delta => {
                    s.1 += 1;
                    s.3 = pa[i].begin;
                }
                _ => shifts.push((delta, 1, pa[i].begin, pa[i].begin)),
            }
            i += 1;
            j += 1;
        } else if pa.get(i + 1).is_some_and(|f| f.length == pb[j].length) {
            i += 1;
        } else {
            j += 1;
        }
    }
    let big: Vec<_> = shifts.iter().filter(|s| s.1 >= 50).collect();
    println!("function shift runs (old address range, +delta, functions) with >= 50 functions:");
    for s in big {
        let sign = if s.0 < 0 { "-" } else { "+" };
        println!("  0x{:08X}..0x{:08X}  {sign}0x{:X}  {}", s.2, s.3, s.0.unsigned_abs(), s.1);
    }
    println!("ranges (guest addresses):");
    for &(s, e) in &ranges {
        let addr = b.base_address + s as u32;
        println!("  0x{:08X}..0x{:08X} {:>8} bytes  {}", addr, b.base_address + e as u32, e - s, section_of(addr));
    }
    Ok(())
}

