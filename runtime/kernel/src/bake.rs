//! Native version of the game's cube depth bake, `sub_82F58250` (Phase 7: the
//! last long stalls when an entrance starts).
//!
//! `sub_82F58250(out, cube, mesh, size, flag)` casts a ray from a mesh's
//! centre through every texel of a 6 x size x size cube, against every
//! triangle of the mesh (`sub_82F77B20`, a single-sided ray/triangle test),
//! keeps the farthest hit and stores it, scaled and packed, as one byte per
//! texel. That is 6 * size^2 * triangles ray tests on one guest thread, and the
//! game's main thread waits for it: 185-200 ms recompiled, twice per match.
//!
//! This is the same computation, transcribed operation by operation from the
//! recompiled function with the same VMX helpers (`ppc_rt`), so every byte
//! matches; it runs on locals instead of the guest register file and stack,
//! with the mesh read once, and the faces' rows spread over host threads.
//! The guest-visible effects are the original's: the same calls to the
//! cube's setup and finishing functions (`sub_82F52BE0`, `sub_82F52CF8`,
//! `sub_82F57EE8`) with the same arguments in a frame at the same address,
//! the texels written in the original's order, and `*out = 0`.
//!
//! `WWE14_BAKE=original` runs the recompiled function instead;
//! `WWE14_BAKE=verify` runs it and then checks this version against what it
//! wrote, byte for byte.

use std::sync::atomic::{AtomicUsize, Ordering};

use ppc_rt::*;

use crate::{diag, mem};

const ORIGINAL: u32 = 0x82F5_8250;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Native,
    Original,
    Verify,
}

fn mode() -> Mode {
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("WWE14_BAKE").as_deref() {
        Ok("original") => Mode::Original,
        Ok("verify") => Mode::Verify,
        _ => Mode::Native,
    })
}

fn original(c: &mut Ctx, b: *mut u8) {
    unsafe { ppc_rt::original(ORIGINAL).expect("the bake is replaced")(c, b) };
}

/// Guest element 0 of a vector (the word `stvewx` stores at a 16-byte-aligned address).
fn elem0(v: u128) -> u32 {
    (v >> 96) as u32
}

fn vec4(x: f32, y: f32, z: f32, w: f32) -> u128 {
    ((x.to_bits() as u128) << 96) | ((y.to_bits() as u128) << 64) | ((z.to_bits() as u128) << 32) | w.to_bits() as u128
}

fn f32_at(b: *mut u8, a: u32) -> f64 {
    f32::from_bits(unsafe { ld32(b, a) }) as f64
}

/// The ray/triangle test's constants (`sub_82F77B20`).
struct TriConsts {
    eps: u128,
    zero: f64,
    one: u128,
    v41: u128,
}

/// `sub_82F77B20(hit, origin, dir, p0, p1, p2, out)`: the distance along
/// `dir` (splatted) where the ray hits the triangle's front, or `None`.
#[inline(always)]
fn ray_triangle(k: &TriConsts, origin: u128, dir: u128, p0: u128, p1: u128, p2: u128) -> Option<u128> {
    let v59 = vsubfp(p1, p0);
    let v61 = vsubfp(p2, p0);
    let v60 = vpermwi(dir, 99);
    let v57 = vpermwi(v59, 99);
    let v56 = vmulfp(v59, v60);
    let v55 = vmulfp(dir, v57);
    let v54 = vsubfp(v55, v56);
    let pvec = vpermwi(v54, 99);
    let det = vmsum3fp(v61, pvec);
    if cr6(vcmpgtfp(k.eps, det)).lt {
        return None;
    }
    let tvec = vsubfp(origin, p0);
    let u = vmsum3fp(tvec, pvec);
    if (f32::from_bits(elem0(u)) as f64) < k.zero {
        return None;
    }
    if cr6(vcmpgtfp(u, det)).lt {
        return None;
    }
    let v50 = vpermwi(tvec, 99);
    let v49 = vpermwi(v61, 99);
    let v48 = vmulfp(v61, v50);
    let v47 = vmulfp(tvec, v49);
    let v46 = vsubfp(v47, v48);
    let qvec = vpermwi(v46, 99);
    let v = vmsum3fp(dir, qvec);
    if (f32::from_bits(elem0(v)) as f64) < k.zero {
        return None;
    }
    if cr6(vcmpgtfp(vaddfp(u, v), det)).lt {
        return None;
    }
    let t = vmsum3fp(v59, qvec);
    let r = vrefp(det);
    let v11 = vnmsubfp(r, det, k.one);
    let r = vmaddfp(v11, r, r);
    let v40 = vmulfp(k.v41, r);
    Some(vmulfp(t, v40))
}

