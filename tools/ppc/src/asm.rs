//! A small assembler, the inverse of the disassembler.
//!
//! It is driven by the same opcode table as the decoder: each table entry's
//! disassembly template gives the mnemonic (with its `.`/`o`/`l`/`a` variants)
//! and the operand order. Common extended mnemonics (`li`, `mr`, `slwi`,
//! `beq`, `blr`, `cmpwi`, `twgti`, ...) are rewritten to their base forms.
//!
//! Every word it produces is decoded again and each operand is read back, so
//! an out-of-range value or a field the encoder got wrong is an error rather
//! than a silently different instruction.
//!
//! Used for known-answer tests and for config files that name instructions.
//! It translates text to machine words; it never executes anything.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::{Form, Instr, OPS, Op};

/// One way to spell an instruction: the table entry plus the flag bits
/// (`.`, `o`, `l`, `a`) the spelling sets.
#[derive(Clone, Debug)]
struct Spelling {
    op: Op,
    flags: u32,
    /// Operand field names in written order, e.g. `["RD", "d", "RA0"]`.
    operands: Vec<&'static str>,
}

fn template_operands(t: &'static str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(open) = rest.find('[') {
        let Some(len) = rest[open..].find(']') else { break };
        out.push(&rest[open + 1..open + len]);
        rest = &rest[open + len + 1..];
    }
    out
}

fn rc_bit(form: Form) -> Option<u32> {
    match form {
        Form::X | Form::XO | Form::A | Form::M | Form::MD | Form::MDS | Form::XS | Form::XFL => Some(1),
        Form::VC => Some(1 << 10),
        Form::VX128_R => Some(1 << 6),
        _ => None,
    }
}

fn spellings() -> &'static HashMap<String, Vec<Spelling>> {
    static TABLE: OnceLock<HashMap<String, Vec<Spelling>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map: HashMap<String, Vec<Spelling>> = HashMap::new();
        for info in OPS.iter() {
            let Some(t) = info.disasm else { continue };
            let (head, ops) = t.split_once(' ').unwrap_or((t, ""));
            let operands = template_operands(ops);
            let base: String = head.split('[').next().unwrap().to_string();
            let flag_names: Vec<&str> = template_operands(head);
            // Every combination of the optional flags, in template order.
            for combo in 0..(1u32 << flag_names.len()) {
                let mut name = base.clone();
                let mut flags = 0u32;
                let mut ok = true;
                for (k, f) in flag_names.iter().enumerate() {
                    if combo & (1 << k) == 0 {
                        continue;
                    }
                    let (suffix, bit) = match *f {
                        "OE" => ("o", Some(1 << 10)),
                        "Rc" => (".", rc_bit(info.form)),
                        "LK" => ("l", Some(1)),
                        "AA" => ("a", Some(2)),
                        _ => ("", None),
                    };
                    let Some(bit) = bit else {
                        ok = false;
                        break;
                    };
                    name.push_str(suffix);
                    flags |= bit;
                }
                if ok {
                    map.entry(name).or_default().push(Spelling { op: info.op, flags, operands: operands.clone() });
                }
            }
        }
        map
    })
}

fn bits(word: &mut u32, lo: u32, len: u32, v: u32) {
    let m = ((1u64 << len) - 1) as u32;
    *word = (*word & !(m << lo)) | ((v & m) << lo);
}

fn is_vmx128(op: Op) -> bool {
    op.is_vmx128()
}

