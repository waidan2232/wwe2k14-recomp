//! VMX and VMX128 vector operations called by recompiled code.
//!
//! A vector register is a `u128` holding its 16 bytes in guest memory order
//! (big-endian), so guest element 0 is the most significant. Viewing the
//! `u128` as `[T; N]` on a little-endian host gives host lane `i` = guest
//! element `N - 1 - i`. Element-wise operations don't care about the order.
//! Anything that moves elements between positions maps indices with [`lane`].
//!
//! Floating point follows the VMX "non-Java" mode the 360 runs in: denormal
//! inputs count as zero and denormal results are flushed to zero. That is done
//! explicitly here ([`ftz`]), never by changing the host's floating-point
//! control register.
//!
//! The VMX saturation flag (VSCR[SAT]) isn't kept: the game never reads VSCR.

use crate::Cr;

/// Host lane of guest element `k` in an `n`-element view.
#[inline(always)]
pub const fn lane(n: usize, k: usize) -> usize {
    n - 1 - k
}

macro_rules! view {
    ($to:ident, $from:ident, $t:ty, $n:expr) => {
        #[inline(always)]
        #[allow(unnecessary_transmutes)]
        pub fn $to(v: u128) -> [$t; $n] {
            unsafe { core::mem::transmute::<u128, [$t; $n]>(v) }
        }
        #[inline(always)]
        #[allow(unnecessary_transmutes)]
        pub fn $from(a: [$t; $n]) -> u128 {
            unsafe { core::mem::transmute::<[$t; $n], u128>(a) }
        }
    };
}

view!(u8s, from_u8s, u8, 16);
view!(i8s, from_i8s, i8, 16);
view!(u16s, from_u16s, u16, 8);
view!(i16s, from_i16s, i16, 8);
view!(u32s, from_u32s, u32, 4);
view!(i32s, from_i32s, i32, 4);
view!(f32s, from_f32s, f32, 4);

macro_rules! zip {
    ($name:ident, $view:ident, $from:ident, $t:ty, $n:expr) => {
        #[inline(always)]
        fn $name(a: u128, b: u128, f: impl Fn($t, $t) -> $t) -> u128 {
            let (x, y) = ($view(a), $view(b));
            $from(core::array::from_fn::<$t, $n, _>(|i| f(x[i], y[i])))
        }
    };
}

zip!(zip_u8, u8s, from_u8s, u8, 16);
zip!(zip_i8, i8s, from_i8s, i8, 16);
zip!(zip_u16, u16s, from_u16s, u16, 8);
zip!(zip_i16, i16s, from_i16s, i16, 8);
zip!(zip_u32, u32s, from_u32s, u32, 4);
zip!(zip_i32, i32s, from_i32s, i32, 4);

// ---- Floating point -----------------------------------------------------------

/// Flushes a denormal to zero, keeping its sign.
#[inline(always)]
pub fn ftz(x: f32) -> f32 {
    if x.abs() < f32::MIN_POSITIVE { f32::from_bits(x.to_bits() & 0x8000_0000) } else { x }
}

#[inline(always)]
fn map_f(a: u128, f: impl Fn(f32) -> f32) -> u128 {
    from_f32s(f32s(a).map(|x| ftz(f(ftz(x)))))
}

#[inline(always)]
fn zip_f(a: u128, b: u128, f: impl Fn(f32, f32) -> f32) -> u128 {
    let (x, y) = (f32s(a), f32s(b));
    from_f32s(core::array::from_fn(|i| ftz(f(ftz(x[i]), ftz(y[i])))))
}

#[inline(always)]
fn zip3_f(a: u128, b: u128, c: u128, f: impl Fn(f32, f32, f32) -> f32) -> u128 {
    let (x, y, z) = (f32s(a), f32s(b), f32s(c));
    from_f32s(core::array::from_fn(|i| ftz(f(ftz(x[i]), ftz(y[i]), ftz(z[i])))))
}

// The arithmetic below runs four lanes at once with SSE on x86-64. Written
// lane by lane (`zip_f`), LLVM kept each element in its own scalar register
// with a compare-and-blend per flush, about four times the work: Bink 2's
// transform (`sub_835A1030`, 570 vector float ops) then decoded the 30 Years
// of WrestleMania intro at about 20 frames a second, and its sound ran dry
// (user41-44). The lane-by-lane forms stay as the reference
// (`tests::simd_float_matches_lanes`).

#[cfg(target_arch = "x86_64")]
mod sse {
    use core::arch::x86_64::*;