/// What the texel loops need, computed as the original's prologue computes it.
struct Setup {
    /// The rays' origin (`r1+0xA0`): the bounds' centre, or a constant.
    origin: u128,
    /// `r1+0xC0..0xFF`, the block `sub_82F52BE0` reads.
    block: [u128; 4],
    /// `N` as a float (`r1+0x80`, `0x84`, `0x88`) and the depth scale (`r1+0x8C`).
    size_f: f32,
    scale: f32,
}

fn setup(b: *mut u8, mesh: u32, n: i32, flag: bool) -> Setup {
    unsafe {
        let mut vmin = vspltw(ldvl(b, 0x8211_BAD8), 0);
        let mut vmax = vspltw(ldvl(b, 0x8211_BAD4), 0);
        let count = ld32(b, mesh.wrapping_add(4));
        if count as i32 > 0 {
            let verts = ld32(b, mesh);
            for i in 0..count {
                let v = ldv(b, verts.wrapping_add(16 * i) & !15);
                vmin = vminfp(vmin, v);
                vmax = vmaxfp(vmax, v);
            }
        }
        let extent = vsubfp(vmax, vmin);
        let zero = vspltisw(0);
        let r18 = 0x8213_1B80u32;
        let c110 = ldv(b, r18 + 0x110);
        let mut origin = ldv(b, r18 + 0x10);
        let radius: f64;
        if flag {
            let v60 = vmsum3fp(extent, extent);
            let half = vspltw(ldvl(b, 0x8202_B6D4), 0);
            let centre = vmaddfp(extent, half, vmin);
            let v0 = vrsqrtefp(v60);
            let v12 = vmulfp(v60, c110);
            let v58 = vcmpgefp(zero, v60);
            origin = centre;
            let v11 = vmulfp(v0, v0);
            let v13 = vnmsubfp(v12, v11, c110);
            let v0 = vmaddfp(v0, v13, v0);
            let v57 = vmulfp(v60, v0);
            let v56 = v57 & !v58;
            let f13 = f32::from_bits(elem0(v56)) as f64;
            radius = (f13 * f32_at(b, 0x8202_B6D4)) as f32 as f64;
        } else {
            let v55 = vmsum3fp(vmax, vmax);
            let v54 = vmsum3fp(vmin, vmin);
            let v13 = vrsqrtefp(v55);
            let v12 = vrsqrtefp(v54);
            let v8 = vmulfp(v55, c110);
            let v10 = vmulfp(v54, c110);
            let v53 = vcmpgefp(zero, v55);
            let v52 = vcmpgefp(zero, v54);
            let v9 = vmulfp(v13, v13);
            let v11 = vmulfp(v12, v12);
            let v7 = vnmsubfp(v8, v9, c110);
            let v0 = vnmsubfp(v10, v11, c110);
            let v13 = vmaddfp(v13, v7, v13);
            let v0 = vmaddfp(v12, v0, v12);
            let v49 = vmulfp(v55, v13) & !v53;
            let v48 = vmulfp(v54, v0) & !v52;
            let f0 = f32::from_bits(elem0(v49)) as f64;
            let f13 = f32::from_bits(elem0(v48)) as f64;
            radius = if f0 > f13 { f0 } else { f13 };
        }
        let size_f = ((n as i64) as f64) as f32;
        let mask = ldv(b, 0x8201_C8A0);
        let scale = (f32_at(b, 0x8200_46A4) / radius) as f32;
        Setup {
            origin,
            block: [ldv(b, r18 + 0x1C0), ldv(b, r18 + 0x1D0), ldv(b, r18 + 0x1E0), origin ^ mask],
            size_f,
            scale,
        }
    }
}

/// The loop constants (`f26`-`f30`, `v125`, the normalising constant).
struct Loop {
    f26: f64,
    f27: f64,
    f28: f64,
    f29: f64,
    f30: f64,
    v125: u128,
    c110: u128,
    zero: u128,
    tri: TriConsts,
}