/// Places `v` into the field `name` of `word`. The inverse of [`Instr::operand`].
fn set_field(word: &mut u32, op: Op, address: u32, name: &str, v: i64) -> Result<(), String> {
    let form = op.form();
    let u = v as u32;
    match name {
        "RT" | "RD" | "RS" | "FT" | "FD" | "FS" | "TO" | "BO" | "CRBD" | "FPSCRD" => bits(word, 21, 5, u),
        "RA" | "RA0" | "FA" | "BI" | "CRBA" => bits(word, 16, 5, u),
        "RB" | "FB" | "CRBB" => bits(word, 11, 5, u),
        "VD" | "VS" | "VT" => {
            bits(word, 21, 5, u);
            if is_vmx128(op) {
                bits(word, 2, 2, u >> 5);
            }
        }
        "VA" => {
            bits(word, 16, 5, u);
            if is_vmx128(op) {
                bits(word, 5, 1, u >> 5);
                bits(word, 10, 1, u >> 6);
            }
        }
        "VB" => {
            bits(word, 11, 5, u);
            if is_vmx128(op) {
                bits(word, 0, 2, u >> 5);
            }
        }
        "FC" | "RC" => bits(word, 6, 5, u),
        "VC" => {
            if form == Form::VX128_2 {
                bits(word, 6, 3, u)
            } else {
                bits(word, 6, 5, u)
            }
        }
        "d" => bits(word, 0, 16, u),
        "ds" => {
            if v & 3 != 0 {
                return Err(format!("displacement {v} is not a multiple of 4"));
            }
            bits(word, 2, 14, u >> 2)
        }
        "SIMM" => match form {
            Form::VX | Form::VX128_3 => bits(word, 16, 5, u),
            _ => bits(word, 0, 16, u),
        },
        "UIMM" => match form {
            Form::VX | Form::VX128_3 => bits(word, 16, 5, u),
            Form::VX128_P => {
                bits(word, 16, 5, u);
                bits(word, 6, 3, u >> 5);
            }
            _ => bits(word, 0, 16, u),
        },
        "CRFD" => bits(word, 23, 3, u),
        "CRFS" => bits(word, 18, 3, u),
        "L" => bits(word, if form == Form::XFL { 25 } else { 21 }, 1, u),
        "L15" => bits(word, 16, 1, u),
        "SPR" | "TBR" => bits(word, 11, 10, ((u & 0x1F) << 5) | (u >> 5)),
        "CRM" => bits(word, 12, 8, u),
        "FM" => bits(word, 17, 8, u),
        "SH" => match form {
            Form::XS | Form::MD => {
                bits(word, 11, 5, u);
                bits(word, 1, 1, u >> 5);
            }
            Form::VX128_5 => bits(word, 6, 4, u),
            _ => bits(word, 11, 5, u),
        },
        "SHB" => bits(word, 6, 4, u),
        "MB" | "ME" => match form {
            Form::M => bits(word, if name == "MB" { 6 } else { 1 }, 5, u),
            Form::MD | Form::MDS => {
                bits(word, 6, 5, u);
                bits(word, 5, 1, u >> 5);
            }
            _ => return Err(format!("{name} is not a field of {}", op.mnemonic())),
        },
        "IMM" => bits(word, if form == Form::VX128_4 { 16 } else { 11 }, 5, u),
        "z" => bits(word, 6, 2, u),
        "LEV" => bits(word, 5, 7, u),
        "ADDR" => {
            let absolute = *word & 2 != 0;
            let offset = if absolute { v } else { v - address as i64 };
            if offset & 3 != 0 {
                return Err(format!("branch target 0x{v:X} is not word aligned"));
            }
            match form {
                Form::I => bits(word, 2, 24, (offset >> 2) as u32),
                Form::B => bits(word, 2, 14, (offset >> 2) as u32),
                _ => return Err("ADDR on a non-branch form".into()),
            }
        }
        _ => return Err(format!("don't know how to encode operand {name}")),
    }
    Ok(())
}

/// Parses a number: decimal or `0x` hex, optionally negative.
pub fn parse_number(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest.trim()),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let v = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok().or_else(|| u64::from_str_radix(hex, 16).ok().map(|u| u as i64))?
    } else {
        body.parse::<i64>().ok()?
    };
    Some(if neg { v.wrapping_neg() } else { v })
}