    #[inline(always)]
    pub fn ld(v: u128) -> __m128 {
        unsafe { core::mem::transmute::<u128, __m128>(v) }
    }
    #[inline(always)]
    pub fn st(v: __m128) -> u128 {
        unsafe { core::mem::transmute::<__m128, u128>(v) }
    }
    /// [`super::ftz`] on four lanes: a denormal becomes a zero of its sign.
    #[inline(always)]
    pub fn ftz(x: __m128) -> __m128 {
        unsafe {
            let abs = _mm_castsi128_ps(_mm_set1_epi32(0x7FFF_FFFF));
            let tiny = _mm_cmplt_ps(_mm_and_ps(x, abs), _mm_set1_ps(f32::MIN_POSITIVE));
            _mm_andnot_ps(_mm_and_ps(tiny, abs), x)
        }
    }
    #[inline(always)]
    pub fn zip(a: u128, b: u128, f: impl Fn(__m128, __m128) -> __m128) -> u128 {
        st(ftz(f(ftz(ld(a)), ftz(ld(b)))))
    }
    #[inline(always)]
    pub fn zip3(a: u128, b: u128, c: u128, f: impl Fn(__m128, __m128, __m128) -> __m128) -> u128 {
        st(ftz(f(ftz(ld(a)), ftz(ld(b)), ftz(ld(c)))))
    }
}

#[inline(always)]
pub fn vaddfp(a: u128, b: u128) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return sse::zip(a, b, |x, y| unsafe { core::arch::x86_64::_mm_add_ps(x, y) });
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vaddfp(a, b);
}

#[inline(always)]
pub fn vsubfp(a: u128, b: u128) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return sse::zip(a, b, |x, y| unsafe { core::arch::x86_64::_mm_sub_ps(x, y) });
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vsubfp(a, b);
}

#[inline(always)]
pub fn vmulfp(a: u128, b: u128) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return sse::zip(a, b, |x, y| unsafe { core::arch::x86_64::_mm_mul_ps(x, y) });
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vmulfp(a, b);
}

/// `a * c + b`. Unlike the scalar FPU's `fmadd`, this isn't fused: the
/// product is rounded to single first. The test vectors expect
/// `1.1 * 1.1 + 1.1` = 0x4013D70A, not the fused 0x4013D70B.
#[inline(always)]
pub fn vmaddfp(a: u128, c: u128, b: u128) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return sse::zip3(a, c, b, |x, y, z| unsafe { core::arch::x86_64::_mm_add_ps(core::arch::x86_64::_mm_mul_ps(x, y), z) });
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vmaddfp(a, c, b);
}

/// `-(a * c - b)`, not fused (see [`vmaddfp`]).
#[inline(always)]
pub fn vnmsubfp(a: u128, c: u128, b: u128) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return sse::zip3(a, c, b, |x, y, z| unsafe {
        use core::arch::x86_64::*;
        _mm_xor_ps(_mm_sub_ps(_mm_mul_ps(x, y), z), _mm_set1_ps(-0.0))
    });
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vnmsubfp(a, c, b);
}

/// The lane-by-lane definitions: the fallback off x86-64, and the reference
/// the four-lane forms are tested against.
#[allow(dead_code)]
mod lanes {
    use super::*;
    pub fn vaddfp(a: u128, b: u128) -> u128 {
        zip_f(a, b, |x, y| x + y)
    }
    pub fn vsubfp(a: u128, b: u128) -> u128 {
        zip_f(a, b, |x, y| x - y)
    }
    pub fn vmulfp(a: u128, b: u128) -> u128 {
        zip_f(a, b, |x, y| x * y)
    }
    pub fn vmaddfp(a: u128, c: u128, b: u128) -> u128 {
        zip3_f(a, c, b, |x, y, z| x * y + z)
    }
    pub fn vnmsubfp(a: u128, c: u128, b: u128) -> u128 {
        zip3_f(a, c, b, |x, y, z| -(x * y - z))
    }
    pub fn vcfsx(b: u128, scale: u32) -> u128 {
        let div = (2.0f32).powi(-(scale as i32));
        from_f32s(i32s(b).map(|x| ftz(x as f32 * div)))
    }
    pub fn vctsxs(b: u128, scale: u32) -> u128 {
        let mul = (2.0f32).powi(scale as i32);
        from_i32s(f32s(b).map(|x| (ftz(x) * mul) as i32))
    }
}

#[inline(always)]
fn quiet(x: f32) -> f32 {
    f32::from_bits(x.to_bits() | 0x0040_0000)
}

/// VMX max: a NaN operand propagates (quieted); +0 is greater than -0.
#[inline(always)]
pub fn vmaxfp(a: u128, b: u128) -> u128 {
    zip_f(a, b, |x, y| {
        if x.is_nan() {
            quiet(x)
        } else if y.is_nan() {
            quiet(y)
        } else if x == y {
            f32::from_bits(x.to_bits() & y.to_bits())
        } else if x > y {
            x
        } else {
            y
        }
    })
}

/// VMX min: a NaN operand propagates (quieted); -0 is less than +0.
#[inline(always)]
pub fn vminfp(a: u128, b: u128) -> u128 {
    zip_f(a, b, |x, y| {
        if x.is_nan() {
            quiet(x)
        } else if y.is_nan() {
            quiet(y)
        } else if x == y {
            f32::from_bits(x.to_bits() | y.to_bits())
        } else if x < y {
            x
        } else {
            y
        }
    })
}