fn loop_consts(b: *mut u8, s: &Setup) -> Loop {
    unsafe {
        let f29 = f32_at(b, 0x8218_B8C0);
        Loop {
            f26: f32_at(b, 0x8200_3E0C),
            f27: (f29 / s.size_f as f64) as f32 as f64,
            f28: f32_at(b, 0x8200_4734),
            f29,
            f30: f32_at(b, 0x8218_B8CC),
            v125: vspltw(ldvl(b, 0x8218_B8CC), 0),
            c110: ldv(b, 0x8213_1C90),
            zero: vspltisw(0),
            tri: TriConsts {
                eps: ldv(b, 0x8396_00B0),
                zero: f32_at(b, 0x8218_B8CC),
                one: ldv(b, 0x8213_1BA0),
                v41: vspltw(ldvl(b, 0x8218_B8C0), 0),
            },
        }
    }
}

/// A texel's coordinate on its face, from its row or column.
#[inline(always)]
fn coord(l: &Loop, i: i32) -> f64 {
    let f13 = (i as i64) as f64;
    let f12 = f13 as f32 as f64;
    let f11 = (f12 * l.f27) as f32 as f64;
    let f10 = (f11.mul_add(l.f26, -l.f29)) as f32 as f64;
    (f10 + l.f27) as f32 as f64
}

/// The farthest hit for one texel: the word the original stores at `r1+0x70`.
#[inline(always)]
fn texel(l: &Loop, origin: u128, tris: &[[u128; 3]], face: u32, row: f64, col: f64) -> u32 {
    let (row, col) = (row as f32, col as f32);
    let (f28, f29, f30) = (l.f28 as f32, l.f29 as f32, l.f30 as f32);
    let dir = match face {
        0 => vec4(f29, row, col, f30),
        1 => vec4(f28, row, col, f30),
        2 => vec4(col, f29, row, f30),
        3 => vec4(col, f28, row, f30),
        4 => vec4(col, row, f29, f30),
        _ => vec4(col, row, f28, f30),
    };
    let len2 = vmsum3fp(dir, dir);
    let v0 = vrsqrtefp(len2);
    let v11 = vmulfp(len2, l.c110);
    let v44 = vcmpgefp(l.zero, len2);
    let v12 = vmulfp(v0, v0);
    let v13 = vnmsubfp(v11, v12, l.c110);
    let v0 = vmaddfp(v0, v13, v0);
    let dir = vmulfp(dir, v0 & !v44);
    let mut far = l.v125;
    for t in tris {
        if let Some(h) = ray_triangle(&l.tri, origin, dir, t[0], t[1], t[2])
            && cr6(vcmpgtfp(h, far)).lt
        {
            far = h;
        }
    }
    elem0(far)
}

/// The packed byte for a texel's farthest hit (the integer code at `0x82F58654`).
#[inline(always)]
fn pack(scale: f32, far: u32) -> u8 {
    let f12 = ((scale as f64) * (f32::from_bits(far) as f64)) as f32;
    let mut r = [0u64; 32];
    r[21] = 0xFFFF_FFFF_FF80_0000;
    r[22] = 1;
    r[9] = f12.to_bits() as u64;
    r[11] = rotl32(r[9], 9) & 0xFF;
    r[11] = r[11].wrapping_add(0xFFFF_FFFF_FFFF_FF81);
    r[8] = r[11].wrapping_add(0xFFFF_FFFF_FFFF_FFE9);
    r[10] = rotl32(r[9], 0) & 0x7FFF_FFFF;
    r[6] = r[8].wrapping_add(0xFFFF_FFFF_FFFF_FFFF);
    r[5] = r[10].wrapping_add(0xFFFF_FFFF_FFFF_FFFF);
    r[10] = ((r[6] as i32) >> 31) as i64 as u64;
    r[4] = 0x17u64.wrapping_sub(r[11]);
    r[3] = ((r[5] as i32) >> 31) as i64 as u64;
    r[5] = rotl32(r[10], 0) & 0x1F;
    r[6] = r[4] & r[10];
    r[5] = rotl32(r[5], 0) & 0xFFFF_FFFF_FFFF_FFF7;
    r[4] = r[9] & !r[3];
    r[3] = r[5].wrapping_sub(r[6]);
    r[9] = !(r[10] | r[10]);
    r[5] = {
        let n = (r[3] & 63) as u32;
        let v = r[21] as i32;
        (if n >= 32 { v >> 31 } else { v >> n }) as i64 as u64
    };
    r[3] = r[4];
    r[5] |= r[9];
    r[3] = (rotl32(r[22], 23) & 0xFF80_0000) | (r[3] & !0xFF80_0000u64);
    r[10] = ((r[4] as i32) >> 31) as i64 as u64;
    r[4] = r[3] & r[5];
    r[3] = ((r[11] as i32) >> 31) as i64 as u64;
    r[11] = rotl32(r[4], 1) & 0xFFFF_FFFE;
    r[9] = r[8] & r[9];
    r[8] = r[11].wrapping_add(0xFFFF_FFFF_FFFF_FFFF);
    r[5] = r[8] | r[10];
    r[11] = r[5].wrapping_sub(r[4]);
    r[4] = r[11].wrapping_add(1);
    r[3] = r[4] & !r[3];
    r[11] = {
        let n = (r[6] & 63) as u32;
        let v = r[3] as i32;
        (if n >= 32 { v >> 31 } else { v >> n }) as i64 as u64
    };
    r[10] = {
        let n = r[9] & 63;
        if n >= 32 { 0 } else { ((r[11] as u32) << n) as u64 }
    };
    (rotl32(r[10], 0) & 0xFF) as u8
}

