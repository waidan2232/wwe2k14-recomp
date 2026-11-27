//! Xenos shader microcode: decoding and disassembly.
//!
//! A shader is a sequence of 96-bit instruction slots. The first slots hold
//! control-flow (CF) instructions, two 48-bit ones per slot; `exec` CF
//! instructions name a run of later slots holding ALU and fetch instructions,
//! with a 2-bit-per-instruction sequence saying which kind each is.

/// One microcode slot: three big-endian words.
pub type Slot = [u32; 3];

pub fn slots(code: &[u8]) -> Vec<Slot> {
    code.chunks_exact(12)
        .map(|c| core::array::from_fn(|i| u32::from_be_bytes(c[4 * i..4 * i + 4].try_into().unwrap())))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecCond {
    Always,
    /// Run if bool constant `index` equals `value`.
    Bool { index: u32, value: bool },
    /// Run if the predicate equals `value`.
    Pred { value: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocKind {
    None,
    Position,
    Interpolators,
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cf {
    Nop,
    /// Run `count` instructions from slot `address`. `sequence` has 2 bits per
    /// instruction: bit 0 set = fetch, clear = ALU; bit 1 = serialize.
    Exec { address: u32, count: u32, sequence: u32, end: bool, cond: ExecCond, predicate_clean: bool },
    LoopStart { address: u32, loop_id: u32, repeat: bool },
    LoopEnd { address: u32, loop_id: u32, predicated_break: bool, condition: bool },
    Call { address: u32, cond: ExecCond },
    Return,
    Jump { address: u32, cond: ExecCond },
    Alloc { size: u32, kind: AllocKind },
    MarkVsFetchDone,
}

fn bits(v: u64, lo: u32, n: u32) -> u32 {
    ((v >> lo) & ((1u64 << n) - 1)) as u32
}

/// Decodes one 48-bit CF instruction.
pub fn decode_cf(v: u64) -> Cf {
    let opcode = bits(v, 44, 4);
    let exec = |end: bool, cond: ExecCond| Cf::Exec {
        address: bits(v, 0, 12),
        count: bits(v, 12, 3),
        sequence: bits(v, 16, 12),
        end,
        cond,
        predicate_clean: bits(v, 41, 1) != 0,
    };
    let bool_cond = || ExecCond::Bool { index: bits(v, 34, 8), value: bits(v, 42, 1) != 0 };
    let pred_cond = || ExecCond::Pred { value: bits(v, 42, 1) != 0 };
    // Calls and jumps: address (13 bits), force (always) bit 13, predicated bit 14.
    let branch_cond = || {
        if bits(v, 13, 1) != 0 {
            ExecCond::Always
        } else if bits(v, 14, 1) != 0 {
            pred_cond()
        } else {
            bool_cond()
        }
    };
    match opcode {
        0 => Cf::Nop,
        1 => exec(false, ExecCond::Always),
        2 => exec(true, ExecCond::Always),
        // Conditional on a bool constant: 3/4, and 13/14 (`CondExecPredClean`,
        // which also leaves the predicate clean); 5/6 test the predicate.
        3 | 13 => exec(false, bool_cond()),
        4 | 14 => exec(true, bool_cond()),
        5 => exec(false, pred_cond()),
        6 => exec(true, pred_cond()),
        7 => Cf::LoopStart { address: bits(v, 0, 13), loop_id: bits(v, 16, 5), repeat: bits(v, 13, 1) != 0 },
        8 => Cf::LoopEnd {
            address: bits(v, 0, 13),
            loop_id: bits(v, 16, 5),
            predicated_break: bits(v, 21, 1) != 0,
            condition: bits(v, 42, 1) != 0,
        },
        9 => Cf::Call { address: bits(v, 0, 13), cond: branch_cond() },
        10 => Cf::Return,
        11 => Cf::Jump { address: bits(v, 0, 13), cond: branch_cond() },
        12 => Cf::Alloc {
            size: bits(v, 0, 3),
            kind: match bits(v, 41, 2) {
                1 => AllocKind::Position,
                2 => AllocKind::Interpolators,
                3 => AllocKind::Memory,
                _ => AllocKind::None,
            },
        },
        _ => Cf::MarkVsFetchDone,
    }
}

/// The two CF instructions in a slot.
pub fn cf_pair(s: Slot) -> [Cf; 2] {
    let a = s[0] as u64 | ((s[1] as u64 & 0xFFFF) << 32);
    let b = (s[1] >> 16) as u64 | ((s[2] as u64) << 16);
    [decode_cf(a), decode_cf(b)]
}

/// Reads the CF program: pairs from slot 0 until the slot an exec first
/// points into (CF and instruction slots share one address space).
pub fn control_flow(s: &[Slot]) -> Vec<Cf> {
    let mut cf = Vec::new();
    let mut limit = s.len();
    let mut i = 0;
    while i < limit {
        for c in cf_pair(s[i]) {
            if let Cf::Exec { address, count, .. } = c
                && count > 0
            {
                limit = limit.min(address as usize);
            }
            cf.push(c);
        }
        i += 1;
    }
    cf
}

/// Slots holding vertex fetches, in program order.
pub fn vertex_fetch_slots(s: &[Slot]) -> Vec<usize> {
    let mut out = Vec::new();
    for c in control_flow(s) {
        if let Cf::Exec { address, count, sequence, .. } = c {
            for k in 0..count {
                let a = (address + k) as usize;
                if (sequence >> (2 * k)) & 1 != 0
                    && let Some(&slot) = s.get(a)
                    && matches!(decode_fetch(slot), Fetch::Vertex(_))
                    && !out.contains(&a)
                {
                    out.push(a);
                }
            }
        }
    }
    out
}

/// The texture fetch constants a program samples.
pub fn texture_fetch_constants(s: &[Slot]) -> Vec<u32> {
    let mut out = Vec::new();
    for c in control_flow(s) {
        if let Cf::Exec { address, count, sequence, .. } = c {
            for k in 0..count {
                if (sequence >> (2 * k)) & 1 != 0
                    && let Some(&slot) = s.get((address + k) as usize)
                    && let Fetch::Texture(t) = decode_fetch(slot)
                    && t.opcode == 1
                    && !out.contains(&t.fetch_const)
                {
                    out.push(t.fetch_const);
                }
            }
        }
    }
    out
}

/// The descriptor a translated vertex shader reads for one vertex fetch
/// (`VF.d[k]`), from the fetch as D3D patched it for the bound declaration:
/// fetch constant, format word (swizzle, flags, format, exponent adjust,
/// mini bit), stride and offset in words. The renderer fills one per entry of
/// the shader's `vertex_fetch_slots`.
pub fn vertex_fetch_descriptor(s: Slot) -> [u32; 4] {
    let [w0, w1, w2] = s;
    [((w0 >> 20) & 0x1F) * 3 + ((w0 >> 25) & 3), w1, w2 & 0xFF, sext((w2 >> 8) & 0x7F_FFFF, 23) as u32]
}

/// What identifies a vertex fetch whatever D3D's patch did to it: word 0 with
/// the patched fields masked (registers and operand component stay; the
/// opcode bits, zero for a vertex fetch, carry the two predication bits).
pub fn vertex_fetch_key(s: Slot) -> u32 {
    (s[0] & !0x3FF0_0000) | (s[1] >> 31) | ((s[2] >> 31) << 1)
}

/// Descriptors for every vertex fetch of (patched) microcode, in program
/// order, keyed by `vertex_fetch_key`. Mini fetches (format word bit 30) reuse
/// the stream of the full fetch before them; they are made full here (that
/// stream's fetch constant and stride, bit 30 clear), so each descriptor
/// stands alone whatever order the shader runs them in.
pub fn vertex_fetch_descriptors(s: &[Slot]) -> Vec<(u32, [u32; 4])> {
    let mut last = [0u32; 4];
    vertex_fetch_slots(s)
        .into_iter()
        .map(|i| {
            let mut d = vertex_fetch_descriptor(s[i]);
            if (d[1] >> 30) & 1 != 0 {
                d[0] = last[0];
                d[2] = last[2];
                d[1] &= !(1 << 30);
            } else {
                last = d;
            }
            (vertex_fetch_key(s[i]), d)
        })
        .collect()
}

/// A shader's identity: FNV-1a of its microcode with the vertex-fetch fields
/// D3D rewrites for the bound declaration masked out (fetch constant,
/// prefetch count, swizzle, format, flags, mini bit, stride, offset; only
/// registers, operand component and predication stay). D3D's patch also
/// reorders the fetches among their slots, so the fetches are hashed as a
/// sorted set after the other slots. The shader as shipped and as D3D
/// patched it at a draw then have the same identity.
pub fn identity(code: &[u8]) -> u64 {
    let mut s = slots(code);
    let fetches = vertex_fetch_slots(&s);
    let mut keys: Vec<u32> = fetches.iter().map(|&i| vertex_fetch_key(s[i])).collect();
    keys.sort_unstable();
    for &i in &fetches {
        s[i] = [0; 3];
    }
    s.iter()
        .flatten()
        .chain(keys.iter())
        .flat_map(|x| x.to_be_bytes())
        .fold(0xCBF2_9CE4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3))
}

// ---- Fetch ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexFetch {
    pub src: u32,
    pub src_rel: bool,
    /// Component of `src` holding the vertex index.
    pub src_comp: u32,
    pub dst: u32,
    pub dst_rel: bool,
    /// Vertex fetch constant 0..95 (32 slots × 3).
    pub fetch_const: u32,
    /// Per destination component: 0..3 = fetched component, 4 = 0.0, 5 = 1.0, 7 = keep.
    pub dst_swz: [u32; 4],
    pub signed: bool,
    /// Integer (unnormalised) rather than normalised fraction.
    pub integer: bool,
    pub format: u32,
    pub exp_adjust: i32,
    /// Reuses the previous full fetch's constant, stride and index.
    pub mini: bool,
    /// Stride in words.
    pub stride: u32,
    /// Offset in words.
    pub offset: i32,
    pub predicated: bool,
    pub pred_condition: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextureFetch {
    pub opcode: u32,
    pub src: u32,
    pub src_rel: bool,
    /// Coordinate components of `src` (x, y, z), absolute.
    pub src_swz: [u32; 3],
    pub dst: u32,
    pub dst_rel: bool,
    pub fetch_const: u32,
    pub unnormalized: bool,
    pub dst_swz: [u32; 4],
    /// 0 1D, 1 2D, 2 3D/stacked, 3 cube.
    pub dimension: u32,
    pub use_computed_lod: bool,
    pub use_register_lod: bool,
    pub use_register_gradients: bool,
    /// LOD bias in 1/16 steps? (raw 7-bit signed field / 16).
    pub lod_bias: f32,
    pub offset: [f32; 3],
    pub predicated: bool,
    pub pred_condition: bool,
    /// Filters the instruction sets over the fetch constant's (word 1 bits
    /// 12-17): magnification, minification, mip. 0 point, 1 linear, 2 base
    /// map (mip), 3 the fetch constant's.
    pub filters: [u32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fetch {
    Vertex(VertexFetch),
    Texture(TextureFetch),
}

fn sext(v: u32, n: u32) -> i32 {
    ((v << (32 - n)) as i32) >> (32 - n)
}

fn swz12(v: u32) -> [u32; 4] {
    core::array::from_fn(|i| (v >> (3 * i)) & 7)
}

pub fn decode_fetch(s: Slot) -> Fetch {
    let [w0, w1, w2] = s;
    let opcode = w0 & 0x1F;
    if opcode == 0 {
        Fetch::Vertex(VertexFetch {
            src: (w0 >> 5) & 0x3F,
            src_rel: (w0 >> 11) & 1 != 0,
            src_comp: (w0 >> 30) & 3,
            dst: (w0 >> 12) & 0x3F,
            dst_rel: (w0 >> 18) & 1 != 0,
            fetch_const: ((w0 >> 20) & 0x1F) * 3 + ((w0 >> 25) & 3),
            dst_swz: swz12(w1 & 0xFFF),
            signed: (w1 >> 12) & 1 != 0,
            integer: (w1 >> 13) & 1 != 0,
            format: (w1 >> 16) & 0x3F,
            exp_adjust: sext((w1 >> 24) & 0x3F, 6),
            mini: (w1 >> 30) & 1 != 0,
            stride: w2 & 0xFF,
            offset: sext((w2 >> 8) & 0x7F_FFFF, 23),
            predicated: w1 >> 31 != 0,
            pred_condition: w2 >> 31 != 0,
        })
    } else {
        Fetch::Texture(TextureFetch {
            opcode,
            src: (w0 >> 5) & 0x3F,
            src_rel: (w0 >> 11) & 1 != 0,
            src_swz: core::array::from_fn(|i| (w0 >> (26 + 2 * i)) & 3),
            dst: (w0 >> 12) & 0x3F,
            dst_rel: (w0 >> 18) & 1 != 0,
            fetch_const: (w0 >> 20) & 0x1F,
            unnormalized: (w0 >> 25) & 1 != 0,
            dst_swz: swz12(w1 & 0xFFF),
            dimension: (w2 >> 14) & 3,
            use_computed_lod: (w1 >> 28) & 1 != 0,
            use_register_lod: (w1 >> 29) & 1 != 0,
            use_register_gradients: w2 & 1 != 0,
            lod_bias: sext((w2 >> 2) & 0x7F, 7) as f32 / 16.0,
            offset: core::array::from_fn(|i| sext((w2 >> (16 + 5 * i)) & 0x1F, 5) as f32 / 2.0),
            predicated: w1 >> 31 != 0,
            pred_condition: w2 >> 31 != 0,
            filters: [(w1 >> 12) & 3, (w1 >> 14) & 3, (w1 >> 16) & 3],
        })
    }
}

// ---- ALU ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    /// Temporary register (else a float constant).
    pub temp: bool,
    /// Register or constant index (temps: 0..63).
    pub index: u32,
    /// Temps: relative to the loop counter `aL`; constants: see `Alu::const_rel`.
    pub relative: bool,
    /// Temps only: absolute value.
    pub abs: bool,
    pub negate: bool,
    /// Source component for each of x, y, z, w.
    pub swizzle: [u32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alu {
    pub vector_op: u32,
    pub scalar_op: u32,
    pub vector_dest: u32,
    pub vector_dest_rel: bool,
    pub scalar_dest: u32,
    pub scalar_dest_rel: bool,
    pub export: bool,
    pub vector_mask: u32,
    pub scalar_mask: u32,
    pub vector_clamp: bool,
    pub scalar_clamp: bool,
    /// Constants take their absolute value.
    pub abs_constants: bool,
    pub src: [Source; 3],
    pub predicated: bool,
    pub pred_condition: bool,
    /// Constant addressing: absolute (else relative to `a0`) for the first and
    /// second constant operand in source order.
    pub const_rel: [bool; 2],
    pub address_absolute: bool,
}

pub fn decode_alu(s: Slot) -> Alu {
    let [w0, w1, w2] = s;
    let src = |n: u32| {
        // n = 1, 2, 3.
        let temp = (w2 >> (32 - n)) & 1 != 0;
        let reg = (w2 >> (8 * (3 - n))) & 0xFF;
        let swz = (w1 >> (8 * (3 - n))) & 0xFF;
        Source {
            temp,
            index: if temp { reg & 0x3F } else { reg },
            relative: temp && reg & 0x40 != 0,
            abs: temp && reg & 0x80 != 0,
            negate: (w1 >> (24 + 3 - n)) & 1 != 0,
            // Swizzles are relative: component i reads (i + s_i) & 3.
            swizzle: core::array::from_fn(|i| (i as u32 + ((swz >> (2 * i)) & 3)) & 3),
        }
    };
    Alu {
        vector_dest: w0 & 0x3F,
        vector_dest_rel: (w0 >> 6) & 1 != 0,
        abs_constants: (w0 >> 7) & 1 != 0,
        scalar_dest: (w0 >> 8) & 0x3F,
        scalar_dest_rel: (w0 >> 14) & 1 != 0,
        export: (w0 >> 15) & 1 != 0,
        vector_mask: (w0 >> 16) & 0xF,
        scalar_mask: (w0 >> 20) & 0xF,
        vector_clamp: (w0 >> 24) & 1 != 0,
        scalar_clamp: (w0 >> 25) & 1 != 0,
        scalar_op: w0 >> 26,
        src: [src(1), src(2), src(3)],
        pred_condition: (w1 >> 27) & 1 != 0,
        predicated: (w1 >> 28) & 1 != 0,
        address_absolute: (w1 >> 29) & 1 != 0,
        const_rel: [(w1 >> 31) & 1 != 0, (w1 >> 30) & 1 != 0],
        vector_op: (w2 >> 24) & 0x1F,
    }
}

pub const VECTOR_OPS: [&str; 30] = [
    "add", "mul", "max", "min", "seq", "sgt", "sge", "sne", "frc", "trunc", "floor", "mad", "cnde", "cndge", "cndgt",
    "dp4", "dp3", "dp2add", "cube", "max4", "setp_eq_push", "setp_ne_push", "setp_gt_push", "setp_ge_push", "kill_eq",
    "kill_gt", "kill_ge", "kill_ne", "dst", "maxa",
];

pub const SCALAR_OPS: [&str; 51] = [
    "adds", "adds_prev", "muls", "muls_prev", "muls_prev2", "maxs", "mins", "seqs", "sgts", "sges", "snes", "frcs",
    "truncs", "floors", "exp", "logc", "log", "rcpc", "rcpf", "rcp", "rsqc", "rsqf", "rsq", "maxas", "maxasf", "subs",
    "subs_prev", "setp_eq", "setp_ne", "setp_gt", "setp_ge", "setp_inv", "setp_pop", "setp_clr", "setp_rstr", "kills_eq",
    "kills_gt", "kills_ge", "kills_ne", "kills_one", "sqrt", "op41", "mulsc0", "mulsc1", "addsc0", "addsc1", "subsc0",
    "subsc1", "sin", "cos", "retain_prev",
];

/// Number of vector operands each vector op reads.
pub fn vector_arity(op: u32) -> usize {
    match op {
        8 | 9 | 10 | 19 => 1,
        11 | 12 | 13 | 14 | 17 => 3,
        18 => 2,
        _ => 2,
    }
}

// ---- Disassembly --------------------------------------------------------------------------

fn swz_str(s: &[u32]) -> String {
    s.iter().map(|&c| ['x', 'y', 'z', 'w'][c as usize]).collect()
}

fn src_str(s: &Source) -> String {
    let base = if s.temp {
        format!("r{}{}", s.index, if s.relative { "[aL]" } else { "" })
    } else {
        format!("c{}", s.index)
    };
    let swz = if s.swizzle == [0, 1, 2, 3] { String::new() } else { format!(".{}", swz_str(&s.swizzle)) };
    let v = format!("{base}{swz}");
    let v = if s.abs { format!("|{v}|") } else { v };
    if s.negate { format!("-{v}") } else { v }
}

fn mask_str(m: u32) -> String {
    (0..4).filter(|i| m & (1 << i) != 0).map(|i| ['x', 'y', 'z', 'w'][i]).collect()
}

pub fn disassemble(code: &[u8], vertex: bool) -> String {
    let s = slots(code);
    let cf = control_flow(&s);
    let mut out = String::new();
    for (n, c) in cf.iter().enumerate() {
        out += &format!("cf{n:<3} {c:?}\n");
        if let Cf::Exec { address, count, sequence, .. } = *c {
            for k in 0..count {
                let a = (address + k) as usize;
                let Some(&slot) = s.get(a) else {
                    out += "      <past the end>\n";
                    continue;
                };
                let fetch = (sequence >> (2 * k)) & 1 != 0;
                out += &format!("  {a:3}  {:08X} {:08X} {:08X}  ", slot[0], slot[1], slot[2]);
                if fetch {
                    out += &format!("{:?}\n", decode_fetch(slot));
                } else {
                    out += &alu_str(&decode_alu(slot), vertex);
                    out.push('\n');
                }
            }
        }
    }
    out
}

pub fn alu_str(a: &Alu, vertex: bool) -> String {
    let dest = |d: u32, rel: bool| {
        if a.export {
            match (vertex, d) {
                (true, 62) => "oPos".to_string(),
                (true, 63) => "oPts".to_string(),
                (false, 61) => "oDepth".to_string(),
                (true, n) => format!("o{n}"),
                (false, n) => format!("oC{n}"),
            }
        } else {
            format!("r{d}{}", if rel { "[aL]" } else { "" })
        }
    };
    let mut s = String::new();
    if a.predicated {
        s += if a.pred_condition { "(p0) " } else { "(!p0) " };
    }
    if a.vector_mask != 0 || (24..=27).contains(&a.vector_op) || (20..=23).contains(&a.vector_op) {
        let ops: Vec<String> = a.src[..vector_arity(a.vector_op)].iter().map(src_str).collect();
        s += &format!(
            "{}{} {}.{}, {}",
            VECTOR_OPS.get(a.vector_op as usize).unwrap_or(&"?"),
            if a.vector_clamp { "_sat" } else { "" },
            dest(a.vector_dest, a.vector_dest_rel),
            mask_str(a.vector_mask),
            ops.join(", ")
        );
    }
    if a.scalar_op != 50 || a.scalar_mask != 0 {
        if !s.is_empty() && !s.ends_with(' ') {
            s += "  +  ";
        }
        // The constant-and-temporary ops (mulsc, addsc, subsc) pack the temporary's
        // index in the opcode's low bit, the src3 select bit and src3 swizzle bits
        // 2-5; the register field is the constant's.
        let operand = if (42..=47).contains(&a.scalar_op) {
            let s3 = &a.src[2];
            let swz = s3.swizzle.iter().enumerate().fold(0u32, |v, (i, &c)| v | ((c + 4 - i as u32) & 3) << (2 * i));
            let reg = (a.scalar_op & 1) | (swz & 0x3C) | ((s3.temp as u32) << 1);
            let creg = if s3.temp { s3.index | (s3.relative as u32) << 6 | (s3.abs as u32) << 7 } else { s3.index };
            let xyzw = ['x', 'y', 'z', 'w'];
            format!(
                "{}c{creg}.{}, r{reg}.{}",
                if s3.negate { "-" } else { "" },
                xyzw[(((swz >> 6) + 3) & 3) as usize],
                xyzw[(swz & 3) as usize]
            )
        } else {
            src_str(&a.src[2])
        };
        s += &format!(
            "{}{} {}.{}, {}",
            SCALAR_OPS.get(a.scalar_op as usize).unwrap_or(&"?"),
            if a.scalar_clamp { "_sat" } else { "" },
            if a.export { dest(a.vector_dest, a.vector_dest_rel) } else { dest(a.scalar_dest, a.scalar_dest_rel) },
            mask_str(a.scalar_mask),
            operand
        );
    }
    s
}