/// Reciprocal estimate. The 360's estimate has 12 bits of precision; exact is
/// within that and code refines it anyway.
#[inline(always)]
pub fn vrefp(a: u128) -> u128 {
    map_f(a, |x| 1.0 / x)
}

#[inline(always)]
pub fn vrsqrtefp(a: u128) -> u128 {
    map_f(a, |x| 1.0 / x.sqrt())
}

#[inline(always)]
pub fn vexptefp(a: u128) -> u128 {
    map_f(a, f32::exp2)
}

#[inline(always)]
pub fn vlogefp(a: u128) -> u128 {
    map_f(a, f32::log2)
}

#[inline(always)]
pub fn vrfin(a: u128) -> u128 {
    map_f(a, f32::round_ties_even)
}

#[inline(always)]
pub fn vrfiz(a: u128) -> u128 {
    map_f(a, f32::trunc)
}

#[inline(always)]
pub fn vrfip(a: u128) -> u128 {
    map_f(a, f32::ceil)
}

#[inline(always)]
pub fn vrfim(a: u128) -> u128 {
    map_f(a, f32::floor)
}

/// Signed words to float, divided by `2^scale`.
#[inline(always)]
pub fn vcfsx(b: u128, scale: u32) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return unsafe {
        use core::arch::x86_64::*;
        let x = _mm_cvtepi32_ps(core::mem::transmute::<u128, __m128i>(b));
        sse::st(sse::ftz(_mm_mul_ps(x, _mm_set1_ps((2.0f32).powi(-(scale as i32))))))
    };
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vcfsx(b, scale);
}

#[inline(always)]
pub fn vcfux(b: u128, scale: u32) -> u128 {
    let div = (2.0f32).powi(-(scale as i32));
    from_f32s(u32s(b).map(|x| ftz(x as f32 * div)))
}

/// Float times `2^scale` to signed words, saturating; NaN gives 0.
#[inline(always)]
pub fn vctsxs(b: u128, scale: u32) -> u128 {
    #[cfg(target_arch = "x86_64")]
    return unsafe {
        use core::arch::x86_64::*;
        let t = _mm_mul_ps(sse::ftz(sse::ld(b)), _mm_set1_ps((2.0f32).powi(scale as i32)));
        // cvttps gives 0x80000000 for anything out of range and for NaN:
        // too large becomes 0x7FFFFFFF, NaN becomes 0 (too small is right).
        let c = _mm_cvttps_epi32(t);
        let big = _mm_castps_si128(_mm_cmpge_ps(t, _mm_set1_ps(2_147_483_648.0)));
        let ordered = _mm_castps_si128(_mm_cmpord_ps(t, t));
        core::mem::transmute::<__m128i, u128>(_mm_and_si128(_mm_xor_si128(c, big), ordered))
    };
    #[cfg(not(target_arch = "x86_64"))]
    return lanes::vctsxs(b, scale);
}

#[inline(always)]
pub fn vctuxs(b: u128, scale: u32) -> u128 {
    let mul = (2.0f32).powi(scale as i32);
    from_u32s(f32s(b).map(|x| (ftz(x) * mul) as u32))
}

/// Three-element dot product of x, y, z, written to every element.
#[inline(always)]
pub fn vmsum3fp(a: u128, b: u128) -> u128 {
    let (x, y) = (f32s(a).map(ftz), f32s(b).map(ftz));
    let (g0, g1, g2) = (lane(4, 0), lane(4, 1), lane(4, 2));
    let d = ftz(x[g0] * y[g0] + x[g1] * y[g1] + x[g2] * y[g2]);
    from_f32s([d; 4])
}

/// Four-element dot product, written to every element.
#[inline(always)]
pub fn vmsum4fp(a: u128, b: u128) -> u128 {
    let (x, y) = (f32s(a).map(ftz), f32s(b).map(ftz));
    let (g0, g1, g2, g3) = (lane(4, 0), lane(4, 1), lane(4, 2), lane(4, 3));
    let d = ftz(x[g0] * y[g0] + x[g1] * y[g1] + x[g2] * y[g2] + x[g3] * y[g3]);
    from_f32s([d; 4])
}

// ---- Compares -----------------------------------------------------------------

/// CR6 after a vector compare with the record bit: LT = every element true,
/// EQ = every element false.
#[inline(always)]
pub fn cr6(mask: u128) -> Cr {
    Cr { lt: mask == u128::MAX, gt: false, eq: mask == 0, so: false }
}

#[inline(always)]
fn cmp_f(a: u128, b: u128, f: impl Fn(f32, f32) -> bool) -> u128 {
    let (x, y) = (f32s(a), f32s(b));
    from_u32s(core::array::from_fn(|i| if f(ftz(x[i]), ftz(y[i])) { u32::MAX } else { 0 }))
}

#[inline(always)]
pub fn vcmpeqfp(a: u128, b: u128) -> u128 {
    cmp_f(a, b, |x, y| x == y)
}

#[inline(always)]
pub fn vcmpgefp(a: u128, b: u128) -> u128 {
    cmp_f(a, b, |x, y| x >= y)
}

