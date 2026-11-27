//! Xenos microcode → WGSL.
//!
//! The generated shader keeps the Xenos machine model as plain WGSL values: a
//! 64-entry temporary register file `r`, the address register `a0`, the loop
//! counters `aL`, the predicate `p0` and the previous scalar result `ps`.
//! Control flow becomes a `loop { switch pc { ... } }` over the CF
//! instructions, which handles any jumps, loops and calls. Constants, bool and
//! loop constants and fetch constants come from uniforms the renderer fills
//! from D3D's shadow state; vertex data is read from a storage buffer holding
//! guest memory (big-endian words), textures from bound texture/sampler pairs.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::ucode::{self, Alu, Cf, ExecCond, Fetch, Source, TextureFetch, VertexFetch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Pixel,
}

/// What the renderer needs to know to bind a translated shader.
#[derive(Debug, Default)]
pub struct Reflection {
    /// Texture fetch constants sampled, with their dimension (1 2D, 3 cube).
    pub textures: BTreeSet<(u32, u32)>,
    /// Filters the fetch instructions set over a fetch constant's (mag, min,
    /// mip; 3 = the constant's), by fetch constant. The renderer's sampler for
    /// that constant takes them (one sampler per constant: the first fetch's).
    pub filters: std::collections::BTreeMap<u32, [u32; 3]>,
    /// Microcode slot of each vertex fetch, in descriptor order: `U[dr.vf + k]`
    /// describes the fetch at slot `vertex_fetch_slots[k]`.
    pub vertex_fetch_slots: Vec<u32>,
    /// Interpolators written (vertex) or read (pixel).
    pub interpolators: BTreeSet<u32>,
    /// Pixel color outputs written.
    pub colors: BTreeSet<u32>,
    pub writes_depth: bool,
    pub kills: bool,
}

pub struct Translation {
    pub wgsl: String,
    pub reflection: Reflection,
}

struct Emitter {
    stage: Stage,
    body: String,
    refl: Reflection,
    errors: Vec<String>,
    /// The body calls `tex_size` (emitted only then, so other shaders' text,
    /// and their compiled pipelines, stay the same).
    uses_tex_size: bool,
}

/// A texture's size in guest texels, from its fetch constant (word 2: width - 1
/// in bits 0-12, height - 1 in bits 13-25). Texel units (unnormalized
/// coordinates, texel offsets) are the guest's: the host's texture may be
/// larger, as a render target drawn at a higher resolution is.
const TEX_SIZE: &str = r#"
fn tex_size(n: u32) -> vec2<f32> {
    let w = fetch_word(6u * n + 2u);
    return vec2<f32>(f32((w & 0x1FFFu) + 1u), f32(((w >> 13u) & 0x1FFFu) + 1u));
}
"#;

// Xenos computes texture LODs per quad whatever the control flow; the
// translated control flow is a state machine WGSL can't prove uniform.
const HEADER: &str = r#"diagnostic(off, derivative_uniformity);

// A frame's draw data: one buffer of 16-byte rows (`U`) and a record per draw
// (`DR`) with where each of its blocks starts in it, in rows. The vertex shader
// gets its draw from the instance index (the renderer draws instance `k` of
// draw `k`) and passes it on to the pixel shader. Blocks: vertex constants (a
// table of 4 rows), control (2 rows of booleans, 8 of loop constants), fetch
// constants (48), vertex memory windows (8), vertex fetch descriptors, pixel
// constants (a table of 4 rows), the position transform (2) and the alpha test
// (1). A constant table holds the first rows of the stage's 16 blocks of 16
// registers: draws share the blocks they have in common.
struct DrawRec { vk: u32, ctl: u32, fet: u32, win: u32, vf: u32, pk: u32, xf: u32, at: u32 }
@group(0) @binding(0) var<storage, read> U: array<vec4<u32>>;
@group(0) @binding(1) var<storage, read> DR: array<DrawRec>;
var<private> dr: DrawRec;

