//! The kernel's `sprintf` family (`sprintf`, `_snprintf`, `_vsnprintf`,
//! `DbgPrint`), formatting from guest arguments.
//!
//! Variadic arguments on the 360 each take an 8-byte slot: the callee spills
//! r3..r10 to the caller's parameter area at `r1 + 0x18 + 8*i`, and further
//! arguments are already there (`r1 + 0x54 + 8*(i-8)` holds the low word of
//! slot `i`). A `va_list` points at the first variadic slot. 32-bit values are
//! the low word of their slot; doubles and 64-bit integers use the whole slot.

use ppc_rt::Ctx;

use crate::mem;

/// Where the arguments come from.
pub enum Args<'a> {
    /// Directly from a variadic call: argument index `next` onward.
    Regs { c: &'a Ctx, next: usize },
    /// A guest `va_list`.
    VaList(u32),
}

impl Args<'_> {
    fn slot(&mut self) -> u64 {
        match self {
            Args::Regs { c, next } => {
                let i = *next;
                *next += 1;
                if i < 8 { c.r[3 + i] } else { mem::r64(c.r[1] as u32 + 0x50 + 8 * (i as u32 - 8)) }
            }
            Args::VaList(p) => {
                let v = mem::r64(*p);
                *p += 8;
                v
            }
        }
    }
    fn u32(&mut self) -> u32 {
        self.slot() as u32
    }
    fn u64(&mut self) -> u64 {
        self.slot()
    }
    fn f64(&mut self) -> f64 {
        f64::from_bits(self.slot())
    }
}