#[inline(always)]
pub fn vcmpgtfp(a: u128, b: u128) -> u128 {
    cmp_f(a, b, |x, y| x > y)
}

/// Bounds compare: bit 31 set if `a > b`, bit 30 set if `a < -b` (both for NaN).
#[inline(always)]
pub fn vcmpbfp(a: u128, b: u128) -> u128 {
    let (x, y) = (f32s(a), f32s(b));
    from_u32s(core::array::from_fn(|i| {
        let (x, y) = (ftz(x[i]), ftz(y[i]));
        let nan = x.is_nan() || y.is_nan();
        (((nan || !(x <= y)) as u32) << 31) | (((nan || !(x >= -y)) as u32) << 30)
    }))
}

/// CR6 after `vcmpbfp.`: EQ = every element within bounds.
#[inline(always)]
pub fn cr6_bounds(mask: u128) -> Cr {
    Cr { lt: false, gt: false, eq: mask == 0, so: false }
}

macro_rules! int_cmp {
    ($name:ident, $view:ident, $uview:ident, $from:ident, $t:ty, $u:ty, $n:expr, $op:tt) => {
        #[inline(always)]
        pub fn $name(a: u128, b: u128) -> u128 {
            let (x, y) = ($view(a), $view(b));
            $from(core::array::from_fn::<$u, $n, _>(|i| if x[i] $op y[i] { <$u>::MAX } else { 0 }))
        }
    };
}

int_cmp!(vcmpequb, u8s, u8s, from_u8s, u8, u8, 16, ==);
int_cmp!(vcmpequh, u16s, u16s, from_u16s, u16, u16, 8, ==);
int_cmp!(vcmpequw, u32s, u32s, from_u32s, u32, u32, 4, ==);
int_cmp!(vcmpgtub, u8s, u8s, from_u8s, u8, u8, 16, >);
int_cmp!(vcmpgtuh, u16s, u16s, from_u16s, u16, u16, 8, >);
int_cmp!(vcmpgtuw, u32s, u32s, from_u32s, u32, u32, 4, >);
int_cmp!(vcmpgtsb, i8s, u8s, from_u8s, i8, u8, 16, >);
int_cmp!(vcmpgtsh, i16s, u16s, from_u16s, i16, u16, 8, >);
int_cmp!(vcmpgtsw, i32s, u32s, from_u32s, i32, u32, 4, >);

// ---- Integer arithmetic -------------------------------------------------------

macro_rules! int_ops {
    ($zip:ident, $t:ty, $wide:ty, $add:ident, $adds:ident, $sub:ident, $subs:ident, $avg:ident, $max:ident, $min:ident) => {
        #[inline(always)]
        pub fn $add(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.wrapping_add(y))
        }
        #[inline(always)]
        pub fn $adds(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.saturating_add(y))
        }
        #[inline(always)]
        pub fn $sub(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.wrapping_sub(y))
        }
        #[inline(always)]
        pub fn $subs(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.saturating_sub(y))
        }
        #[inline(always)]
        pub fn $avg(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| ((x as $wide + y as $wide + 1) >> 1) as $t)
        }
        #[inline(always)]
        pub fn $max(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.max(y))
        }
        #[inline(always)]
        pub fn $min(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.min(y))
        }
    };
}

int_ops!(zip_u8, u8, u16, vaddubm, vaddubs, vsububm, vsububs, vavgub, vmaxub, vminub);
int_ops!(zip_u16, u16, u32, vadduhm, vadduhs, vsubuhm, vsubuhs, vavguh, vmaxuh, vminuh);
int_ops!(zip_u32, u32, u64, vadduwm, vadduws, vsubuwm, vsubuws, vavguw, vmaxuw, vminuw);

macro_rules! signed_ops {
    ($zip:ident, $t:ty, $wide:ty, $adds:ident, $subs:ident, $avg:ident, $max:ident, $min:ident) => {
        #[inline(always)]
        pub fn $adds(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.saturating_add(y))
        }
        #[inline(always)]
        pub fn $subs(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.saturating_sub(y))
        }
        #[inline(always)]
        pub fn $avg(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| ((x as $wide + y as $wide + 1) >> 1) as $t)
        }
        #[inline(always)]
        pub fn $max(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.max(y))
        }
        #[inline(always)]
        pub fn $min(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.min(y))
        }
    };
}

signed_ops!(zip_i8, i8, i16, vaddsbs, vsubsbs, vavgsb, vmaxsb, vminsb);
signed_ops!(zip_i16, i16, i32, vaddshs, vsubshs, vavgsh, vmaxsh, vminsh);
signed_ops!(zip_i32, i32, i64, vaddsws, vsubsws, vavgsw, vmaxsw, vminsw);

/// Carry out of each word addition.
#[inline(always)]
pub fn vaddcuw(a: u128, b: u128) -> u128 {
    zip_u32(a, b, |x, y| x.overflowing_add(y).1 as u32)
}