fn parse_operand(name: &str, text: &str, resolve: &dyn Fn(&str) -> Option<u32>) -> Result<i64, String> {
    let t = text.trim();
    let reg = |prefix: char| -> Option<i64> {
        let rest = t.strip_prefix(prefix)?;
        rest.parse::<i64>().ok()
    };
    let value = match name {
        // A few templates use RD for an FPR (`mffs`), so accept either prefix.
        "RT" | "RD" | "RS" | "RA" | "RA0" | "RB" | "RC" => reg('r').or_else(|| reg('f')).or_else(|| parse_number(t)),
        "FT" | "FD" | "FS" | "FA" | "FB" | "FC" => reg('f').or_else(|| parse_number(t)),
        "VD" | "VS" | "VT" | "VA" | "VB" | "VC" => reg('v').or_else(|| parse_number(t)),
        "CRFD" | "CRFS" => t.strip_prefix("cr").and_then(|r| r.parse().ok()).or_else(|| parse_number(t)),
        "SPR" | "TBR" => match t {
            "xer" => Some(1),
            "lr" => Some(8),
            "ctr" => Some(9),
            "tbl" => Some(268),
            "tbu" => Some(269),
            _ => parse_number(t),
        },
        "ADDR" => parse_number(t).or_else(|| resolve(t).map(|a| a as i64)),
        _ => parse_number(t),
    };
    value.ok_or_else(|| format!("can't read operand {name} from {t:?}"))
}

/// Range check for immediates, so `addi r3, r3, 0x12345` is an error. 16-bit
/// immediates accept the signed or unsigned spelling (`-1` and `0xFFFF`), and
/// a 32-bit value that sign-extends from 16 bits (`0xFFFFFFFF`).
fn check_range(name: &str, form: Form, v: i64) -> Result<i64, String> {
    let (lo, hi): (i64, i64) = match name {
        "d" => (-0x8000, 0xFFFF),
        "ds" => (-0x8000, 0x7FFC),
        "SIMM" if matches!(form, Form::VX | Form::VX128_3) => (-16, 15),
        "UIMM" if matches!(form, Form::VX | Form::VX128_3) => (0, 31),
        "UIMM" if form == Form::VX128_P => (0, 255),
        "SIMM" | "UIMM" => (-0x8000, 0xFFFF),
        _ => return Ok(v),
    };
    if (lo..=hi).contains(&v) {
        return Ok(v);
    }
    let w = v as u32 as i32 as i64;
    if (v as u64 >> 32 == 0 || v < 0) && (-0x8000..0).contains(&w) {
        return Ok(w);
    }
    Err(format!("{name} value {v} is out of range"))
}

fn encode(s: &Spelling, address: u32, args: &[String], resolve: &dyn Fn(&str) -> Option<u32>) -> Result<u32, String> {
    if args.len() != s.operands.len() {
        return Err(format!("{} takes {} operands, got {}", s.op.mnemonic(), s.operands.len(), args.len()));
    }
    let mut word = s.op.info().opcode | s.flags;
    let mut values = Vec::new();
    for (name, text) in s.operands.iter().zip(args) {
        let v = check_range(name, s.op.form(), parse_operand(name, text, resolve)?)?;
        set_field(&mut word, s.op, address, name, v)?;
        values.push((*name, v));
    }
    // Read everything back through the decoder.
    let i = Instr::decode(address, word).ok_or_else(|| format!("0x{word:08X} doesn't decode"))?;
    if i.op != s.op {
        return Err(format!("encoded 0x{word:08X} decodes as {} instead of {}", i.op.mnemonic(), s.op.mnemonic()));
    }
    for (name, v) in values {
        let back = if name == "ADDR" { i.branch_target().map(|t| t as i64) } else { i.operand(name) };
        let expect = match name {
            "ADDR" => v as u32 as i64,
            "d" | "SIMM" if !matches!(s.op.form(), Form::VX | Form::VX128_3) => v as u16 as i16 as i64,
            "UIMM" if !matches!(s.op.form(), Form::VX | Form::VX128_3 | Form::VX128_P) => v & 0xFFFF,
            "SIMM" => ((v as i32) << 27 >> 27) as i64,
            _ => v,
        };
        if back != Some(expect) {
            return Err(format!("operand {name}={v} doesn't fit (reads back as {back:?})"));
        }
    }
    Ok(word)
}