// Float constant `i` of this stage (a uniform array clamped its index too):
// its block's first row from the table, then the register within the block.
fn kc(i: u32) -> vec4<f32> {
    let j = min(i, 255u);
    return bitcast<vec4<f32>>(U[U[dr.KFIELD + j / 64u][(j / 16u) % 4u] + j % 16u]);
}
fn fetch_word(i: u32) -> u32 { return U[dr.fet + min(i, 191u) / 4u][i % 4u]; }
fn bool_const(i: u32) -> bool { return ((U[dr.ctl + min(i, 255u) / 128u][(i / 32u) % 4u] >> (i % 32u)) & 1u) != 0u; }
fn loop_const(i: u32) -> u32 { return U[dr.ctl + 2u + min(i, 31u) / 4u][i % 4u]; }
// Xenos (D3D9) multiply: 0 times anything, even infinity or NaN, is 0.
fn xmul(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    return select(a * b, vec4<f32>(0.0), (a == vec4<f32>(0.0)) | (b == vec4<f32>(0.0)));
}
fn xmuls(a: f32, b: f32) -> f32 { return select(a * b, 0.0, a == 0.0 || b == 0.0); }
fn xdot4(a: vec4<f32>, b: vec4<f32>) -> f32 { let m = xmul(a, b); return m.x + m.y + m.z + m.w; }
fn d3dmax(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> { return select(b, a, a >= b); }
fn d3dmin(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> { return select(b, a, a < b); }
const FLT_MAX: f32 = 3.402823466e38;
// The cube ALU op on direction d: (tc, sc, 2 * major axis, face), with the
// usual cube-map face selection (faces +X -X +Y -Y +Z -Z = 0..5).
fn cube_op(d: vec3<f32>) -> vec4<f32> {
    let a = abs(d);
    if (a.z >= a.x && a.z >= a.y) {
        return vec4<f32>(-d.y, select(d.x, -d.x, d.z < 0.0), 2.0 * d.z, select(4.0, 5.0, d.z < 0.0));
    }
    if (a.y >= a.x) {
        return vec4<f32>(select(d.z, -d.z, d.y < 0.0), d.x, 2.0 * d.y, select(2.0, 3.0, d.y < 0.0));
    }
    return vec4<f32>(-d.y, select(-d.z, d.z, d.x < 0.0), 2.0 * d.x, select(0.0, 1.0, d.x < 0.0));
}
// A cube fetch's coordinates (s, t in [1, 2], face) back to a direction.
fn cube_dir(c: vec3<f32>) -> vec3<f32> {
    let sc = (c.x - 1.5) * 2.0;
    let tc = (c.y - 1.5) * 2.0;
    switch u32(clamp(floor(c.z + 0.5), 0.0, 5.0)) {
        case 0u: { return vec3<f32>(1.0, -tc, -sc); }
        case 1u: { return vec3<f32>(-1.0, -tc, sc); }
        case 2u: { return vec3<f32>(sc, 1.0, tc); }
        case 3u: { return vec3<f32>(sc, -1.0, -tc); }
        case 4u: { return vec3<f32>(sc, -tc, 1.0); }
        default: { return vec3<f32>(-sc, -tc, -1.0); }
    }
}
"#;

const VERTEX_DECLS: &str = r#"
@group(0) @binding(3) var<storage, read> M: array<u32>;
"#;

/// The vertex formats `vfetch_decode` knows: format, each fetched component's
/// (word, first bit, width), and the kind of number (see `vlayout`).
const VERTEX_LAYOUTS: &[(u32, &[(u32, u32, u32)], u32)] = &[
    (6, &[(0, 0, 8), (0, 8, 8), (0, 16, 8), (0, 24, 8)], 0),
    (7, &[(0, 0, 10), (0, 10, 10), (0, 20, 10), (0, 30, 2)], 0),
    (16, &[(0, 0, 11), (0, 11, 11), (0, 22, 10)], 0),
    (17, &[(0, 0, 10), (0, 10, 11), (0, 21, 11)], 0),
    (25, &[(0, 0, 16), (0, 16, 16)], 0),
    (26, &[(0, 0, 16), (0, 16, 16), (1, 0, 16), (1, 16, 16)], 0),
    (31, &[(0, 0, 16), (0, 16, 16)], 1),
    (32, &[(0, 0, 16), (0, 16, 16), (1, 0, 16), (1, 16, 16)], 1),
    (33, &[(0, 0, 32)], 3),
    (34, &[(0, 0, 32), (1, 0, 32)], 3),
    (35, &[(0, 0, 32), (1, 0, 32), (2, 0, 32), (3, 0, 32)], 3),
    (36, &[(0, 0, 32)], 2),
    (37, &[(0, 0, 32), (1, 0, 32)], 2),
    (38, &[(0, 0, 32), (1, 0, 32), (2, 0, 32), (3, 0, 32)], 2),
    (57, &[(0, 0, 32), (1, 0, 32), (2, 0, 32)], 2),
];

/// The vertex declarations with the decoder, its layout table filled in.
fn vertex_decls() -> String {
    let mut cases = String::new();
    for &(fmt, comps, kind) in VERTEX_LAYOUTS {
        let mut c = [0u32; 4];
        for (i, &(word, first, width)) in comps.iter().enumerate() {
            c[i] = word | first << 2 | width << 7;
        }
        let _ = writeln!(
            cases,
            "        case {fmt}u: {{ return VLayout(vec4<u32>({}u, {}u, {}u, {}u), {}u, {kind}u); }}",
            c[0],
            c[1],
            c[2],
            c[3],
            comps.len()
        );
    }
    format!("{VERTEX_DECLS}{}", VERTEX_DECODE.replace("__LAYOUTS__", &cases))
}

const VERTEX_DECODE: &str = r#"
// One vertex element's layout by format: each fetched component's word
// (bits 0-1), first bit (2-6) and width (7-12); how many components are
// fetched; and how they are numbers (0 integer or normalised fields of under
// 32 bits, 1 16-bit floats, 2 32-bit floats, 3 32-bit integer or normalised).
// Formats name fields from the most significant; x is lowest.
// (Each format had its own decoding code, and each word its own search of
// the windows: inlined into every fetch, that made the driver take 1.5 s over
// a skinned vertex shader's first compile, 2026-11-25.)
struct VLayout { c: vec4<u32>, n: u32, kind: u32 }
fn vlayout(fmt: u32) -> VLayout {
    switch fmt {
__LAYOUTS__        default: { return VLayout(vec4<u32>(0u), 0u, 0u); }
    }
}
// Fields of `bits` bits (< 32). The format word's flags: bit 12 signed, bit
// 13 integer (else normalised), bit 14 signed mode (0: x / (2^(n-1) - 1)
// clamped to -1; 1: (2x + 1) / (2^n - 1)). Unfetched lanes (0 bits) give
// anything; the caller replaces them.
// The quotients are correctly rounded (`vdiv`): the attire shaders decode
// bytes back from them, and GPU division alone can be an ulp low (checked
// against the f64 quotient for every normalised format by the renderer's
// `decoder_check` example; v / (2^n - 1) is never near a tie).
// (A bit-exact, division-free version, tried 2026-11-25 for a suspected
// rounding bug that turned out not to exist, made the driver compile every
// pipeline three times slower: it was a loop in each format's code.)
fn vnum(v: vec4<u32>, bits: vec4<u32>, f: u32) -> vec4<f32> {
    let sg = ((f >> 12u) & 1u) != 0u;
    let it = ((f >> 13u) & 1u) != 0u;
    let sh = vec4<u32>(32u) - bits;
    let s = vec4<f32>(bitcast<vec4<i32>>(v << sh) >> sh);
    let one = vec4<u32>(1u);
    if (it) { return select(vec4<f32>(v), s, sg); }
    if (!sg) { return vdiv(vec4<f32>(v), vec4<f32>((one << bits) - one)); }
    if (((f >> 14u) & 1u) != 0u) { return vdiv(2.0 * s + 1.0, vec4<f32>((one << bits) - one)); }
    return max(vdiv(s, vec4<f32>((one << (bits - one)) - one)), vec4<f32>(-1.0));
}
// n / d correctly rounded, for integers n and d below 2^24: the quotient is
// within an ulp, its residual is exact in a fused multiply-add, and one
// correction step rounds it right.
fn vdiv(n: vec4<f32>, d: vec4<f32>) -> vec4<f32> {
    let q = n / d;
    return fma(fma(-q, d, n), 1.0 / d, q);
}
// 16-bit floats to f32 without the optional f16 capability. Exponent 31 is an
// ordinary exponent: Xenos 16-bit floats have no infinity or NaN.
fn half(h: vec4<u32>) -> vec4<f32> {
    let e = (h >> vec4<u32>(10u)) & vec4<u32>(0x1Fu);
    let m = h & vec4<u32>(0x3FFu);
    let v = select(bitcast<vec4<f32>>(((e + 112u) << vec4<u32>(23u)) | (m << vec4<u32>(13u))), vec4<f32>(m) * 5.9604645e-8, e == vec4<u32>(0u));
    return select(v, -v, ((h >> vec4<u32>(15u)) & vec4<u32>(1u)) != vec4<u32>(0u));
}
fn vnum32(v: vec4<u32>, f: u32) -> vec4<f32> {
    let sg = ((f >> 12u) & 1u) != 0u;
    let it = ((f >> 13u) & 1u) != 0u;
    let s = vec4<f32>(bitcast<vec4<i32>>(v));
    if (it) { return select(vec4<f32>(v), s, sg); }
    if (!sg) { return vec4<f32>(v) / 4294967295.0; }
    return max(s / 2147483647.0, vec4<f32>(-1.0));
}
// Decodes one vertex element at physical word address `addr`. `f` is the
// fetch's format word: swizzle (bits 0-11), flags (12-15), format (16-21),
// exponent adjust (24-29, signed). M holds guest memory bytes as they are, so
// M[i] is the little-endian read the GPU makes; the fetch constant's endian
// mode then swaps each word (2 = 8-in-32 for words the big-endian CPU wrote).
fn vfetch_decode(addr: u32, endian: u32, f: u32) -> vec4<f32> {
    let lay = vlayout((f >> 16u) & 0x3Fu);
    // The element's words: from the draw's window (up to 8, in U: first
    // physical word, word count, where it starts in M) holding the first, and
    // 0 outside it.
    var at = vec2<u32>(0u, 0u);
    for (var k = 0u; k < 8u; k = k + 1u) {
        let w = U[dr.win + k];
        let i = addr - w.x;
        if (i < w.y) {
            at = vec2<u32>(w.z + i, w.y - i);
            break;
        }
    }
    let last = max(at.y, 1u) - 1u;
    let lane = vec4<u32>(0u, 1u, 2u, 3u);
    var raw = vec4<u32>(M[at.x], M[at.x + min(1u, last)], M[at.x + min(2u, last)], M[at.x + min(3u, last)]);
    raw = select(vec4<u32>(0u), raw, lane < vec4<u32>(at.y));
    // Endian modes: 1 swaps bytes in halves, 3 swaps halves, 2 both.
    raw = select(raw, ((raw & vec4<u32>(0x00FF00FFu)) << vec4<u32>(8u)) | ((raw >> vec4<u32>(8u)) & vec4<u32>(0x00FF00FFu)), endian == 1u || endian == 2u);
    raw = select(raw, (raw << vec4<u32>(16u)) | (raw >> vec4<u32>(16u)), endian >= 2u);
    let c = lay.c;
    let bits = (c >> vec4<u32>(7u)) & vec4<u32>(63u);
    let first = (c >> vec4<u32>(2u)) & vec4<u32>(31u);
    let word = c & vec4<u32>(3u);
    let fld = vec4<u32>(
        extractBits(raw[word.x], first.x, bits.x),
        extractBits(raw[word.y], first.y, bits.y),
        extractBits(raw[word.z], first.z, bits.z),
        extractBits(raw[word.w], first.w, bits.w),
    );
    var v: vec4<f32>;
    switch lay.kind {
        case 0u: { v = vnum(fld, bits, f); }
        case 1u: { v = half(fld); }
        case 2u: { v = bitcast<vec4<f32>>(fld); }
        default: { v = vnum32(fld, f); }
    }
    // The exponent adjust scales fetched components only; the others are 0, 0, 0, 1.
    let scale = exp2(f32(i32(f << 2u) >> 26u));
    return select(vec4<f32>(0.0, 0.0, 0.0, 1.0), v * scale, lane < vec4<u32>(lay.n));
}
// One destination component from a fetch swizzle selector: 0-3 a fetched
// component, 4 zero, 5 one, 6-7 keep.
fn vswz(old: f32, v: vec4<f32>, s: u32) -> f32 {
    return select(select(v[s & 3u], f32(s - 4u), s >= 4u), old, s >= 6u);
}
"#;

impl Emitter {
    fn line(&mut self, s: &str) {
        self.body.push_str("            ");
        self.body.push_str(s);
        self.body.push('\n');
    }

    fn const_index(&self, a: &Alu, slot: usize, index: u32) -> String {
        if a.const_rel[slot] {
            if a.address_absolute {
                format!("u32(i32({index}) + a0)")
            } else {
                format!("u32(i32({index}) + aL[ld])")
            }
        } else {
            format!("{index}u")
        }
    }

    /// A float constant register. A shader's own literals (`def`) are not
    /// baked in: D3D loads them into the constant file when it sets the shader,
    /// and the renderer's constants carry them the same way.
    fn constant(&self, a: &Alu, slot: usize, index: u32) -> String {
        format!("kc({})", self.const_index(a, slot, index))
    }

    /// The operand as a vec4 expression (swizzle, abs, negate applied).
    fn operand(&self, a: &Alu, s: &Source, const_slot: usize) -> String {
        let base = if s.temp {
            if s.relative { format!("r[u32(i32({}) + aL[ld])]", s.index) } else { format!("r[{}]", s.index) }
        } else {
            self.constant(a, const_slot, s.index)
        };
        let swz: String = s.swizzle.iter().map(|&c| ['x', 'y', 'z', 'w'][c as usize]).collect();
        let mut e = if swz == "xyzw" { base } else { format!("{base}.{swz}") };
        if s.abs || (!s.temp && a.abs_constants) {
            e = format!("abs({e})");
        }
        if s.negate {
            e = format!("(-{e})");
        }
        e
    }

    fn alu(&mut self, a: &Alu) {
        // Constant operands are numbered in source order for relative addressing.
        let mut slots = [0usize; 3];
        let mut n = 0;
        for (i, s) in a.src.iter().enumerate() {
            if !s.temp {
                slots[i] = n.min(1);
                n += 1;
            }
        }
        let src: Vec<String> = (0..3).map(|i| self.operand(a, &a.src[i], slots[i])).collect();
        let vop = a.vector_op;
        let vector_used = a.vector_mask != 0 || (20..=27).contains(&vop) || vop == 29;
        let scalar_used = a.scalar_op != 50 || a.scalar_mask != 0;
        self.line("{");
        // Only the operands the vector op reads. The others' fields can hold
        // anything: with a constant scalar op (42-47) the third is a constant
        // index and a temp's bits, which read as a loop-relative temp (r61 + aL
        // for c253). Reading it anyway made the driver keep all 64 registers in
        // indexable memory, and 594 shaders without loops compiled several
        // times slower (2-3 s for the wrestlers' pipelines, 2026-11-25).
        let reads = |k: usize| match k {
            0 => true,
            1 => !matches!(vop, 8..=10 | 18 | 19),
            _ => matches!(vop, 11..=14 | 17),
        };
        if vector_used {
            if reads(2) && (42..=47).contains(&a.scalar_op) {
                self.errors.push(format!("vector op {vop} reads src3, which constant scalar op {} uses as a constant", a.scalar_op));
            }
            let lets: Vec<String> = (0..3).filter(|&k| reads(k)).map(|k| format!("let s{k} = {};", src[k])).collect();
            self.line(&lets.join(" "));
        }
        // Scalar operands: a = component in the w slot, b = in the x slot (of src3).
        let s3 = &a.src[2];
        let comp = |c: u32| ['x', 'y', 'z', 'w'][c as usize];
        if scalar_used {
            match a.scalar_op {
                42..=47 => {
                    // Constant (src3 register field) with a temporary encoded in
                    // the opcode's low bit, src3 swizzle bits 2..5 and the src3 select bit.
                    let swz = {
                        let mut v = 0u32;
                        for (i, &c) in s3.swizzle.iter().enumerate() {
                            v |= ((c + 4 - i as u32) & 3) << (2 * i);
                        }
                        v
                    };
                    // The temporary's index, 0-63: bit 0 from the opcode (the _0/_1
                    // pair), bit 1 the src3 select bit, bits 2-5 from the swizzle.
                    // It is never relative.
                    let reg2 = (a.scalar_op & 1) | (swz & 0x3C) | ((s3.temp as u32) << 1);
                    // The select bit belongs to reg2, so the register field is
                    // all constant index (decode_alu split it as a temp's).
                    let creg = if s3.temp { s3.index | (s3.relative as u32) << 6 | (s3.abs as u32) << 7 } else { s3.index };
                    let slot = (!a.src[0].temp || !a.src[1].temp) as usize;
                    let mut ca = format!("{}.{}", self.constant(a, slot, creg), comp(((swz >> 6) + 3) & 3));
                    let mut cb = format!("r[{reg2}].{}", comp(swz & 3));
                    // Absolute value and negation apply to both operands
                    // (checked against a screen-quad shader: x*scale - 1 needs both negated).
                    if a.abs_constants {
                        ca = format!("abs({ca})");
                        cb = format!("abs({cb})");
                    }
                    if s3.negate {
                        ca = format!("(-{ca})");
                        cb = format!("(-{cb})");
                    }
                    self.line(&format!("let sa = {ca}; let sb = {cb};"));
                }
                // `src[2]` is already swizzled: its w slot is operand a, its x slot operand b.
                _ => self.line(&format!("let t3 = {}; let sa = t3.w; let sb = t3.x;", src[2])),
            }
        }
        // Vector result.
        let v = if !vector_used { String::new() } else { match vop {
            0 => "s0 + s1".into(),
            1 => "xmul(s0, s1)".into(),
            2 => "d3dmax(s0, s1)".into(),
            3 => "d3dmin(s0, s1)".into(),
            4 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 == s1)".into(),
            5 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 > s1)".into(),
            6 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 >= s1)".into(),
            7 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 != s1)".into(),
            8 => "fract(s0)".into(),
            9 => "trunc(s0)".into(),
            10 => "floor(s0)".into(),
            11 => "xmul(s0, s1) + s2".into(),
            12 => "select(s2, s1, s0 == vec4<f32>(0.0))".into(),
            13 => "select(s2, s1, s0 >= vec4<f32>(0.0))".into(),
            14 => "select(s2, s1, s0 > vec4<f32>(0.0))".into(),
            15 => "vec4<f32>(xdot4(s0, s1))".into(),
            16 => "vec4<f32>(xdot4(vec4<f32>(s0.xyz, 0.0), vec4<f32>(s1.xyz, 0.0)))".into(),
            17 => "vec4<f32>(xmuls(s0.x, s1.x) + xmuls(s0.y, s1.y) + s2.x)".into(),
            19 => "vec4<f32>(max(max(s0.x, s0.y), max(s0.z, s0.w)))".into(),
            20..=23 => {
                // The first operand is always tested for 0; the opcode's test applies to the second.
                let op = ["==", "!=", ">", ">="][(vop - 20) as usize];
                self.line(&format!("let push = (s0.w == 0.0) && (s1.w {op} 0.0);"));
                format!("select(vec4<f32>(s0.x + 1.0), vec4<f32>(0.0), (s0.x == 0.0) && (s1.x {op} 0.0))")
            }
            24..=27 => {
                let op = ["==", ">", ">=", "!="][(vop - 24) as usize];
                self.refl.kills |= self.stage == Stage::Pixel;
                self.line(&format!("let kill = any(s0 {op} s1);"));
                "select(vec4<f32>(0.0), vec4<f32>(1.0), kill)".into()
            }
            28 => "vec4<f32>(1.0, xmuls(s0.y, s1.y), s0.z, s1.w)".into(),
            29 => "d3dmax(s0, s1)".into(),
            // cube: the compiler passes the direction as src1 = d.zzxy, src2 = d.yxzz.
            18 => "cube_op(vec3<f32>(s0.z, s0.w, s0.x))".into(),
            _ => {
                self.errors.push(format!("unknown vector op {vop}"));
                "vec4<f32>(0.0)".into()
            }
        } };
        if vector_used {
            self.line(&format!("var vres = {v};"));
            if a.vector_clamp {
                self.line("vres = clamp(vres, vec4<f32>(0.0), vec4<f32>(1.0));");
            }
        }
        // Scalar result.
        if scalar_used {
            let s = match a.scalar_op {
                0 => "sa + sb".into(),
                1 => "sa + ps".into(),
                2 => "xmuls(sa, sb)".into(),
                3 => "xmuls(sa, ps)".into(),
                4 => "select(xmuls(sa, ps), -FLT_MAX, ps == -FLT_MAX || !(abs(ps) <= FLT_MAX) || !(abs(sb) <= FLT_MAX) || sb <= 0.0)".into(),
                5 => "select(sb, sa, sa >= sb)".into(),
                6 => "select(sb, sa, sa < sb)".into(),
                7 => "select(0.0, 1.0, sa == 0.0)".into(),
                8 => "select(0.0, 1.0, sa > 0.0)".into(),
                9 => "select(0.0, 1.0, sa >= 0.0)".into(),
                10 => "select(0.0, 1.0, sa != 0.0)".into(),
                11 => "fract(sa)".into(),
                12 => "trunc(sa)".into(),
                13 => "floor(sa)".into(),
                14 => "exp2(sa)".into(),
                15 => "max(log2(sa), -FLT_MAX)".into(),
                16 => "log2(sa)".into(),
                17 => "clamp(1.0 / sa, -FLT_MAX, FLT_MAX)".into(),
                18 => "select(1.0 / sa, 0.0, abs(1.0 / sa) > FLT_MAX)".into(),
                19 => "1.0 / sa".into(),
                20 => "clamp(inverseSqrt(sa), -FLT_MAX, FLT_MAX)".into(),
                21 => "select(inverseSqrt(sa), 0.0, abs(inverseSqrt(sa)) > FLT_MAX)".into(),
                22 => "inverseSqrt(sa)".into(),
                23 | 24 => "select(sb, sa, sa >= sb)".into(),
                25 => "sa - sb".into(),
                26 => "sa - ps".into(),
                27..=30 => {
                    let op = ["==", "!=", ">", ">="][(a.scalar_op - 27) as usize];
                    format!("select(1.0, 0.0, sa {op} 0.0)")
                }
                // setp_inv: 1 becomes 0 (and sets the predicate), 0 becomes 1, anything
                // else is kept. Leaving 1 as 1 lost the place in an if/else-if chain
                // (the custom championship belt's region colours came out black, 2026-11-26).
                31 => "select(select(sa, 1.0, sa == 0.0), 0.0, sa == 1.0)".into(),
                32 => "select(sa - 1.0, 0.0, sa - 1.0 <= 0.0)".into(),
                33 => "FLT_MAX".into(),
                34 => "sa".into(),
                35..=39 => {
                    let cond = ["sa == 0.0", "sa > 0.0", "sa >= 0.0", "sa != 0.0", "sa == 1.0"][(a.scalar_op - 35) as usize];
                    self.refl.kills |= self.stage == Stage::Pixel;
                    self.line(&format!("let skill = {cond};"));
                    "select(0.0, 1.0, skill)".into()
                }
                40 => "sqrt(sa)".into(),
                42 | 43 => "xmuls(sa, sb)".into(),
                44 | 45 => "sa + sb".into(),
                46 | 47 => "sa - sb".into(),
                48 => "sin(sa)".into(),
                49 => "cos(sa)".into(),
                50 => "ps".into(),
                op => {
                    self.errors.push(format!("unknown scalar op {op}"));
                    "0.0".into()
                }
            };
            self.line(&format!("var sres: f32 = {s};"));
            if a.scalar_clamp {
                self.line("sres = clamp(sres, 0.0, 1.0);");
            }
        }
        // Predication guards every write and side effect.
        let guard = if a.predicated { format!("p0 == {}", a.pred_condition) } else { "true".into() };
        self.line(&format!("if ({guard}) {{"));
        if (24..=27).contains(&vop) && self.stage == Stage::Pixel {
            self.line("    if (kill) { discard; }");
        }
        if (35..=39).contains(&a.scalar_op) && self.stage == Stage::Pixel {
            self.line("    if (skill) { discard; }");
        }
        if (20..=23).contains(&vop) {
            self.line("    p0 = push;");
        }
        match a.scalar_op {
            27..=30 => {
                let op = ["==", "!=", ">", ">="][(a.scalar_op - 27) as usize];
                self.line(&format!("    p0 = sa {op} 0.0;"));
            }
            31 => self.line("    p0 = sa == 1.0;"),
            32 => self.line("    p0 = sa - 1.0 <= 0.0;"),
            33 => self.line("    p0 = false;"),
            34 => self.line("    p0 = sa == 0.0;"),
            23 => self.line("    a0 = clamp(i32(floor(sa + 0.5)), -256, 255);"),
            24 => self.line("    a0 = clamp(i32(floor(sa)), -256, 255);"),
            _ => {}
        }
        if vop == 29 {
            self.line("    a0 = clamp(i32(floor(s0.w + 0.5)), -256, 255);");
        }
        // Exports share their write masks: components in both masks get 1.0,
        // and with the scalar-relative bit set, components in neither get 0.0.
        let (vmask, smask, ones, zeros) = if a.export {
            let both = a.vector_mask & a.scalar_mask;
            let zeros = if a.scalar_dest_rel { 0xF & !(a.vector_mask | a.scalar_mask) } else { 0 };
            (a.vector_mask & !both, a.scalar_mask & !both, both, zeros)
        } else {
            (a.vector_mask, a.scalar_mask, 0, 0)
        };
        if vector_used && vmask != 0 {
            let d = self.dest(a, a.vector_dest, a.vector_dest_rel);
            self.write_masked(&d, "vres", vmask);
        }
        if ones | zeros != 0 {
            let d = self.dest(a, a.vector_dest, false);
            self.write_masked(&d, "vec4<f32>(1.0)", ones);
            self.write_masked(&d, "vec4<f32>(0.0)", zeros);
        }
        if scalar_used && smask != 0 {
            // An exporting instruction has one destination, the export register
            // in the vector destination field; the masks pick which components
            // the vector and scalar results provide.
            let d = if a.export { self.dest(a, a.vector_dest, false) } else { self.dest(a, a.scalar_dest, a.scalar_dest_rel) };
            self.write_masked(&d, "vec4<f32>(sres)", smask);
        }
        if scalar_used {
            self.line("    ps = sres;");
        }
        self.line("}");
        self.line("}");
    }

    fn dest(&mut self, a: &Alu, d: u32, rel: bool) -> String {
        if !a.export {
            return if rel { format!("r[u32(i32({d}) + aL[ld])]") } else { format!("r[{d}]") };
        }
        match (self.stage, d) {
            (Stage::Vertex, 62) => "opos".into(),
            (Stage::Vertex, 63) => "opts".into(),
            (Stage::Vertex, n) if n < 16 => {
                self.refl.interpolators.insert(n);
                format!("o[{n}]")
            }
            (Stage::Pixel, n) if n < 4 => {
                self.refl.colors.insert(n);
                format!("oc[{n}]")
            }
            (Stage::Pixel, 61) => {
                self.refl.writes_depth = true;
                "odepth".into()
            }
            (_, n) => {
                self.errors.push(format!("export to register {n} (memory export?) is not supported yet"));
                "junk".into()
            }
        }
    }

    fn write_masked(&mut self, dest: &str, value: &str, mask: u32) {
        if mask == 0xF {
            self.line(&format!("    {dest} = {value};"));
        } else {
            for (i, c) in ['x', 'y', 'z', 'w'].iter().enumerate() {
                if mask & (1 << i) != 0 {
                    self.line(&format!("    {dest}.{c} = ({value}).{c};"));
                }
            }
        }
    }

    /// A vertex fetch. D3D rewrites each fetch's element layout for the bound
    /// vertex declaration at draw time (`sub_82CBE418`: fetch constant use,
    /// format, flags, swizzle, stride, offset, and which fetches become mini
    /// fetches), so the layout is not part of the translated shader: fetch k
    /// reads it from descriptor `U[dr.vf + k]`, which the renderer fills from D3D's
    /// result. Registers and predication are fixed and stay in the code.
    fn vfetch(&mut self, f: &VertexFetch, slot: usize) {
        if self.stage != Stage::Vertex {
            self.errors.push("vertex fetch in a pixel shader".into());
            return;
        }
        let k = self.refl.vertex_fetch_slots.len();
        self.refl.vertex_fetch_slots.push(slot as u32);
        let src = if f.src_rel { format!("r[u32(i32({}) + aL[ld])]", f.src) } else { format!("r[{}]", f.src) };
        let comp = ['x', 'y', 'z', 'w'][f.src_comp as usize];
        let d = if f.dst_rel { format!("r[u32(i32({}) + aL[ld])]", f.dst) } else { format!("r[{}]", f.dst) };
        let guard = if f.predicated { format!("p0 == {}", f.pred_condition) } else { "true".into() };
        self.line("{");
        self.line(&format!("let vd = U[dr.vf + {k}u];"));
        // A full fetch sets the stream (fetch constant), index and stride that
        // following mini fetches (format word bit 30) reuse.
        self.line("if (((vd.y >> 30u) & 1u) == 0u) {");
        self.line("    let fw0 = fetch_word(vd.x * 2u); let fw1 = fetch_word(vd.x * 2u + 1u);");
        self.line("    vf_base = fw0 >> 2u; vf_endian = fw1 & 3u; vf_stride = vd.z;");
        self.line(&format!("    let vi = {src}.{comp};"));
        self.line("    vf_index = u32(i32(select(floor(vi), floor(vi + 0.5), ((vd.y >> 15u) & 1u) != 0u)));");
        self.line("}");
        self.line("let v = vfetch_decode(vf_base + vf_index * vf_stride + vd.w, vf_endian, vd.y);");
        self.line(&format!("if ({guard}) {{"));
        self.line(&format!("    let o = {d};"));
        self.line(&format!(
            "    {d} = vec4<f32>(vswz(o.x, v, vd.y & 7u), vswz(o.y, v, (vd.y >> 3u) & 7u), vswz(o.z, v, (vd.y >> 6u) & 7u), vswz(o.w, v, (vd.y >> 9u) & 7u));"
        ));
        self.line("}");
        self.line("}");
    }

    fn tfetch(&mut self, t: &TextureFetch) {
        let src = if t.src_rel { format!("r[u32(i32({}) + aL[ld])]", t.src) } else { format!("r[{}]", t.src) };
        let c = |i: usize| ['x', 'y', 'z', 'w'][t.src_swz[i] as usize];
        let guard = if t.predicated { format!("p0 == {}", t.pred_condition) } else { "true".into() };
        match t.opcode {
            1 => {}
            // set_texture_lod: the LOD register for later fetches that use it.
            24 => {
                self.line(&format!("if ({guard}) {{ tlod = {src}.{}; }}", c(0)));
                return;
            }
            op => {
                self.errors.push(format!("texture fetch opcode {op} is not supported yet"));
                return;
            }
        }
        let n = t.fetch_const;
        if let Some(&(_, dim)) = self.refl.textures.iter().find(|&&(f, _)| f == n)
            && dim != t.dimension
        {
            self.errors.push(format!("fetch constant {n} sampled as dimensions {dim} and {}", t.dimension));
            return;
        }
        let coord = match t.dimension {
            1 => {
                self.line("{");
                self.line(&format!("var uv = vec2<f32>({src}.{}, {src}.{});", c(0), c(1)));
                if t.unnormalized {
                    self.uses_tex_size = true;
                    self.line(&format!("uv = uv / tex_size({n}u);"));
                }
                if t.offset[0] != 0.0 || t.offset[1] != 0.0 {
                    self.uses_tex_size = true;
                    self.line(&format!("uv = uv + vec2<f32>({:?}, {:?}) / tex_size({n}u);", t.offset[0], t.offset[1]));
                }
                "uv"
            }
            // Cube: (s, t) in [1, 2] and the face index, as the cube ALU op and
            // the compiler's scale and bias leave them; turned back into a direction.
            3 => {
                self.line("{");
                self.line(&format!("let uv = cube_dir(vec3<f32>({src}.{}, {src}.{}, {src}.{}));", c(0), c(1), c(2)));
                "uv"
            }
            d => {
                self.errors.push(format!("texture dimension {d} is not supported yet"));
                return;
            }
        };
        self.refl.textures.insert((n, t.dimension));
        if t.filters != [3, 3, 3] {
            self.refl.filters.entry(n).or_insert(t.filters);
        }
        // LOD: computed from derivatives (pixel shaders only), plus the LOD
        // register if used, plus the instruction's bias.
        let bias = format!("{:?}", t.lod_bias);
        let reg = if t.use_register_lod { "tlod + " } else { "" };
        let sample = if self.stage == Stage::Vertex || !t.use_computed_lod {
            format!("textureSampleLevel(t{n}, s{n}, {coord}, {reg}{bias})")
        } else if t.use_register_lod || t.lod_bias != 0.0 {
            format!("textureSampleBias(t{n}, s{n}, {coord}, {reg}{bias})")
        } else {
            format!("textureSample(t{n}, s{n}, {coord})")
        };
        self.line(&format!("let tv = {sample};"));
        let d = if t.dst_rel { format!("r[u32(i32({}) + aL[ld])]", t.dst) } else { format!("r[{}]", t.dst) };
        self.line(&format!("if ({guard}) {{"));
        for (i, ch) in ['x', 'y', 'z', 'w'].iter().enumerate() {
            match t.dst_swz[i] {
                s @ 0..=3 => self.line(&format!("    {d}.{ch} = tv.{};", ['x', 'y', 'z', 'w'][s as usize])),
                4 => self.line(&format!("    {d}.{ch} = 0.0;")),
                5 => self.line(&format!("    {d}.{ch} = 1.0;")),
                _ => {}
            }
        }
        self.line("}");
        self.line("}");
    }

    fn cond(&self, c: ExecCond) -> String {
        match c {
            ExecCond::Always => "true".into(),
            ExecCond::Bool { index, value } => format!("bool_const({index}u) == {value}"),
            ExecCond::Pred { value } => format!("p0 == {value}"),
        }
    }
}