/// 1 where the word subtraction doesn't borrow.
#[inline(always)]
pub fn vsubcuw(a: u128, b: u128) -> u128 {
    zip_u32(a, b, |x, y| (x >= y) as u32)
}

// ---- Shifts and rotates (per element, count from the matching element of b) ----

macro_rules! shifts {
    ($zip:ident, $szip:ident, $t:ty, $s:ty, $bits:expr, $sl:ident, $sr:ident, $sra:ident, $rl:ident) => {
        #[inline(always)]
        pub fn $sl(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x << (y as u32 % $bits))
        }
        #[inline(always)]
        pub fn $sr(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x >> (y as u32 % $bits))
        }
        #[inline(always)]
        pub fn $sra(a: u128, b: u128) -> u128 {
            $szip(a, b, |x, y| x >> (y as u32 % $bits))
        }
        #[inline(always)]
        pub fn $rl(a: u128, b: u128) -> u128 {
            $zip(a, b, |x, y| x.rotate_left(y as u32 % $bits))
        }
    };
}

shifts!(zip_u8, zip_i8, u8, i8, 8, vslb, vsrb, vsrab, vrlb);
shifts!(zip_u16, zip_i16, u16, i16, 16, vslh, vsrh, vsrah, vrlh);
shifts!(zip_u32, zip_i32, u32, i32, 32, vslw, vsrw, vsraw, vrlw);

/// Whole-register shift left by `b`'s last byte & 7 bits.
#[inline(always)]
pub fn vsl(a: u128, b: u128) -> u128 {
    a << (b as u32 & 7)
}

#[inline(always)]
pub fn vsr(a: u128, b: u128) -> u128 {
    a >> (b as u32 & 7)
}

/// Whole-register shift left by octets: `(b's last byte >> 3) & 15` bytes.
#[inline(always)]
pub fn vslo(a: u128, b: u128) -> u128 {
    let n = (b as u32 >> 3) & 15;
    if n == 0 { a } else { a << (8 * n) }
}

#[inline(always)]
pub fn vsro(a: u128, b: u128) -> u128 {
    let n = (b as u32 >> 3) & 15;
    if n == 0 { a } else { a >> (8 * n) }
}

/// Bytes `sh..sh+16` of the 32-byte concatenation `a || b`.
#[inline(always)]
pub fn vsldoi(a: u128, b: u128, sh: u32) -> u128 {
    if sh == 0 { a } else { (a << (8 * sh)) | (b >> (128 - 8 * sh)) }
}

// ---- Permutes, merges, splats -----------------------------------------------------

/// `vperm`: each byte of `c` (low 5 bits) picks a byte of `a || b`.
///
/// The control is data, so written byte by byte this is sixteen dependent
/// table lookups. Where the build targets a CPU with SSSE3 and SSE4.1 (the
/// game's does: x86-64-v3) it is two byte shuffles and a blend instead.
#[inline(always)]
pub fn vperm(a: u128, b: u128, c: u128) -> u128 {
    #[cfg(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "sse4.1"))]
    {
        // The features are part of the build's target.
        unsafe { vperm_shuffle(a, b, c) }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "sse4.1")))]
    {
        vperm_bytes(a, b, c)
    }
}

/// `vperm`, one byte at a time.
#[inline(always)]
pub fn vperm_bytes(a: u128, b: u128, c: u128) -> u128 {
    let (x, y, sel) = (a.to_be_bytes(), b.to_be_bytes(), c.to_be_bytes());
    u128::from_be_bytes(core::array::from_fn(|i| {
        let k = (sel[i] & 31) as usize;
        if k < 16 { x[k] } else { y[k - 16] }
    }))
}

/// `vperm` with `pshufb`.
///
/// Host byte `j` of a register is guest byte `15 - j`. A control byte `k`
/// below 16 wants guest byte `k` of `a`, which is host byte `15 - k`; one from
/// 16 up wants guest byte `k - 16` of `b`, host byte `31 - k`. Both are
/// `!k & 15`, so one shuffle index serves both sources, and bit 4 of the
/// control picks between the two results.
///
/// # Safety
/// The CPU must have SSSE3 and SSE4.1.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "ssse3,sse4.1")]
pub unsafe fn vperm_shuffle(a: u128, b: u128, c: u128) -> u128 {
    use core::arch::x86_64::*;
    unsafe {
        let (a, b, c): (__m128i, __m128i, __m128i) = (core::mem::transmute(a), core::mem::transmute(b), core::mem::transmute(c));
        let index = _mm_andnot_si128(c, _mm_set1_epi8(0x0F));
        let from_a = _mm_shuffle_epi8(a, index);
        let from_b = _mm_shuffle_epi8(b, index);
        // Bit 4 of each control byte, moved to the byte's top bit for the blend.
        let take_b = _mm_slli_epi16(c, 3);
        core::mem::transmute(_mm_blendv_epi8(from_a, from_b, take_b))
    }
}

/// `(b & c) | (a & !c)`.
#[inline(always)]
pub fn vsel(a: u128, b: u128, c: u128) -> u128 {
    (b & c) | (a & !c)
}