/// Splits `"r3, -8(r1)"` into `["r3", "-8", "r1"]`.
fn split_args(s: &str) -> Vec<String> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let (Some(open), true) = (part.find('('), part.ends_with(')')) {
            out.push(part[..open].trim().to_string());
            out.push(part[open + 1..part.len() - 1].trim().to_string());
        } else {
            out.push(part.to_string());
        }
    }
    out
}

/// Conditions for `b<cond>`, `tw<cond>`: (BO for branches, CR bit, TO for traps).
const CONDITIONS: &[(&str, u32, u32, u32)] = &[
    ("lt", 12, 0, 16),
    ("le", 4, 1, 20),
    ("eq", 12, 2, 4),
    ("ge", 4, 0, 12),
    ("gt", 12, 1, 8),
    ("nl", 4, 0, 12),
    ("ne", 4, 2, 24),
    ("ng", 4, 1, 20),
    ("so", 12, 3, 0),
    ("ns", 4, 3, 0),
    ("un", 12, 3, 0),
    ("nu", 4, 3, 0),
];

const TRAP_CONDITIONS: &[(&str, u32)] = &[
    ("lt", 16),
    ("le", 20),
    ("eq", 4),
    ("ge", 12),
    ("gt", 8),
    ("nl", 12),
    ("ne", 24),
    ("ng", 20),
    ("llt", 2),
    ("lle", 6),
    ("lge", 5),
    ("lgt", 1),
    ("lnl", 5),
    ("lng", 6),
];

fn n(args: &[String], k: usize) -> Result<i64, String> {
    args.get(k).and_then(|a| parse_number(a)).ok_or_else(|| format!("operand {k} must be a number"))
}

fn arity(args: &[String], k: usize) -> Result<(), String> {
    if args.len() == k { Ok(()) } else { Err(format!("expected {k} operands, got {}", args.len())) }
}