impl Emitter {
    /// The body of one exec: its ALU and fetch instructions.
    fn exec(&mut self, s: &[ucode::Slot], address: u32, count: u32, sequence: u32) {
        for k in 0..count {
            let a = (address + k) as usize;
            let Some(&slot) = s.get(a) else {
                self.errors.push(format!("exec past the end of the shader (slot {a})"));
                continue;
            };
            if (sequence >> (2 * k)) & 1 != 0 {
                match ucode::decode_fetch(slot) {
                    Fetch::Vertex(v) => self.vfetch(&v, a),
                    Fetch::Texture(t) => self.tfetch(&t),
                }
            } else {
                self.alu(&ucode::decode_alu(slot));
            }
        }
    }
}

/// For control flow that can be emitted structured: each `LoopStart`'s
/// `LoopEnd` (by index), or `None` when the program needs the CF state
/// machine. Structured means no calls or returns; properly nested loops whose
/// `LoopStart` exits to just after its `LoopEnd` and whose `LoopEnd` repeats
/// from just after its `LoopStart`; and only forward jumps, none of which
/// lands inside a loop body from outside it.
fn loop_pairs(cf: &[Cf]) -> Option<std::collections::HashMap<usize, usize>> {
    let mut pairs = std::collections::HashMap::new();
    let mut open: Vec<usize> = Vec::new();
    for (n, c) in cf.iter().enumerate() {
        match *c {
            Cf::Call { .. } | Cf::Return => return None,
            Cf::LoopStart { .. } => open.push(n),
            Cf::LoopEnd { address, .. } => {
                let start = open.pop()?;
                let Cf::LoopStart { address: exit, .. } = cf[start] else { return None };
                if address as usize != start + 1 || exit as usize != n + 1 {
                    return None;
                }
                pairs.insert(start, n);
            }
            _ => {}
        }
    }
    if !open.is_empty() {
        return None;
    }
    // A loop body is (start, end]: from the instruction after LoopStart through LoopEnd.
    let inside = |at: usize, (start, end): (usize, usize)| at > start && at <= end;
    for (n, c) in cf.iter().enumerate() {
        if let Cf::Jump { address, .. } = *c {
            let to = address as usize;
            if to <= n {
                return None;
            }
            if pairs.iter().any(|(&s, &e)| inside(to, (s, e)) && !inside(n, (s, e))) {
                return None;
            }
        }
    }
    Some(pairs)
}

