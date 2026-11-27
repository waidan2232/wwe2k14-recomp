//! Checks a translated vertex shader's vertex decoder against an earlier
//! translation's, on the GPU: both decode the same random elements (every
//! format, flags, exponent adjust, endian mode, window edges) and the results
//! must match bit for bit. So does the fetch swizzle (`vswz`).
//!
//! `cargo run --release -p render --example decoder_check -- OLD.wgsl NEW.wgsl`

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

/// The vertex shader's decoder: from its `M` declaration to `VsOut`.
fn decoder(src: &str) -> String {
    let start = "@group(0) @binding(3) var<storage, read> M: array<u32>;";
    let a = src.find(start).expect("an M declaration") + start.len();
    let b = src.find("struct VsOut").expect("VsOut");
    src[a..b].to_string()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let old = decoder(&std::fs::read_to_string(&args[0]).expect("old shader"));
    let mut new = decoder(&std::fs::read_to_string(&args[1]).expect("new shader"));
    // Variants of the new normalisation, to find which one the old results come from.
    let div = "vec4<f32>(v) / vec4<f32>((one << bits) - one)";
    match args.get(2).map(String::as_str) {
        Some("rcp") => {
            new = new.replace(div, "vec4<f32>(v) * select(select(select(select(select(vec4<f32>(1.0) / vec4<f32>((one << bits) - one), vec4<f32>(1.0 / 255.0), bits == vec4<u32>(8u)), vec4<f32>(1.0 / 1023.0), bits == vec4<u32>(10u)), vec4<f32>(1.0 / 2047.0), bits == vec4<u32>(11u)), vec4<f32>(1.0 / 65535.0), bits == vec4<u32>(16u)), vec4<f32>(1.0 / 3.0), bits == vec4<u32>(2u))")
        }
        Some("exact") => {
            new = new.replace(div, "vdiv(vec4<f32>(v), vec4<f32>((one << bits) - one))");
            new.push_str("fn vdiv(n: vec4<f32>, d: vec4<f32>) -> vec4<f32> { let q = n / d; return fma(fma(-q, d, n), 1.0 / d, q); }\n");
        }
        _ => {}
    }
    let mut old = old;
    for f in ["vfetch_decode", "vword", "vnum32", "vnum", "half2", "half", "swap32", "vswz", "vlayout", "VLayout"] {
        old = old.replace(&format!("{f}("), &format!("{f}_old(")).replace(&format!("struct {f} "), &format!("struct {f}_old "));
    }
    let shader = format!(
        "struct DrawRec {{ vk: u32, ctl: u32, fet: u32, win: u32, vf: u32, pk: u32, xf: u32, at: u32 }}
@group(0) @binding(0) var<storage, read> U: array<vec4<u32>>;
var<private> dr: DrawRec;
@group(0) @binding(3) var<storage, read> M: array<u32>;
struct Case {{ addr: u32, endian: u32, f: u32, swz: u32 }}
@group(0) @binding(4) var<storage, read> C: array<Case>;
@group(0) @binding(5) var<storage, read_write> O: array<vec4<u32>>;
{old}
{new}
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {{
    let i = id.x;
    if (i >= arrayLength(&C)) {{ return; }}
    dr = DrawRec(0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u);
    let c = C[i];
    let a = vfetch_decode_old(c.addr, c.endian, c.f);
    let b = vfetch_decode(c.addr, c.endian, c.f);
    O[4u * i] = bitcast<vec4<u32>>(a);
    O[4u * i + 1u] = bitcast<vec4<u32>>(b);
    let s = vec4<u32>(c.swz & 7u, (c.swz >> 3u) & 7u, (c.swz >> 6u) & 7u, (c.swz >> 9u) & 7u);
    O[4u * i + 2u] = bitcast<vec4<u32>>(vec4<f32>(vswz_old(9.5, b, s.x), vswz_old(9.5, b, s.y), vswz_old(9.5, b, s.z), vswz_old(9.5, b, s.w)));
    O[4u * i + 3u] = bitcast<vec4<u32>>(vec4<f32>(vswz(9.5, b, s.x), vswz(9.5, b, s.y), vswz(9.5, b, s.z), vswz(9.5, b, s.w)));
}}
"
    );

    // Two windows apart in physical memory; the rest empty.
    let (w0, n0, w1, n1) = (1000u32, 4096u32, 90000u32, 4096u32);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let m: Vec<u32> = (0..n0 + n1).map(|_| rng.next()).collect();
    let mut u = vec![[0u32; 4]; 8];
    u[0] = [w0, n0, 0, 0];
    u[1] = [w1, n1, n0, 0];
    let formats = [6u32, 7, 16, 17, 25, 26, 31, 32, 33, 34, 35, 36, 37, 38, 57, 0, 5, 63];
    let mut cases: Vec<[u32; 4]> = Vec::new();
    for &fmt in &formats {
        for _ in 0..20000 {
            let r = rng.next();
            // Mostly inside a window, some at its last words, some outside.
            let addr = match r % 10 {
                0 => w0 + n0 - 1 - rng.next() % 4,
                1 => w1 + n1 - 1 - rng.next() % 4,
                2 => rng.next() % 200_000,
                3..=6 => w0 + rng.next() % n0,
                _ => w1 + rng.next() % n1,
            };
            let flags = rng.next() & 0x7000;
            let exp = (rng.next() % 9) as i32 - 4;
            let f = (rng.next() & 0xFFF) | flags | fmt << 16 | ((exp as u32) & 0x3F) << 24;
            cases.push([addr, rng.next() % 4, f, rng.next() & 0xFFF]);
        }
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("adapter");
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_limits: adapter.limits(), ..Default::default() })).expect("device");
    use wgpu::util::DeviceExt;
    let buf = |data: &[u8], usage| device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: data, usage });
    let bytes = |v: &[u32]| v.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
    let ub = buf(&bytes(&u.concat()), wgpu::BufferUsages::STORAGE);
    let mb = buf(&bytes(&m), wgpu::BufferUsages::STORAGE);
    let cb = buf(&bytes(&cases.concat()), wgpu::BufferUsages::STORAGE);
    let out_size = (cases.len() * 64) as u64;
    let ob = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: out_size, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
    let rb = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: out_size, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("decoder check"), source: wgpu::ShaderSource::Wgsl(shader.into()) });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: None, layout: None, module: &module, entry_point: Some("main"), compilation_options: Default::default(), cache: None });
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: mb.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: cb.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: ob.as_entire_binding() },
        ],
    });
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups((cases.len() as u32).div_ceil(64), 1, 1);
    }
    enc.copy_buffer_to_buffer(&ob, 0, &rb, 0, out_size);
    queue.submit([enc.finish()]);
    rb.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let out: Vec<u32> = rb.slice(..).get_mapped_range().expect("mapped").chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();

    let same = |a: u32, b: u32| a == b || (f32::from_bits(a).is_nan() && f32::from_bits(b).is_nan());
    let (mut bad, mut bad_swz) = (std::collections::BTreeMap::<u32, (usize, String)>::new(), 0usize);
    for (i, c) in cases.iter().enumerate() {
        let o = &out[16 * i..16 * i + 16];
        if !(0..4).all(|k| same(o[k], o[4 + k])) {
            let e = bad.entry((c[2] >> 16) & 0x3F).or_insert((0, String::new()));
            e.0 += 1;
            if e.1.is_empty() {
                e.1 = format!(
                    "addr {} endian {} f {:08x}: old {:?} new {:?}",
                    c[0],
                    c[1],
                    c[2],
                    (0..4).map(|k| f32::from_bits(o[k])).collect::<Vec<_>>(),
                    (4..8).map(|k| f32::from_bits(o[k])).collect::<Vec<_>>()
                );
            }
        }
        if !(8..12).all(|k| same(o[k], o[4 + k])) {
            bad_swz += 1;
        }
    }
    // Normalised fields against the correctly rounded quotient, computed on
    // the CPU (in f64, then rounded: v / (2^n - 1) is never near an f32 tie).
    let layouts: [(u32, &[(u32, u32, u32)]); 6] = [
        (6, &[(0, 0, 8), (0, 8, 8), (0, 16, 8), (0, 24, 8)]),
        (7, &[(0, 0, 10), (0, 10, 10), (0, 20, 10), (0, 30, 2)]),
        (16, &[(0, 0, 11), (0, 11, 11), (0, 22, 10)]),
        (17, &[(0, 0, 10), (0, 10, 11), (0, 21, 11)]),
        (25, &[(0, 0, 16), (0, 16, 16)]),
        (26, &[(0, 0, 16), (0, 16, 16), (1, 0, 16), (1, 16, 16)]),
    ];
    let windows = [(w0, n0, 0u32), (w1, n1, n0)];
    let (mut checked, mut inexact) = (0usize, 0usize);
    let mut inexact_ex = String::new();
    for (i, c) in cases.iter().enumerate() {
        let (addr, endian, f) = (c[0], c[1], c[2]);
        let Some(&(_, comps)) = layouts.iter().find(|l| l.0 == (f >> 16) & 0x3F) else { continue };
        if f & 0x2000 != 0 {
            continue; // integer
        }
        let word = |k: u32| -> u32 {
            let Some(&(first, count, at)) = windows.iter().find(|w| addr.wrapping_sub(w.0) < w.1) else { return 0 };
            let i = addr - first + k;
            if i >= count {
                return 0;
            }
            let raw = m[(at + i) as usize];
            let raw = if endian == 1 || endian == 2 { ((raw & 0x00FF_00FF) << 8) | ((raw >> 8) & 0x00FF_00FF) } else { raw };
            if endian >= 2 { raw.rotate_left(16) } else { raw }
        };
        let scale = 2f64.powi(((f << 2) as i32) >> 26);
        for (k, &(w, first, bits)) in comps.iter().enumerate() {
            let v = (word(w) >> first) & ((1u32 << bits) - 1);
            let s = ((v << (32 - bits)) as i32 >> (32 - bits)) as f64;
            let full = ((1u64 << bits) - 1) as f64;
            let q = if f & 0x1000 == 0 {
                v as f64 / full
            } else if f & 0x4000 != 0 {
                (2.0 * s + 1.0) / full
            } else {
                (s / (((1u64 << (bits - 1)) - 1) as f64)).max(-1.0)
            };
            // The quotient rounds to f32 first, then the power-of-two scale applies.
            let want = ((q as f32) as f64 * scale) as f32;
            let got = f32::from_bits(out[16 * i + 4 + k]);
            checked += 1;
            if got.to_bits() != want.to_bits() {
                inexact += 1;
                if inexact_ex.is_empty() {
                    inexact_ex = format!("f {f:08x} component {k} field {v}: got {got:e}, want {want:e}");
                }
            }
        }
    }
    println!("decoder_check: {checked} normalised components against the correctly rounded quotient: {inexact} differ {inexact_ex}");
    if args.get(2).map(String::as_str) == Some("exact") && inexact == 0 {
        println!("decoder_check: the new decoder is exact; differences from the old one are the old one's rounding");
    }
    println!("decoder_check: {} elements, {} formats", cases.len(), formats.len());
    for (fmt, (n, ex)) in &bad {
        println!("  format {fmt}: {n} differ, e.g. {ex}");
    }
    println!("  swizzles that differ: {bad_swz}");
    if bad.is_empty() && bad_swz == 0 {
        println!("decoder_check: every element and swizzle matches bit for bit");
    } else {
        std::process::exit(1);
    }
}