/// Rewrites an extended mnemonic into its base instruction. Returns `None` if
/// `mnem` isn't an extended mnemonic.
fn expand(mnem: &str, args: &[String]) -> Option<Result<(String, Vec<String>), String>> {
    let (core, dot) = match mnem.strip_suffix('.') {
        Some(c) => (c, "."),
        None => (mnem, ""),
    };
    let a = |k: usize| args.get(k).cloned().unwrap_or_default();
    let s = |v: i64| v.to_string();
    let out = |m: &str, v: Vec<String>| Some(Ok((m.to_string(), v)));
    let r = (|| -> Result<Option<(String, Vec<String>)>, String> {
        let base = |m: &str| format!("{m}{dot}");
        Ok(Some(match core {
            "nop" if dot.is_empty() => ("ori".into(), vec!["r0".into(), "r0".into(), "0".into()]),
            "lwsync" if dot.is_empty() => ("sync".into(), vec!["1".into()]),
            "li" | "lis" if dot.is_empty() => {
                arity(args, 2)?;
                (if core == "li" { "addi" } else { "addis" }.into(), vec![a(0), "0".into(), a(1)])
            }
            "mr" | "not" => {
                arity(args, 2)?;
                (base(if core == "mr" { "or" } else { "nor" }), vec![a(0), a(1), a(1)])
            }
            "subi" | "subis" | "subic" => {
                arity(args, 3)?;
                let m = match core {
                    "subi" => "addi",
                    "subis" => "addis",
                    _ if dot.is_empty() => "addic",
                    _ => "addic.",
                };
                (m.into(), vec![a(0), a(1), s(-n(args, 2)?)])
            }
            "sub" | "subo" | "subc" | "subco" => {
                arity(args, 3)?;
                let m = core.replacen("sub", "subf", 1);
                (base(&m), vec![a(0), a(2), a(1)])
            }
            "slwi" | "srwi" | "rotlwi" | "rotrwi" | "clrlwi" | "clrrwi" => {
                arity(args, 3)?;
                let k = n(args, 2)?;
                let (sh, mb, me) = match core {
                    "slwi" => (k, 0, 31 - k),
                    "srwi" => ((32 - k) & 31, k, 31),
                    "rotlwi" => (k, 0, 31),
                    "rotrwi" => ((32 - k) & 31, 0, 31),
                    "clrlwi" => (0, k, 31),
                    _ => (0, 0, 31 - k),
                };
                (base("rlwinm"), vec![a(0), a(1), s(sh), s(mb), s(me)])
            }
            "extlwi" | "extrwi" | "inslwi" | "insrwi" => {
                arity(args, 4)?;
                let (k, b) = (n(args, 2)?, n(args, 3)?);
                let (m, sh, mb, me) = match core {
                    "extlwi" => ("rlwinm", b, 0, k - 1),
                    "extrwi" => ("rlwinm", (b + k) & 31, 32 - k, 31),
                    "inslwi" => ("rlwimi", (32 - b) & 31, b, b + k - 1),
                    _ => ("rlwimi", (32 - (b + k)) & 31, b, b + k - 1),
                };
                (base(m), vec![a(0), a(1), s(sh), s(mb), s(me)])
            }
            "rotlw" => {
                arity(args, 3)?;
                (base("rlwnm"), vec![a(0), a(1), a(2), "0".into(), "31".into()])
            }
            "sldi" | "srdi" | "clrldi" | "clrrdi" | "rotldi" => {
                arity(args, 3)?;
                let k = n(args, 2)?;
                let (m, sh, mask) = match core {
                    "sldi" => ("rldicr", k, 63 - k),
                    "srdi" => ("rldicl", (64 - k) & 63, k),
                    "clrldi" => ("rldicl", 0, k),
                    "clrrdi" => ("rldicr", 0, 63 - k),
                    _ => ("rldicl", k, 0),
                };
                (base(m), vec![a(0), a(1), s(sh), s(mask)])
            }
            "cmpw" | "cmpwi" | "cmplw" | "cmplwi" | "cmpd" | "cmpdi" | "cmpld" | "cmpldi" if dot.is_empty() => {
                let (cr, rest) = if args.len() == 3 { (a(0), &args[1..]) } else { ("cr0".to_string(), args) };
                if rest.len() != 2 {
                    return Err("compare takes 2 or 3 operands".into());
                }
                let m = match core {
                    "cmpw" | "cmpd" => "cmp",
                    "cmpwi" | "cmpdi" => "cmpi",
                    "cmplw" | "cmpld" => "cmpl",
                    _ => "cmpli",
                };
                let l = if core.starts_with("cmpd") || core.starts_with("cmpld") { "1" } else { "0" };
                (m.into(), vec![cr, l.into(), rest[0].clone(), rest[1].clone()])
            }
            "trap" if dot.is_empty() => ("tw".into(), vec!["31".into(), "r0".into(), "r0".into()]),
            "mtlr" | "mtctr" | "mtxer" if dot.is_empty() => {
                arity(args, 1)?;
                let spr = &core[2..];
                ("mtspr".into(), vec![spr.into(), a(0)])
            }
            "mflr" | "mfctr" | "mfxer" if dot.is_empty() => {
                arity(args, 1)?;
                let spr = &core[2..];
                ("mfspr".into(), vec![a(0), spr.into()])
            }
            "mtcr" if dot.is_empty() => {
                arity(args, 1)?;
                ("mtcrf".into(), vec!["0xFF".into(), a(0)])
            }
            "crset" | "crclr" | "crmove" | "crnot" if dot.is_empty() => {
                let (m, v) = match core {
                    "crset" => ("creqv", vec![a(0), a(0), a(0)]),
                    "crclr" => ("crxor", vec![a(0), a(0), a(0)]),
                    "crmove" => ("cror", vec![a(0), a(1), a(1)]),
                    _ => ("crnor", vec![a(0), a(1), a(1)]),
                };
                (m.into(), v)
            }
            "blr" | "blrl" | "bctr" | "bctrl" if dot.is_empty() => {
                arity(args, 0)?;
                let m = match core {
                    "blr" => "bclr",
                    "blrl" => "bclrl",
                    "bctr" => "bcctr",
                    _ => "bcctrl",
                };
                (m.into(), vec!["20".into(), "0".into()])
            }
            _ => {
                if !dot.is_empty() {
                    return Ok(None);
                }
                // tw<cond>, tw<cond>i, td<cond>, td<cond>i
                for (prefix, imm_m, reg_m) in [("tw", "twi", "tw"), ("td", "tdi", "td")] {
                    let Some(rest) = core.strip_prefix(prefix) else { continue };
                    let (cond, imm) = match rest.strip_suffix('i') {
                        Some(c) if TRAP_CONDITIONS.iter().any(|t| t.0 == c) => (c, true),
                        _ => (rest, false),
                    };
                    if let Some(&(_, to)) = TRAP_CONDITIONS.iter().find(|t| t.0 == cond) {
                        arity(args, 2)?;
                        return Ok(Some(((if imm { imm_m } else { reg_m }).into(), vec![s(to as i64), a(0), a(1)])));
                    }
                }
                // bdnz/bdz[l] target and b<cond>[l][a] / b<cond>lr[l] / b<cond>ctr[l]
                let Some(rest) = core.strip_prefix('b') else { return Ok(None) };
                let (rest, suffix) = if let Some(r) = rest.strip_suffix("lrl") {
                    (r, "lrl")
                } else if let Some(r) = rest.strip_suffix("lr") {
                    (r, "lr")
                } else if let Some(r) = rest.strip_suffix("ctrl") {
                    (r, "ctrl")
                } else if let Some(r) = rest.strip_suffix("ctr") {
                    (r, "ctr")
                } else if let Some(r) = rest.strip_suffix("la") {
                    (r, "la")
                } else if let Some(r) = rest.strip_suffix('l') {
                    (r, "l")
                } else if let Some(r) = rest.strip_suffix('a') {
                    (r, "a")
                } else {
                    (rest, "")
                };
                let rest = rest.trim_end_matches(['+', '-']);
                let (bo, bi_bit, uses_cr) = match rest {
                    "dnz" => (16, 0, false),
                    "dz" => (18, 0, false),
                    _ => match CONDITIONS.iter().find(|c| c.0 == rest) {
                        Some(&(_, bo, bit, _)) => (bo, bit, true),
                        None => return Ok(None),
                    },
                };
                let to_reg = matches!(suffix, "lr" | "lrl" | "ctr" | "ctrl");
                let (cr, target) = if uses_cr {
                    match (args.len(), to_reg) {
                        (0, true) => (0, None),
                        (1, true) => (parse_cr(&a(0))?, None),
                        (1, false) => (0, Some(a(0))),
                        (2, false) => (parse_cr(&a(0))?, Some(a(1))),
                        _ => return Err("wrong number of branch operands".into()),
                    }
                } else {
                    match (args.len(), to_reg) {
                        (0, true) => (0, None),
                        (1, false) => (0, Some(a(0))),
                        _ => return Err("wrong number of branch operands".into()),
                    }
                };
                let bi = s(4 * cr + bi_bit as i64);
                let m = match suffix {
                    "lr" => "bclr",
                    "lrl" => "bclrl",
                    "ctr" => "bcctr",
                    "ctrl" => "bcctrl",
                    "l" => "bcl",
                    "a" => "bca",
                    "la" => "bcla",
                    _ => "bc",
                };
                let mut v = vec![s(bo as i64), bi];
                v.extend(target);
                (m.into(), v)
            }
        }))
    })();
    match r {
        Ok(Some((m, v))) => out(&m, v),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    }
}