/// Emits CF instructions `start..end` structured (`loop_pairs`): each in an
/// `if (pc == n)` block, the program counter only growing within a level, and
/// each loop a WGSL `loop` whose body restarts at the instruction after its
/// `LoopStart`. Same semantics as the state machine: a jump out of a loop ends
/// the iteration and the loop, an `exec_end` anywhere ends the program.
fn emit_structured(e: &mut Emitter, s: &[ucode::Slot], cf: &[Cf], pairs: &std::collections::HashMap<usize, usize>, start: usize, end: usize, depth: usize) {
    let pad = "    ".repeat(depth + 2);
    let mut n = start;
    while n < end {
        let next = n + 1;
        let _ = writeln!(e.body, "{pad}if (pc == {n}u) {{");
        match cf[n] {
            Cf::LoopStart { loop_id, .. } => {
                let last = pairs[&n];
                let brk = match cf[last] {
                    Cf::LoopEnd { predicated_break: true, condition, .. } => format!("p0 == {condition}"),
                    _ => "false".into(),
                };
                let _ = writeln!(
                    e.body,
                    "{pad}    ld = ld + 1u; let lc = loop_const({loop_id}u); count[ld] = lc & 0xFFu; aL[ld] = i32((lc >> 8u) & 0xFFu);\n\
                     {pad}    if (count[ld] == 0u) {{ ld = ld - 1u; pc = {exit}u; }} else {{\n\
                     {pad}    loop {{\n\
                     {pad}        pc = {next}u;",
                    exit = last + 1
                );
                emit_structured(e, s, cf, pairs, next, last, depth + 2);
                let _ = writeln!(
                    e.body,
                    "{pad}        if (pc != {last}u) {{ break; }}\n\
                     {pad}        let le = loop_const({loop_id}u); count[ld] = count[ld] - 1u; aL[ld] = aL[ld] + (i32(le << 8u) >> 24);\n\
                     {pad}        if (count[ld] != 0u && !({brk})) {{ continue; }}\n\
                     {pad}        ld = ld - 1u; pc = {exit}u; break;\n\
                     {pad}    }}\n\
                     {pad}    }}",
                    exit = last + 1
                );
                let _ = writeln!(e.body, "{pad}}}");
                n = last + 1;
                continue;
            }
            // A LoopEnd at this level is its loop's (handled above); never reached on its own.
            Cf::LoopEnd { .. } => {}
            Cf::Nop | Cf::Alloc { .. } | Cf::MarkVsFetchDone => {
                let _ = writeln!(e.body, "{pad}    pc = {next}u;");
            }
            Cf::Exec { address, count, sequence, end: last, cond, .. } => {
                let c = e.cond(cond);
                let _ = writeln!(e.body, "{pad}    if ({c}) {{");
                e.exec(s, address, count, sequence);
                let _ = writeln!(e.body, "{pad}    }}");
                // exec_end ends the program whether or not its condition held.
                let _ = writeln!(e.body, "{pad}    pc = {}u;", if last { u32::MAX as usize } else { next });
            }
            Cf::Jump { address, cond } => {
                let c = e.cond(cond);
                let _ = writeln!(e.body, "{pad}    if ({c}) {{ pc = {address}u; }} else {{ pc = {next}u; }}");
            }
            Cf::Call { .. } | Cf::Return => unreachable!("loop_pairs refuses calls and returns"),
        }
        let _ = writeln!(e.body, "{pad}}}");
        n = next;
    }
}