macro_rules! merge {
    ($name:ident, $view:ident, $from:ident, $t:ty, $n:expr, $base:expr) => {
        #[inline(always)]
        pub fn $name(a: u128, b: u128) -> u128 {
            let (x, y) = ($view(a), $view(b));
            let mut out = [0 as $t; $n];
            for k in 0..$n / 2 {
                out[lane($n, 2 * k)] = x[lane($n, $base + k)];
                out[lane($n, 2 * k + 1)] = y[lane($n, $base + k)];
            }
            $from(out)
        }
    };
}

merge!(vmrghb, u8s, from_u8s, u8, 16, 0);
merge!(vmrglb, u8s, from_u8s, u8, 16, 8);
merge!(vmrghh, u16s, from_u16s, u16, 8, 0);
merge!(vmrglh, u16s, from_u16s, u16, 8, 4);
merge!(vmrghw, u32s, from_u32s, u32, 4, 0);
merge!(vmrglw, u32s, from_u32s, u32, 4, 2);

#[inline(always)]
pub fn vspltb(b: u128, k: u32) -> u128 {
    from_u8s([u8s(b)[lane(16, k as usize & 15)]; 16])
}

#[inline(always)]
pub fn vsplth(b: u128, k: u32) -> u128 {
    from_u16s([u16s(b)[lane(8, k as usize & 7)]; 8])
}

#[inline(always)]
pub fn vspltw(b: u128, k: u32) -> u128 {
    from_u32s([u32s(b)[lane(4, k as usize & 3)]; 4])
}

#[inline(always)]
pub fn vspltisb(v: i32) -> u128 {
    from_u8s([v as u8; 16])
}

#[inline(always)]
pub fn vspltish(v: i32) -> u128 {
    from_u16s([v as u16; 8])
}

#[inline(always)]
pub fn vspltisw(v: i32) -> u128 {
    from_u32s([v as u32; 4])
}

/// `vpermwi128`: word `k` of the result is word `(imm >> (6 - 2k)) & 3` of `b`.
#[inline(always)]
pub fn vpermwi(b: u128, imm: u32) -> u128 {
    let x = u32s(b);
    let mut out = [0u32; 4];
    for k in 0..4 {
        out[lane(4, k)] = x[lane(4, ((imm >> (6 - 2 * k)) & 3) as usize)];
    }
    from_u32s(out)
}

/// `vrlimi128`: rotate `b` left by `rot` words, then take the words selected
/// by `mask` (bit 3 = word 0) from it and the rest from `d`.
#[inline(always)]
pub fn vrlimi(d: u128, b: u128, mask: u32, rot: u32) -> u128 {
    let rotated = if rot & 3 == 0 { b } else { b.rotate_left(32 * (rot & 3)) };
    let mut m = 0u128;
    for k in 0..4 {
        if mask & (8 >> k) != 0 {
            m |= (u32::MAX as u128) << (96 - 32 * k);
        }
    }
    (rotated & m) | (d & !m)
}

// ---- Unpack and pack ------------------------------------------------------------

macro_rules! unpack {
    ($name:ident, $view:ident, $from:ident, $src:ty, $dst:ty, $n:expr, $base:expr) => {
        #[inline(always)]
        pub fn $name(b: u128) -> u128 {
            let x = $view(b);
            let mut out = [0 as $dst; $n / 2];
            for k in 0..$n / 2 {
                out[lane($n / 2, k)] = x[lane($n, $base + k)] as $dst;
            }
            $from(out)
        }
    };
}

unpack!(vupkhsb, i8s, from_i16s, i8, i16, 16, 0);
unpack!(vupklsb, i8s, from_i16s, i8, i16, 16, 8);
unpack!(vupkhsh, i16s, from_i32s, i16, i32, 8, 0);
unpack!(vupklsh, i16s, from_i32s, i16, i32, 8, 4);

macro_rules! pack {
    ($name:ident, $view:ident, $from:ident, $src:ty, $dst:ty, $n:expr, $conv:expr) => {
        #[inline(always)]
        pub fn $name(a: u128, b: u128) -> u128 {
            let (x, y) = ($view(a), $view(b));
            let conv: fn($src) -> $dst = $conv;
            let mut out = [0 as $dst; 2 * $n];
            for k in 0..$n {
                out[lane(2 * $n, k)] = conv(x[lane($n, k)]);
                out[lane(2 * $n, $n + k)] = conv(y[lane($n, k)]);
            }
            $from(out)
        }
    };
}