/// The guest address of texel (`face`, `row`, `col`), from the cube's header as
/// the original reads it per texel.
fn texel_address(b: *mut u8, cube: u32, face: u32, row: u32, col: u32) -> u32 {
    unsafe {
        let base = ld32(b, cube.wrapping_add(0x2C + 4 * face)) as u64;
        let stride = ld16(b, cube.wrapping_add(0xE)) as u64;
        let all = ld32(b, cube.wrapping_add(0x20)) as u64;
        let step = ((stride as i32 as i64).wrapping_mul((col + 1) as i32 as i64)) as u64;
        ea(step.wrapping_add(base).wrapping_add(all).wrapping_add(row as u64), 1)
    }
}

/// Every texel's farthest-hit word, in the original's order (face, row, column),
/// computed on host threads.
fn compute(b: *mut u8, mesh: u32, n: i32, s: &Setup) -> Vec<u32> {
    let l = loop_consts(b, s);
    let tris: Vec<[u128; 3]> = unsafe {
        let count = ld32(b, mesh.wrapping_add(0x10)) as i32;
        let (verts, list) = (ld32(b, mesh), ld32(b, mesh.wrapping_add(0xC)));
        let vertex = |i: u32| ldv(b, verts.wrapping_add(i << 4) & !15);
        (0..count.max(0) as u32)
            .map(|t| {
                let tri = list.wrapping_add(16 * t);
                [vertex(ld32(b, tri)), vertex(ld32(b, tri + 4)), vertex(ld32(b, tri + 8))]
            })
            .collect()
    };
    let n = n.max(0) as usize;
    let coords: Vec<f64> = (0..n as i32).map(|i| coord(&l, i)).collect();
    let mut out = vec![0u32; 6 * n * n];
    // One task per face row on rayon's persistent pool: the game bakes about 40
    // small cubes in a row, so spawning threads per bake would cost more than it saves.
    use rayon::prelude::*;
    out.par_chunks_mut(n.max(1)).enumerate().for_each(|(i, row)| {
        let (face, r) = ((i / n) as u32, i % n);
        for (col, word) in row.iter_mut().enumerate() {
            *word = texel(&l, s.origin, &tris, face, coords[r], coords[col]);
        }
    });
    out
}

fn call(c: &mut Ctx, b: *mut u8, target: u32, lr: u32) {
    c.lr = lr as u64;
    unsafe { call_indirect(c, b, target) };
}

/// Every texel's byte as the cube holds it now, in the original's order.
fn snapshot(b: *mut u8, cube: u32, n: i32) -> Vec<u8> {
    let n = n.max(0) as u32;
    let mut v = Vec::with_capacity(6 * (n * n) as usize);
    for face in 0..6 {
        for row in 0..n {
            for col in 0..n {
                v.push(mem::r8(texel_address(b, cube, face, row, col)));
            }
        }
    }
    v
}