/// Translates one shader.
pub fn translate(code: &[u8], stage: Stage) -> Result<Translation, String> {
    let s = ucode::slots(code);
    let cf = ucode::control_flow(&s);
    let mut e = Emitter { stage, body: String::new(), refl: Reflection::default(), errors: Vec::new(), uses_tex_size: false };
    // Most shaders are a run of (conditional) execs: emit those as straight-line
    // code. Anything with jumps, calls or loops becomes a CF state machine.
    let straight = cf.iter().all(|c| matches!(c, Cf::Nop | Cf::Alloc { .. } | Cf::MarkVsFetchDone | Cf::Exec { .. }));
    // Only forward jumps (no calls, returns or loops): the program counter only
    // grows, so the blocks run in order without a loop around them. Drivers
    // compile that far faster than a switch inside a loop.
    let forward = cf.iter().enumerate().all(|(n, c)| match *c {
        Cf::Jump { address, .. } => address as usize > n,
        Cf::Call { .. } | Cf::Return | Cf::LoopStart { .. } | Cf::LoopEnd { .. } => false,
        _ => true,
    });
    // Loops too, but nested properly and entered only at their start
    // (`loop_pairs`): structured loops instead of the state machine, which
    // drivers compile far more slowly (the game's last 12 such shaders, all
    // pixel shaders with one or two loops). Forward-only programs keep their
    // own form, so their WGSL stays byte for byte what it was.
    let pairs = if straight || forward { None } else { loop_pairs(&cf) };
    let structured = pairs.is_some();
    if let Some(pairs) = &pairs {
        emit_structured(&mut e, &s, &cf, pairs, 0, cf.len(), 0);
    } else if straight {
        for c in &cf {
            if let Cf::Exec { address, count, sequence, end, cond, .. } = *c {
                let c = e.cond(cond);
                let _ = writeln!(e.body, "    if ({c}) {{");
                e.exec(&s, address, count, sequence);
                let _ = writeln!(e.body, "    }}");
                if end {
                    break;
                }
            }
        }
    } else {
        for (n, c) in cf.iter().enumerate() {
            let next = n + 1;
            // Forward-only programs: each CF instruction once, in order, when the
            // program counter has reached it (it only grows). Otherwise a case of
            // the state machine's switch.
            if forward {
                let _ = writeln!(e.body, "        if (pc == {n}u) {{");
            } else {
                let _ = writeln!(e.body, "        case {n}u: {{");
            }
            match *c {
                Cf::Nop | Cf::Alloc { .. } | Cf::MarkVsFetchDone => {
                    let _ = writeln!(e.body, "            pc = {next}u;");
                }
                Cf::Exec { address, count, sequence, end, cond, .. } => {
                    let c = e.cond(cond);
                    let _ = writeln!(e.body, "            if ({c}) {{");
                    e.exec(&s, address, count, sequence);
                    let _ = writeln!(e.body, "            }}");
                    // exec_end ends the program whether or not its condition held.
                    if end {
                        let _ = writeln!(e.body, "            {}", if forward { "pc = 0xFFFFFFFFu;" } else { "done = true;" });
                    } else {
                        let _ = writeln!(e.body, "            pc = {next}u;");
                    }
                }
                Cf::Jump { address, cond } => {
                    let c = e.cond(cond);
                    let _ = writeln!(e.body, "            if ({c}) {{ pc = {address}u; }} else {{ pc = {next}u; }}");
                }
                Cf::Call { address, cond } => {
                    let c = e.cond(cond);
                    let _ = writeln!(
                        e.body,
                        "            if ({c}) {{ stack[sp] = {next}u; sp = sp + 1u; pc = {address}u; }} else {{ pc = {next}u; }}"
                    );
                }
                Cf::Return => {
                    let _ = writeln!(e.body, "            if (sp == 0u) {{ done = true; }} else {{ sp = sp - 1u; pc = stack[sp]; }}");
                }
                Cf::LoopStart { address, loop_id, .. } => {
                    // Loop constant: count (bits 0-7), start (8-15), signed step (16-23).
                    let _ = writeln!(
                        e.body,
                        "            ld = ld + 1u; let lc = loop_const({loop_id}u); count[ld] = lc & 0xFFu; aL[ld] = i32((lc >> 8u) & 0xFFu);\n            \
                         if (count[ld] == 0u) {{ ld = ld - 1u; pc = {address}u; }} else {{ pc = {next}u; }}"
                    );
                }
                Cf::LoopEnd { address, loop_id, predicated_break, condition } => {
                    let brk = if predicated_break { format!("p0 == {condition}") } else { "false".into() };
                    let _ = writeln!(
                        e.body,
                        "            let lc = loop_const({loop_id}u); count[ld] = count[ld] - 1u; aL[ld] = aL[ld] + (i32(lc << 8u) >> 24);\n            \
                         if (count[ld] != 0u && !({brk})) {{ pc = {address}u; }} else {{ ld = ld - 1u; pc = {next}u; }}"
                    );
                }
            }
            let _ = writeln!(e.body, "        }}");
        }
    }
    if !e.errors.is_empty() {
        let mut errs = e.errors.clone();
        errs.sort();
        errs.dedup();
        return Err(errs.join("; "));
    }

    // Float constants: one file per stage (D3D shadows them separately), so
    // vertex constants are binding 0 and pixel constants binding 6.
    let mut w = String::new();
    // Which fetch each descriptor `U[dr.vf + k]` is for (`ucode::vertex_fetch_key`):
    // D3D's patch reorders the fetches, so the renderer matches descriptors
    // to fetches by key, not by position.
    if stage == Stage::Vertex {
        let keys: Vec<String> = e.refl.vertex_fetch_slots.iter().map(|&i| format!("{:08x}", ucode::vertex_fetch_key(s[i as usize]))).collect();
        let _ = writeln!(w, "// vfetch-keys: {}", keys.join(" "));
    }
    // Sampler filters the fetch instructions set: `constant:mag,min,mip`.
    if !e.refl.filters.is_empty() {
        let f: Vec<String> = e.refl.filters.iter().map(|(n, [a, b, c])| format!("{n}:{a},{b},{c}")).collect();
        let _ = writeln!(w, "// tfetch-filters: {}", f.join(" "));
    }
    w.push_str(&HEADER.replace("KFIELD", if stage == Stage::Vertex { "vk" } else { "pk" }));
    let r = &e.refl;
    // Textures: binding 2n / 2n+1 for fetch constant n, shared by both stages
    // (vertex and pixel shaders index one fetch constant file).
    for &(n, dim) in &r.textures {
        let ty = if dim == 3 { "texture_cube<f32>" } else { "texture_2d<f32>" };
        let _ = writeln!(w, "@group(1) @binding({}) var t{n}: {ty};", 2 * n);
        let _ = writeln!(w, "@group(1) @binding({}) var s{n}: sampler;", 2 * n + 1);
    }
    let has_out = !r.colors.is_empty() || r.writes_depth;
    match stage {
        Stage::Vertex => {
            // Vertex fetch descriptors (see `Emitter::vfetch`, the draw's rows at
            // `dr.vf`): x fetch constant, y format word, z stride in words, w
            // offset in words.
            w.push_str(&vertex_decls());
            w.push_str("struct VsOut {\n    @builtin(position) pos: vec4<f32>,\n");
            for i in 0..16 {
                let _ = writeln!(w, "    @location({i}) o{i}: vec4<f32>,");
            }
            w.push_str("    @location(16) @interpolate(flat) draw: u32,\n");
            w.push_str("}\n\n@vertex\nfn main(@builtin(vertex_index) vertex_index: u32, @builtin(instance_index) draw: u32) -> VsOut {\n    dr = DR[draw];\n");
        }
        Stage::Pixel => {
            w.push_str("struct PsIn {\n    @builtin(position) frag_pos: vec4<f32>,\n");
            for i in 0..16 {
                let _ = writeln!(w, "    @location({i}) v{i}: vec4<f32>,");
            }
            w.push_str("    @location(16) @interpolate(flat) draw: u32,\n");
            w.push_str("}\n");
            if has_out {
                w.push_str("struct PsOut {\n");
                for &i in &r.colors {
                    let _ = writeln!(w, "    @location({i}) c{i}: vec4<f32>,");
                }
                if r.writes_depth {
                    w.push_str("    @builtin(frag_depth) depth: f32,\n");
                }
                w.push_str("}\n\n@fragment\nfn main(input: PsIn) -> PsOut {\n");
            } else {
                w.push_str("\n@fragment\nfn main(input: PsIn) {\n");
            }
            w.push_str("    dr = DR[input.draw];\n");
        }
    }
    w.push_str("    var r: array<vec4<f32>, 64>;\n    var p0: bool = false;\n    var a0: i32 = 0;\n    var ps: f32 = 0.0;\n");
    w.push_str("    var aL: array<i32, 5>;\n    var ld: u32 = 0u;\n    var junk: vec4<f32>;\n    var tlod: f32 = 0.0;\n");
    if stage == Stage::Vertex {
        w.push_str("    var vf_base: u32 = 0u;\n    var vf_endian: u32 = 0u;\n    var vf_stride: u32 = 0u;\n    var vf_index: u32 = 0u;\n");
    }
    if !straight {
        w.push_str("    var count: array<u32, 5>;\n    var stack: array<u32, 4>;\n    var sp: u32 = 0u;\n");
        w.push_str("    var pc: u32 = 0u;\n    var done: bool = false;\n");
    }
    match stage {
        Stage::Vertex => {
            w.push_str("    var o: array<vec4<f32>, 16>;\n    var opos = vec4<f32>(0.0, 0.0, 0.0, 1.0);\n    var opts = vec4<f32>(0.0);\n");
            w.push_str("    r[0] = vec4<f32>(f32(vertex_index), 0.0, 0.0, 0.0);\n");
        }
        Stage::Pixel => {
            w.push_str("    var oc: array<vec4<f32>, 4>;\n    var odepth = vec4<f32>(input.frag_pos.z);\n");
            for i in 0..16 {
                let _ = writeln!(w, "    r[{i}] = input.v{i};");
            }
        }
    }
    if straight || forward || structured {
        w.push_str(&e.body);
    } else {
        let _ = write!(
            w,
            "    loop {{\n        if (done) {{ break; }}\n        switch pc {{\n{}        default: {{ done = true; }}\n        }}\n    }}\n",
            e.body
        );
    }
    match stage {
        Stage::Vertex => {
            // The position as exported, through the renderer's transform (the
            // draw's two rows at `dr.xf`, m then z): xy identity while D3D's
            // viewport transform is on (the wgpu viewport does it), screen
            // space to NDC when D3D turned it off; z through D3D's depth scale
            // and offset (which may reverse it).
            w.push_str(
                "    var out: VsOut;\n    let xm = bitcast<vec4<f32>>(U[dr.xf]);\n    let xz = bitcast<vec4<f32>>(U[dr.xf + 1u]);\n    \
                 out.pos = vec4<f32>(opos.xy * xm.xy + xm.zw * opos.w, opos.z * xz.x + xz.y * opos.w, opos.w);\n",
            );
            for i in 0..16 {
                let _ = writeln!(w, "    out.o{i} = o[{i}];");
            }
            w.push_str("    out.draw = draw;\n    return out;\n}\n");
        }
        Stage::Pixel => {
            // Alpha test (fixed function on Xenos, `RB_COLORCONTROL`): colour 0's
            // alpha against the reference with the compare function, from the
            // draw's row at `dr.at`: x function, y reference bits (function 7 =
            // always passes).
            if r.colors.contains(&0) {
                w.push_str(
                    "    let at_a = oc[0].w;\n    let at_f = U[dr.at].x;\n    let at_r = bitcast<f32>(U[dr.at].y);\n    var at_pass = true;\n    switch at_f {\n        \
                     case 0u: { at_pass = false; }\n        case 1u: { at_pass = at_a < at_r; }\n        \
                     case 2u: { at_pass = at_a == at_r; }\n        case 3u: { at_pass = at_a <= at_r; }\n        \
                     case 4u: { at_pass = at_a > at_r; }\n        case 5u: { at_pass = at_a != at_r; }\n        \
                     case 6u: { at_pass = at_a >= at_r; }\n        default: {}\n    }\n    if (!at_pass) { discard; }\n",
                );
            }
            if has_out {
                w.push_str("    var out: PsOut;\n");
                for &i in &r.colors {
                    let _ = writeln!(w, "    out.c{i} = oc[{i}];");
                }
                if r.writes_depth {
                    w.push_str("    out.depth = odepth.x;\n");
                }
                w.push_str("    return out;\n");
            }
            w.push_str("}\n");
        }
    }
    if e.uses_tex_size {
        w.push_str(TEX_SIZE);
    }
    Ok(Translation { wgsl: w, reflection: e.refl })
}