fn parse_cr(s: &str) -> Result<i64, String> {
    s.trim().strip_prefix("cr").and_then(|r| r.parse().ok()).ok_or_else(|| format!("expected a CR field, got {s:?}"))
}

/// Assembles one instruction at `address`. `resolve` maps label names to
/// addresses for branch targets.
pub fn assemble(line: &str, address: u32, resolve: &dyn Fn(&str) -> Option<u32>) -> Result<u32, String> {
    let line = line.trim();
    let (mnem, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let mut args = split_args(rest);
    let mut mnem = mnem.to_ascii_lowercase();

    if let Some(r) = expand(&mnem, &args) {
        let (m, a) = r?;
        mnem = m;
        args = a;
    }
    let candidates = spellings().get(&mnem).ok_or_else(|| format!("unknown mnemonic {mnem:?}"))?;
    // `sc` and `sync` may omit their 0 operand.
    if matches!(mnem.as_str(), "sc" | "sync") && args.is_empty() {
        args.push("0".into());
    }
    let mut first_err = None;
    for s in candidates {
        match encode(s, address, &args, resolve) {
            Ok(w) => return Ok(w),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(format!("{line}: {}", first_err.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asm(s: &str) -> u32 {
        assemble(s, 0x8200_0000, &|_| None).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn base_and_extended_mnemonics() {
        assert_eq!(asm("addi r3, r0, 0x1"), 0x3860_0001);
        assert_eq!(asm("li r3, 1"), 0x3860_0001);
        assert_eq!(asm("li r4, 0xFFFFFFFF"), asm("li r4, -1"));
        assert_eq!(asm("ld r14, -0x98(r1)"), 0xE9C1_FF68);
        assert_eq!(asm("std r14, -0x98(r1)"), 0xF9C1_FF68);
        assert_eq!(asm("mflr r0"), 0x7C08_02A6);
        assert_eq!(asm("mfspr r0, lr"), 0x7C08_02A6);
        assert_eq!(asm("mtctr r0"), 0x7C09_03A6);
        assert_eq!(asm("blr"), 0x4E80_0020);
        assert_eq!(asm("bctr"), 0x4E80_0420);
        assert_eq!(asm("nop"), 0x6000_0000);
        assert_eq!(asm("dcbz r0, r0"), 0x7C00_07EC);
        assert_eq!(asm("dcbz128 r0, r0"), 0x7C20_07EC);
        assert_eq!(asm("lvx128 v64, r11, r12"), 0x100B_60CB);
        assert_eq!(asm("stvx128 v64, r11, r12"), 0x100B_61CB);
        assert_eq!(asm("vpermwi128 v4, v3, 0x1B"), 0x189B_1A10);
        assert_eq!(asm("vpermwi128 v4, v3, 0xE4"), 0x1884_1BD0);
        assert_eq!(asm("vmsum3fp128 v127, v127, v127"), 0x17FF_FDBF);
        // slwi r3, r3, 2 == rlwinm r3, r3, 2, 0, 29
        assert_eq!(asm("slwi r3, r3, 2"), asm("rlwinm r3, r3, 2, 0, 29"));
        assert_eq!(asm("add. r3, r4, r5") & 1, 1);
        assert_eq!(asm("addo. r3, r4, r5") & 0x401, 0x401);
        assert_eq!(asm("cmpwi cr6, r3, 3"), asm("cmpi cr6, 0, r3, 3"));
        assert_eq!(asm("cmpd r3, r4") >> 21 & 1, 1);
    }

    #[test]
    fn branches_resolve_labels() {
        let labels = |l: &str| (l == "target").then_some(0x8200_0010);
        let b = assemble("beq target", 0x8200_0000, &labels).unwrap();
        let i = Instr::decode(0x8200_0000, b).unwrap();
        assert_eq!(i.op, Op::bcx);
        assert_eq!((i.rt(), i.ra()), (12, 2));
        assert_eq!(i.branch_target(), Some(0x8200_0010));
        let b = assemble("bne cr6, target", 0x8200_0000, &labels).unwrap();
        let i = Instr::decode(0x8200_0000, b).unwrap();
        assert_eq!((i.rt(), i.ra()), (4, 26));
        assert_eq!(assemble("bl 0x82000100", 0x8200_0000, &labels).unwrap(), 0x4800_0101);
    }

    #[test]
    fn out_of_range_values_are_errors() {
        assert!(assemble("addi r3, r3, 0x12345", 0, &|_| None).is_err());
        assert!(assemble("vspltisw v3, 16", 0, &|_| None).is_err());
        assert!(assemble("lvx128 v128, r1, r2", 0, &|_| None).is_err());
        assert!(assemble("frobnicate r1", 0, &|_| None).is_err());
    }
}