/// Formats `fmt` (guest bytes) into host bytes.
pub fn format(fmt: &[u8], args: &mut Args) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        let ch = fmt[i];
        i += 1;
        if ch != b'%' {
            out.push(ch);
            continue;
        }
        if i < fmt.len() && fmt[i] == b'%' {
            out.push(b'%');
            i += 1;
            continue;
        }
        // Flags.
        let (mut left, mut plus, mut space, mut alt, mut zero) = (false, false, false, false, false);
        while i < fmt.len() {
            match fmt[i] {
                b'-' => left = true,
                b'+' => plus = true,
                b' ' => space = true,
                b'#' => alt = true,
                b'0' => zero = true,
                _ => break,
            }
            i += 1;
        }
        // Width.
        let mut width: Option<usize> = None;
        if i < fmt.len() && fmt[i] == b'*' {
            let w = args.u32() as i32;
            if w < 0 {
                left = true;
            }
            width = Some(w.unsigned_abs() as usize);
            i += 1;
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                width = Some(width.unwrap_or(0) * 10 + (fmt[i] - b'0') as usize);
                i += 1;
            }
        }
        // Precision.
        let mut prec: Option<usize> = None;
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            if i < fmt.len() && fmt[i] == b'*' {
                let p = args.u32() as i32;
                prec = (p >= 0).then_some(p as usize);
                i += 1;
            } else {
                let mut p = 0;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    p = p * 10 + (fmt[i] - b'0') as usize;
                    i += 1;
                }
                prec = Some(p);
            }
        }
        // Length.
        let mut long64 = false;
        let mut short = false;
        let mut wide: Option<bool> = None;
        loop {
            if i >= fmt.len() {
                break;
            }
            match fmt[i] {
                b'l' => {
                    if fmt.get(i + 1) == Some(&b'l') {
                        long64 = true;
                        i += 1;
                    } else {
                        wide = Some(true);
                    }
                }
                b'h' => {
                    short = true;
                    wide = Some(false);
                }
                b'w' => wide = Some(true),
                b'L' | b'q' | b'j' => long64 = true,
                b'z' | b't' => {}
                b'I' => {
                    if fmt[i + 1..].starts_with(b"64") {
                        long64 = true;
                        i += 2;
                    } else if fmt[i + 1..].starts_with(b"32") {
                        i += 2;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else { break };
        i += 1;
        let mut body: Vec<u8>;
        let mut sign: &[u8] = b"";
        let mut numeric = true;
        match conv {
            b'd' | b'i' => {
                let v: i64 = if long64 {
                    args.u64() as i64
                } else if short {
                    args.u32() as i16 as i64
                } else {
                    args.u32() as i32 as i64
                };
                if v < 0 {
                    sign = b"-";
                } else if plus {
                    sign = b"+";
                } else if space {
                    sign = b" ";
                }
                body = v.unsigned_abs().to_string().into_bytes();
                body = apply_int_precision(body, prec);
            }
            b'u' | b'x' | b'X' | b'o' => {
                let v: u64 = if long64 {
                    args.u64()
                } else if short {
                    args.u32() as u16 as u64
                } else {
                    args.u32() as u64
                };
                body = match conv {
                    b'u' => v.to_string(),
                    b'x' => format!("{v:x}"),
                    b'X' => format!("{v:X}"),
                    _ => format!("{v:o}"),
                }
                .into_bytes();
                body = apply_int_precision(body, prec);
                if alt && v != 0 {
                    sign = match conv {
                        b'x' => b"0x",
                        b'X' => b"0X",
                        b'o' => b"0",
                        _ => b"",
                    };
                }
            }
            b'p' => {
                body = format!("{:08X}", args.u32()).into_bytes();
            }
            b'c' | b'C' => {
                numeric = false;
                let v = args.u32();
                let is_wide = wide.unwrap_or(conv == b'C');
                body = if is_wide { char_bytes(v as u16) } else { vec![v as u8] };
            }
            b's' | b'S' => {
                numeric = false;
                let p = args.u32();
                let is_wide = wide.unwrap_or(conv == b'S');
                let max = prec.unwrap_or(usize::MAX);
                body = if p == 0 {
                    b"(null)".to_vec()
                } else if is_wide {
                    mem::wstr(p, max.min(1 << 20)).iter().flat_map(|&u| char_bytes(u)).collect()
                } else {
                    mem::cstr(p, max.min(1 << 20))
                };
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                let v = args.f64();
                if v.is_sign_negative() && !v.is_nan() {
                    sign = b"-";
                } else if plus {
                    sign = b"+";
                } else if space {
                    sign = b" ";
                }
                body = format_float(v.abs(), conv, prec.unwrap_or(6), alt).into_bytes();
                if !v.is_finite() {
                    zero = false;
                }
            }
            b'n' => {
                // Writes the count so far; supported because CRT code uses it.
                let p = args.u32();
                if p != 0 {
                    mem::w32(p, out.len() as u32);
                }
                continue;
            }
            other => {
                // Unknown conversion: print it verbatim, as the CRT does.
                out.push(b'%');
                out.push(other);
                continue;
            }
        }
        let len = sign.len() + body.len();
        let pad = width.unwrap_or(0).saturating_sub(len);
        if left {
            out.extend_from_slice(sign);
            out.extend_from_slice(&body);
            out.extend(std::iter::repeat_n(b' ', pad));
        } else if zero && numeric && (prec.is_none() || !matches!(conv, b'd' | b'i' | b'u' | b'x' | b'X' | b'o')) {
            out.extend_from_slice(sign);
            out.extend(std::iter::repeat_n(b'0', pad));
            out.extend_from_slice(&body);
        } else {
            out.extend(std::iter::repeat_n(b' ', pad));
            out.extend_from_slice(sign);
            out.extend_from_slice(&body);
        }
    }
    out
}

fn apply_int_precision(body: Vec<u8>, prec: Option<usize>) -> Vec<u8> {
    match prec {
        Some(0) if body == b"0" => Vec::new(),
        Some(p) if p > body.len() => {
            let mut v = vec![b'0'; p - body.len()];
            v.extend(body);
            v
        }
        _ => body,
    }
}

/// Wide character to the narrow output (Latin-1, `?` outside it).
fn char_bytes(u: u16) -> Vec<u8> {
    vec![if u < 0x100 { u as u8 } else { b'?' }]
}

fn format_float(v: f64, conv: u8, prec: usize, alt: bool) -> String {
    if v.is_nan() {
        return if conv.is_ascii_uppercase() { "NAN".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        return if conv.is_ascii_uppercase() { "INF".into() } else { "inf".into() };
    }
    let s = match conv {
        b'f' | b'F' => format!("{v:.prec$}"),
        b'e' | b'E' => exp_form(v, prec),
        b'a' | b'A' => format!("{v:e}"),
        _ => {
            // %g: shortest of %e and %f with `prec` significant digits.
            let p = if prec == 0 { 1 } else { prec };
            let exp = if v == 0.0 { 0 } else { v.abs().log10().floor() as i32 };
            let s = if exp < -4 || exp >= p as i32 {
                exp_form(v, p - 1)
            } else {
                format!("{v:.*}", (p as i32 - 1 - exp).max(0) as usize)
            };
            if alt { s } else { strip_zeros(s) }
        }
    };
    if conv.is_ascii_uppercase() { s.to_ascii_uppercase() } else { s }
}

fn exp_form(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$e}");
    // Rust prints `1.5e2`; C prints `1.5e+002` on MSVC.
    let (m, e) = s.split_once('e').unwrap();
    let e: i32 = e.parse().unwrap();
    format!("{m}e{}{:03}", if e < 0 { '-' } else { '+' }, e.abs())
}

fn strip_zeros(s: String) -> String {
    if let Some((m, e)) = s.split_once('e') {
        let m = if m.contains('.') { m.trim_end_matches('0').trim_end_matches('.') } else { m };
        format!("{m}e{e}")
    } else if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}