/// `Game_BakeCubeDepth`: the replacement for `sub_82F58250`.
pub extern "C-unwind" fn bake_cube_depth(c: &mut Ctx, b: *mut u8) {
    let m = mode();
    let sp = c.r[1] as u32;
    if m == Mode::Original || sp.wrapping_sub(0x1F0) & 15 != 0 {
        if m != Mode::Original {
            diag::log(&format!("bake: stack pointer 0x{sp:08X} not 16-byte aligned; running the recompiled original"));
        }
        return original(c, b);
    }
    let (cube, mesh, n) = (c.r[4] as u32, c.r[5] as u32, c.r[6] as i32);
    let tris = mem::r32(mesh.wrapping_add(0x10));
    if m == Mode::Verify {
        // The native bake, then the original from the same registers; the cube's
        // texels after each (including the finishing pass over face edges).
        let (r, lr) = (c.r, c.lr);
        let t = std::time::Instant::now();
        native(c, b);
        let native_ms = t.elapsed().as_secs_f64() * 1000.0;
        let mine = snapshot(b, cube, n);
        let mine_r3 = c.r[3];
        (c.r, c.lr) = (r, lr);
        let t = std::time::Instant::now();
        original(c, b);
        let original_ms = t.elapsed().as_secs_f64() * 1000.0;
        let theirs = snapshot(b, cube, n);
        let bad = mine.iter().zip(&theirs).filter(|(a, b)| a != b).count();
        let first = mine.iter().zip(&theirs).position(|(a, b)| a != b).map_or(String::new(), |i| {
            let nn = n.max(1) as usize;
            format!(" (first: face {} row {} col {}: original {:02X}, native {:02X})", i / (nn * nn), (i / nn) % nn, i % nn, theirs[i], mine[i])
        });
        diag::log(&format!(
            "bake: verify {n}x{n}x6, {tris} triangles: original {original_ms:.1} ms, native {native_ms:.1} ms; \
             {bad} of {} texels differ{first}; r3 {}",
            mine.len(),
            if mine_r3 == c.r[3] { "same" } else { "DIFFERS" }
        ));
        return;
    }
    let t = std::time::Instant::now();
    native(c, b);
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 20 {
        diag::log(&format!("bake: {n}x{n}x6, {tris} triangles, native {:.1} ms", t.elapsed().as_secs_f64() * 1000.0));
    }
}

/// The bake, natively: the original's calls and guest-visible writes, with the
/// texel loops on host threads.
fn native(c: &mut Ctx, b: *mut u8) {
    let sp = c.r[1] as u32;
    let frame = sp.wrapping_sub(0x1F0);
    let (out, cube, mesh) = (c.r[3] as u32, c.r[4] as u32, c.r[5] as u32);
    let (n64, n, flag) = (c.r[6], c.r[6] as i32, c.r[7] as i8 != 0);
    let lr = c.lr;
    let s = setup(b, mesh, n, flag);
    unsafe {
        // The frame as the original leaves it for its callees.
        st32(b, frame, sp);
        st64(b, frame + 0x70, n as i64 as u64);
        st32(b, frame + 0x80, s.size_f.to_bits());
        st32(b, frame + 0x84, s.size_f.to_bits());
        st32(b, frame + 0x88, s.size_f.to_bits());
        st32(b, frame + 0x8C, s.scale.to_bits());
        stv(b, frame + 0xA0, s.origin);
        for (k, v) in s.block.iter().enumerate() {
            stv(b, frame + 0xC0 + 16 * k as u32, *v);
        }
    }
    c.r[1] = frame as u64;
    c.r[3] = cube as u64;
    c.r[4] = (frame + 0xC0) as u64;
    c.r[5] = (frame + 0x80) as u64;
    call(c, b, 0x82F5_2BE0, 0x82F5_8464);
    c.r[3] = cube as u64;
    c.r[4] = n64;
    call(c, b, 0x82F5_2CF8, 0x82F5_8470);
    let words = compute(b, mesh, n, &s);
    let nn = n.max(0) as usize;
    for (i, &w) in words.iter().enumerate() {
        let (face, row, col) = ((i / (nn * nn)) as u32, ((i / nn) % nn) as u32, (i % nn) as u32);
        mem::w8(texel_address(b, cube, face, row, col), pack(s.scale, w));
    }
    if let Some(&last) = words.last() {
        unsafe { st32(b, frame + 0x70, last) };
    }
    c.r[3] = (frame + 0x70) as u64;
    c.r[4] = cube as u64;
    call(c, b, 0x82F5_7EE8, 0x82F5_8744);
    mem::w32(out, 0);
    c.r[3] = out as u64;
    c.r[1] = sp as u64;
    c.lr = lr;
}