pack!(vpkuhum, u16s, from_u8s, u16, u8, 8, |v| v as u8);
pack!(vpkuwum, u32s, from_u16s, u32, u16, 4, |v| v as u16);
pack!(vpkuhus, u16s, from_u8s, u16, u8, 8, |v| v.min(u8::MAX as u16) as u8);
pack!(vpkuwus, u32s, from_u16s, u32, u16, 4, |v| v.min(u16::MAX as u32) as u16);
pack!(vpkshss, i16s, from_i8s, i16, i8, 8, |v| v.clamp(i8::MIN as i16, i8::MAX as i16) as i8);
pack!(vpkswss, i32s, from_i16s, i32, i16, 4, |v| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
pack!(vpkshus, i16s, from_u8s, i16, u8, 8, |v| v.clamp(0, u8::MAX as i16) as u8);
pack!(vpkswus, i32s, from_u16s, i32, u16, 4, |v| v.clamp(0, u16::MAX as i32) as u16);

// ---- D3D pack/unpack (VMX128) ------------------------------------------------------
//
// The 360's vertex-format conversions. Integer formats travel as floats biased
// so the integer sits in the low mantissa bits: 3.0 + n * 2^-22 for signed
// fields, 1.0 + n * 2^-23 for the unsigned colour and 2-bit fields. The test
// vectors in tools/kat pin down the clamping, NaN and placement behaviour.

const THREE: u32 = 0x4040_0000;
const ONE: u32 = 0x3F80_0000;

/// Clamps `x` to `[lo, hi]` (bit patterns of floats); NaN goes to `lo`.
#[inline(always)]
fn clamp_bits(x: f32, lo: u32, hi: u32) -> u32 {
    let (lo_f, hi_f) = (f32::from_bits(lo), f32::from_bits(hi));
    if x >= lo_f {
        if x > hi_f { hi } else { x.to_bits() }
    } else {
        lo
    }
}

/// Float to the 360's half format: truncating, no infinities (overflow and
/// NaN become the largest magnitude, 0x7FFF).
#[inline(always)]
pub fn f32_to_half(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let abs = bits & 0x7FFF_FFFF;
    let mag = f32::from_bits(abs);
    if mag.is_nan() || mag > 65504.0 {
        return sign | 0x7FFF;
    }
    let exp = (abs >> 23) as i32;
    let man = ((abs >> 13) & 0x3FF) as u16;
    if exp > 0x70 {
        sign | ((((exp - 0x70) as u16) << 10) + man)
    } else {
        let shift = 0x71 - exp;
        if shift > 31 { sign } else { sign | (((0x400 + man as u32) >> shift) as u16) }
    }
}

/// The 360's half format to float: exponent 31 is an ordinary exponent.
#[inline(always)]
pub fn half_to_f32(h: u16) -> f32 {
    let sign = ((h as u32) & 0x8000) << 16;
    let exp = ((h >> 10) & 0x1F) as u32;
    let man = (h & 0x3FF) as u32;
    let bits = if exp == 0 {
        if man == 0 {
            0
        } else {
            // Denormal half: normalise.
            let shift = man.leading_zeros() - 21;
            let man = (man << shift) & 0x3FF;
            ((127 - 15 + 1 - shift) << 23) | (man << 13)
        }
    } else {
        ((exp + 127 - 15) << 23) | (man << 13)
    };
    f32::from_bits(sign | bits)
}

/// Signed 16-bit field from a 3.0-biased float, clamped to +-32767.
#[inline(always)]
fn pack_s16(x: f32) -> u32 {
    clamp_bits(x, THREE - 0x7FFF, THREE + 0x7FFF).wrapping_sub(THREE) & 0xFFFF
}

/// `vpkd3d128` conversion: the packed bits, as a 64-bit value (high word zero
/// for 32-bit formats). `None` for formats the game doesn't use.
#[inline(always)]
fn pack_d3d(b: u128, kind: u32) -> Option<u64> {
    let f = f32s(b);
    let g = |k: usize| f[lane(4, k)];
    Some(match kind {
        // D3DCOLOR: x, y, z, w = R, G, B, A, each 3.0-biased; stored as ARGB bytes.
        0 => {
            let byte = |x: f32| clamp_bits(x, THREE, THREE + 0xFF) & 0xFF;
            ((byte(g(3)) << 24) | (byte(g(0)) << 16) | (byte(g(1)) << 8) | byte(g(2))) as u64
        }
        // Two signed shorts.
        1 => ((pack_s16(g(0)) << 16) | pack_s16(g(1))) as u64,
        // 10:10:10 signed + 2-bit unsigned, x in the low bits.
        2 => {
            let s10 = |x: f32| clamp_bits(x, THREE - 0x1FF, THREE + 0x1FF).wrapping_sub(THREE) & 0x3FF;
            let w = clamp_bits(g(3), ONE, ONE + 3).wrapping_sub(ONE) & 3;
            ((w << 30) | (s10(g(2)) << 20) | (s10(g(1)) << 10) | s10(g(0))) as u64
        }
        // Two halves.
        3 => (((f32_to_half(g(0)) as u32) << 16) | f32_to_half(g(1)) as u32) as u64,
        // Four signed shorts.
        4 => {
            let hi = (pack_s16(g(0)) << 16) | pack_s16(g(1));
            let lo = (pack_s16(g(2)) << 16) | pack_s16(g(3));
            ((hi as u64) << 32) | lo as u64
        }
        // Four halves.
        5 => {
            let h = |k: usize| f32_to_half(g(k)) as u64;
            (h(0) << 48) | (h(1) << 32) | (h(2) << 16) | h(3)
        }
        _ => return None,
    })
}

/// `vpkd3d128 vD, vB, kind, pack, shift`: converts `b` and inserts the result
/// into `d`. `pack` 1 inserts one word, 2 and 3 two words, at the right end of
/// the register shifted left by `shift` words; pack 3 with shift 3 inserts the
/// high word into word 3 instead.
#[inline(always)]
pub fn vpkd3d(d: u128, b: u128, kind: u32, pack: u32, shift: u32) -> u128 {
    // The recompiler only emits formats 0-5.
    let Some(p) = pack_d3d(b, kind) else { panic!("FATAL: vpkd3d128 format {kind} is not implemented") };
    let (value, mask) = match (pack, shift) {
        (3, 3) => ((p >> 32) as u128, u32::MAX as u128),
        (1, s) => ((p as u32 as u128) << (32 * s), (u32::MAX as u128) << (32 * s)),
        (_, s) => ((p as u128) << (32 * s), (u64::MAX as u128) << (32 * s)),
    };
    (value & mask) | (d & !mask)
}

/// Signed field from a biased float: the most negative value is NaN.
#[inline(always)]
fn unpack_signed(v: i32, min: i32) -> f32 {
    if v == min { f32::from_bits(0x7FC0_0000) } else { f32::from_bits(THREE.wrapping_add(v as u32)) }
}

/// `vupkd3d128 vD, vB, kind`: expands the packed data at the right end of `b`.
#[inline(always)]
pub fn vupkd3d(b: u128, kind: u32) -> u128 {
    let w3 = b as u32;
    let w2 = (b >> 32) as u32;
    let s16 = |v: u32| unpack_signed(v as u16 as i16 as i32, -32768);
    let out: [f32; 4] = match kind {
        0 => {
            let byte = |sh: u32| f32::from_bits(ONE | ((w3 >> sh) & 0xFF));
            [byte(16), byte(8), byte(0), byte(24)]
        }
        1 => [s16(w3 >> 16), s16(w3), 0.0, 1.0],
        2 => {
            let s10 = |sh: u32| unpack_signed((((w3 >> sh) & 0x3FF) << 22) as i32 >> 22, -512);
            [s10(0), s10(10), s10(20), f32::from_bits(ONE | (w3 >> 30))]
        }
        3 => [half_to_f32((w3 >> 16) as u16), half_to_f32(w3 as u16), 0.0, 1.0],
        4 => [s16(w2 >> 16), s16(w2), s16(w3 >> 16), s16(w3)],
        5 => [
            half_to_f32((w2 >> 16) as u16),
            half_to_f32(w2 as u16),
            half_to_f32((w3 >> 16) as u16),
            half_to_f32(w3 as u16),
        ],
        // The recompiler only emits formats 0-5.
        _ => panic!("FATAL: vupkd3d128 format {kind} is not implemented"),
    };
    // Guest element order to host lanes.
    from_f32s([out[3], out[2], out[1], out[0]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four-lane float operations give the lane-by-lane results bit for
    /// bit, over random words and edge values (denormals, zeros, infinities,
    /// NaNs, the conversion limits).
    #[test]
    fn simd_float_matches_lanes() {
        const EDGE: [u32; 16] = [
            0, 0x8000_0000, 1, 0x8000_0001, 0x007F_FFFF, 0x807F_FFFF, 0x0080_0000, 0x8080_0000,
            0x7F80_0000, 0xFF80_0000, 0x7FC0_0000, 0xFFC0_0001, 0x7F80_0001, 0x4F00_0000, 0xCF00_0000, 0x3F80_0000,
        ];
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut word = |pick: bool| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            if pick { EDGE[(s >> 40) as usize % 16] } else { s as u32 }
        };
        for i in 0..400_000usize {
            let mut v = |j: usize| from_u32s(core::array::from_fn(|k| word((i + j + k) % 3 == 0)));
            let (a, b, c) = (v(0), v(1), v(2));
            let k = (i % 32) as u32;
            assert_eq!(vaddfp(a, b), lanes::vaddfp(a, b), "vaddfp {a:032X} {b:032X}");
            assert_eq!(vsubfp(a, b), lanes::vsubfp(a, b), "vsubfp {a:032X} {b:032X}");
            assert_eq!(vmulfp(a, b), lanes::vmulfp(a, b), "vmulfp {a:032X} {b:032X}");
            assert_eq!(vmaddfp(a, b, c), lanes::vmaddfp(a, b, c), "vmaddfp {a:032X} {b:032X} {c:032X}");
            assert_eq!(vnmsubfp(a, b, c), lanes::vnmsubfp(a, b, c), "vnmsubfp {a:032X} {b:032X} {c:032X}");
            assert_eq!(vcfsx(a, k), lanes::vcfsx(a, k), "vcfsx {a:032X} {k}");
            assert_eq!(vctsxs(a, k), lanes::vctsxs(a, k), "vctsxs {a:032X} {k}");
        }
    }
}
