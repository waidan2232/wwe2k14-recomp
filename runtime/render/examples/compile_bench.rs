//! How long the driver takes to compile the game's pipelines, with its cache
//! bypassed (Phase 4: the first compile of the largest pipelines).
//!
//! `cargo run --release -p render --example compile_bench -- [--top N] [--unchecked] [--nonce N] [--edit NAME]`
//!
//! Takes pipelines from `private/pipeline-keys.txt` (the warm-up list), the
//! ones with the largest shaders first, and compiles each one on this thread.
//! Run from the project root. Nothing is drawn.

use std::path::PathBuf;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// Translation changes to try on the real shaders, by name (`+` joins several).
/// Some only measure a cost and change what the shader computes.
fn edit(name: &str, src: &str, vertex: bool) -> String {
    name.split('+').fold(src.to_string(), |s, n| edit_one(n, &s, vertex))
}

fn edit_one(name: &str, src: &str, _vertex: bool) -> String {
    match name {
        "none" => src.to_string(),
        // Registers as 64 locals instead of an array.
        "regs" => {
            let decl = "var r: array<vec4<f32>, 64>;";
            if !src.contains(decl) {
                return src.to_string();
            }
            let b = src.as_bytes();
            let mut out = String::with_capacity(src.len());
            let mut i = 0;
            while i < b.len() {
                let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
                if b[i] == b'r' && b.get(i + 1) == Some(&b'[') && (i == 0 || !ident(b[i - 1])) {
                    let digits = b[i + 2..].iter().take_while(|c| c.is_ascii_digit()).count();
                    if digits > 0 && b.get(i + 2 + digits) == Some(&b']') {
                        out.push('r');
                        out.push_str(&src[i + 2..i + 2 + digits]);
                        i += 3 + digits;
                        continue;
                    }
                    panic!("compile_bench: a register indexed at run time");
                }
                out.push(b[i] as char);
                i += 1;
            }
            let vars: String = (0..64).map(|n| format!("var r{n}: vec4<f32>; ")).collect();
            out.replace(decl, &vars)
        }
        // Constants from one load (wrong data: measures the table lookup).
        "kc1" => src.replace("U[U[dr.pk + j / 64u][(j / 16u) % 4u] + j % 16u]", "U[dr.pk + j]").replace("U[U[dr.vk + j / 64u][(j / 16u) % 4u] + j % 16u]", "U[dr.vk + j]"),
        // Plain multiplies (wrong for 0 times infinity: measures the D3D9 multiply).
        "mul" => src
            .replace("return select(a * b, vec4<f32>(0.0), (a == vec4<f32>(0.0)) | (b == vec4<f32>(0.0)));", "return a * b;")
            .replace("return select(a * b, 0.0, a == 0.0 || b == 0.0);", "return a * b;"),
        // No predication (wrong results: measures the branches).
        "nopred" => src.replace("if (p0 == true) {", "if (true) {").replace("if (p0 == false) {", "if (true) {"),
        // No texture samples (wrong results: measures them).
        "notex" => {
            let mut out = String::new();
            for l in src.lines() {
                if let Some(i) = l.find("let tv = textureSample") {
                    out.push_str(&l[..i]);
                    out.push_str("let tv = vec4<f32>(uv.x, uv.y, 0.0, 1.0);\n");
                } else {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            out
        }
        // No exp2/log2 (wrong results: measures them).
        "noexp" => src.replace("exp2(sa)", "(sa)").replace("max(log2(sa), -FLT_MAX)", "(sa)").replace("log2(sa)", "(sa)"),
        // A pixel shader's first `cutN` percent of exec blocks, then every
        // register to the output (wrong results: finds where the cost is).
        n if n.starts_with("cut") && !_vertex => {
            let pct: usize = n[3..].parse().expect("cutN");
            let starts: Vec<usize> = src.lines().enumerate().filter(|(_, l)| l.starts_with("    if (") && !l.contains("at_pass")).map(|(i, _)| i).collect();
            let Some(&at) = starts.get(starts.len() * pct / 100) else { return src.to_string() };
            let sum: Vec<String> = (0..64).map(|r| format!("r[{r}]")).collect();
            let mut out = String::new();
            for (i, l) in src.lines().enumerate() {
                if i == at {
                    out.push_str(&format!("    {{ var bo: PsOut; bo.c0 = {}; return bo; }}\n", sum.join(" + ")));
                }
                out.push_str(l);
                out.push('\n');
            }
            out
        }
        n if n.starts_with("cut") => src.to_string(),
        // Vertex fetches decode only 32-bit floats (wrong results: measures the generic decoder).
        "fetchf" => src.replace(
            "fn vfetch_decode(addr: u32, endian: u32, f: u32) -> vec4<f32> {",
            "fn vfetch_decode(addr: u32, endian: u32, f: u32) -> vec4<f32> { return vec4<f32>(bitcast<f32>(vword(addr, endian)), bitcast<f32>(vword(addr + 1u, endian)), bitcast<f32>(vword(addr + 2u, endian)), 1.0); }\nfn vfetch_decode_old(addr: u32, endian: u32, f: u32) -> vec4<f32> {",
        ),
        // Fetch swizzles without the switch (wrong for 4-7: measures it).
        "swz" => src.replace("fn vswz(old: f32, v: vec4<f32>, s: u32) -> f32 {", "fn vswz(old: f32, v: vec4<f32>, s: u32) -> f32 { return v[s & 3u]; }\nfn vswz_old(old: f32, v: vec4<f32>, s: u32) -> f32 {"),
        _ => panic!("compile_bench: no edit named {name}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let top: usize = opt("--top").and_then(|s| s.parse().ok()).unwrap_or(20);
    let checked = !args.iter().any(|a| a == "--unchecked");
    let nonce: u32 = opt("--nonce")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
    let edit_name = opt("--edit").unwrap_or_else(|| "none".into());
    let keys = std::fs::read_to_string(opt("--keys").unwrap_or_else(|| "private/pipeline-keys.txt".into())).expect("the pipeline list");
    let dirs: Vec<PathBuf> = opt("--dirs").unwrap_or_else(|| "private/pac-wgsl;private/xex-wgsl".into()).split(';').map(PathBuf::from).collect();

    // The largest pipelines: WGSL lines of both shaders.
    let lines_of = |stage: &str, id: &str| -> usize {
        dirs.iter().find_map(|d| std::fs::read_to_string(d.join(format!("{stage}_{id}.wgsl"))).ok()).map_or(0, |s| s.lines().count())
    };
    let mut pipes: Vec<(usize, &str)> = keys
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (vs, ps) = (w.next()?, w.next()?);
            Some((lines_of("vs", vs) + lines_of("ps", ps), l))
        })
        .collect();
    pipes.sort_by(|a, b| b.0.cmp(&a.0));
    // One pipeline per shader pair: other state barely changes the compile.
    let mut seen = std::collections::HashSet::new();
    pipes.retain(|(_, l)| seen.insert(l.split_whitespace().take(2).collect::<Vec<_>>().join(" ")));
    if let Some(only) = opt("--only") {
        pipes.retain(|(_, l)| l.contains(only.as_str()));
    }
    pipes.truncate(top);
    let chosen: Vec<&str> = pipes.iter().map(|p| p.1).collect();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() }))
        .expect("no GPU adapter");
    let required_features = adapter.features() & (wgpu::Features::FLOAT32_FILTERABLE | wgpu::Features::TEXTURE_COMPRESSION_BC);
    let (device, _queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features, required_limits: adapter.limits(), ..Default::default() }))
        .expect("no GPU device");
    eprintln!("compile_bench: {} ({:?}), {} pipelines, edit {edit_name}, bounds checks {}, nonce {nonce}", adapter.get_info().name, adapter.get_info().backend, chosen.len(), if checked { "on" } else { "off" });

    let times = render::bench_compile(&device, &dirs, &chosen, nonce, checked, &|src, vertex| edit(&edit_name, src, vertex));
    let (mut m, mut p) = (0u128, 0u128);
    for ((lines, _), t) in pipes.iter().zip(&times) {
        println!("{:>8.1} ms pipeline {:>6.1} ms modules  {lines:>5} lines  {}", t.pipeline_us as f64 / 1000.0, t.modules_us as f64 / 1000.0, t.key);
        m += t.modules_us;
        p += t.pipeline_us;
    }
    println!("total: {:.1} ms pipelines, {:.1} ms modules, {} pipelines", p as f64 / 1000.0, m as f64 / 1000.0, times.len());
}
