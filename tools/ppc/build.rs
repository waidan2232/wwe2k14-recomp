//! Generates the opcode table from `data/ppc-instructions.xml`.
//!
//! The XML is the instruction list from the Xenia project (BSD-3-Clause, see
//! THIRD_PARTY_NOTICES.md), used here purely as data: mnemonics, canonical
//! encodings, instruction forms and operand lists. Each instruction becomes a
//! mask/value pair. The mask covers the primary opcode (bits 0-5) plus the
//! extended-opcode bit ranges of its form (IBM bit numbering, bit 0 = MSB).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::{env, fs};

struct Insn {
    mnem: String,
    ident: String,
    opcode: u32,
    form: String,
    group: String,
    desc: String,
    sync: bool,
    reads: Vec<(String, bool)>,
    writes: Vec<(String, bool)>,
    disasm: Option<String>,
    mask: u32,
}

/// Extended-opcode bit ranges per form (inclusive, IBM numbering). Forms not
/// listed (D, B, I, SC, M) are identified by the primary opcode alone.
fn form_ranges(form: &str) -> &'static [(u32, u32)] {
    match form {
        "X" | "XL" | "XFX" | "XFL" => &[(21, 30)],
        // mfcr / mfocrf: IBM bit 11 selects the one-field form.
        "XFXM" => &[(11, 11), (21, 30)],
        "VX" => &[(21, 31)],
        "VX128" => &[(22, 25), (27, 27)],
        // VX128 with the VA field as part of the opcode (vupk*s{b,h}128).
        "VX128_U" => &[(11, 15), (21, 27)],
        "VX128_1" => &[(21, 27), (30, 31)],
        "VX128_2" => &[(22, 22), (27, 27)],
        "VX128_3" => &[(21, 27)],
        "VX128_4" => &[(21, 23), (26, 27)],
        "VX128_5" => &[(27, 27)],
        "VX128_R" => &[(22, 24), (27, 27)],
        "VX128_P" => &[(21, 22), (26, 27)],
        "VC" => &[(22, 31)],
        "VA" => &[(26, 31)],
        "XO" => &[(22, 30)],
        "XW" => &[(25, 30)],
        "A" => &[(26, 30)],
        "DS" => &[(30, 31)],
        "MD" => &[(27, 29)],
        "MDS" => &[(27, 30)],
        "XS" => &[(21, 29)],
        "DCBZ" => &[(6, 10), (21, 30)],
        "D" | "B" | "I" | "SC" | "M" => &[],
        other => panic!("ppc-instructions.xml uses unknown form {other:?}"),
    }
}

fn ibm_mask(first: u32, last: u32) -> u32 {
    let len = last - first + 1;
    let ones = if len == 32 { u32::MAX } else { (1u32 << len) - 1 };
    ones << (31 - last)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!(" {name}=\"");
    let start = tag.find(&pat)? + pat.len();
    let end = start + tag[start..].find('"')?;
    Some(unescape(&tag[start..end]))
}

fn strip_comments(mut s: String) -> String {
    while let Some(start) = s.find("<!--") {
        let end = s[start..].find("-->").map(|e| start + e + 3).expect("unterminated XML comment");
        s.replace_range(start..end, "");
    }
    s
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super",
    "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box",
    "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

fn ident(name: &str) -> String {
    let id = name.replace('.', "x");
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "bad identifier {id:?}");
    if RUST_KEYWORDS.contains(&id.as_str()) { format!("r#{id}") } else { id }
}

fn parse(xml: &str) -> Vec<Insn> {
    let xml = strip_comments(xml.to_owned());
    let mut out = Vec::new();
    let mut rest = xml.as_str();
    while let Some(start) = rest.find("<insn ") {
        rest = &rest[start..];
        let tag_end = rest.find('>').expect("unterminated <insn>");
        let tag = &rest[..tag_end];
        let body_end = rest.find("</insn>").expect("missing </insn>");
        let body = &rest[tag_end + 1..body_end];

        let mnem = attr(tag, "mnem").expect("insn without mnem");
        let opcode = u32::from_str_radix(&attr(tag, "opcode").expect("insn without opcode"), 16).expect("bad opcode");
        let form = attr(tag, "form").expect("insn without form");
        let fields = |kind: &str| -> Vec<(String, bool)> {
            let pat = format!("<{kind} ");
            body.match_indices(&pat)
                .map(|(i, _)| {
                    let t = &body[i..i + body[i..].find('>').unwrap()];
                    (attr(t, "field").expect("operand without field"), attr(t, "conditional").as_deref() == Some("true"))
                })
                .collect()
        };
        let disasm = body.find("<disasm>").map(|i| {
            let s = &body[i + 8..];
            unescape(s[..s.find("</disasm>").expect("unterminated <disasm>")].trim())
        });
        let mask = form_ranges(&form).iter().fold(ibm_mask(0, 5), |m, &(a, b)| m | ibm_mask(a, b));
        out.push(Insn {
            ident: ident(&mnem),
            mnem,
            opcode,
            group: attr(tag, "group").expect("insn without group"),
            desc: attr(tag, "desc").unwrap_or_default(),
            sync: attr(tag, "sync").as_deref() == Some("true"),
            reads: fields("in"),
            writes: fields("out"),
            disasm,
            form,
            mask,
        });
        rest = &rest[body_end + 7..];
    }
    out
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let xml_path = manifest.join("data").join("ppc-instructions.xml");
    println!("cargo:rerun-if-changed={}", xml_path.display());
    println!("cargo:rerun-if-changed=build.rs");
    let insns = parse(&fs::read_to_string(&xml_path).expect("read ppc-instructions.xml"));
    assert!(insns.len() > 400, "only {} instructions parsed", insns.len());

    let mut idents = BTreeSet::new();
    for i in &insns {
        assert!(idents.insert(i.ident.clone()), "duplicate mnemonic identifier {}", i.ident);
    }
    let forms: BTreeSet<&str> = insns.iter().map(|i| i.form.as_str()).collect();
    let groups: BTreeSet<&str> = insns.iter().map(|i| i.group.as_str()).collect();
    let fields: BTreeSet<&str> =
        insns.iter().flat_map(|i| i.reads.iter().chain(&i.writes)).map(|(f, _)| f.as_str()).collect();

    let mut s = String::new();
    s.push_str("// @generated by build.rs from data/ppc-instructions.xml. Do not edit.\n\n");

    s.push_str("/// One entry per instruction in the table. Names follow the XML; an `x` suffix\n");
    s.push_str("/// marks instructions with record (`.`) or overflow (`o`) variants.\n");
    s.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[allow(non_camel_case_types)]\n#[repr(u16)]\npub enum Op {\n");
    for (n, i) in insns.iter().enumerate() {
        writeln!(s, "    {} = {n},", i.ident).unwrap();
    }
    s.push_str("}\n\n");
    writeln!(s, "pub const OP_COUNT: usize = {};\n", insns.len()).unwrap();

    s.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[allow(non_camel_case_types)]\npub enum Form {\n");
    for f in &forms {
        writeln!(s, "    {f},").unwrap();
    }
    s.push_str("}\n\n");

    s.push_str("/// Instruction class: b = branch, c = control/CR/SPR, f = FPU, i = integer, m = memory, v = vector.\n");
    s.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[allow(non_camel_case_types)]\npub enum Group {\n");
    for g in &groups {
        writeln!(s, "    {g},").unwrap();
    }
    s.push_str("}\n\n");

    s.push_str("/// Operand or implicit state an instruction reads or writes.\n");
    s.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[allow(non_camel_case_types)]\npub enum Field {\n");
    for f in &fields {
        writeln!(s, "    {f},").unwrap();
    }
    s.push_str("}\n\n");
    s.push_str("impl Field {\n    pub fn name(self) -> &'static str {\n        match self {\n");
    for f in &fields {
        writeln!(s, "            Field::{f} => {f:?},").unwrap();
    }
    s.push_str("        }\n    }\n}\n\n");

    s.push_str("pub static OPS: [OpInfo; OP_COUNT] = [\n");
    for i in &insns {
        let list = |v: &[(String, bool)]| {
            v.iter().map(|(f, c)| format!("(Field::{f}, {c})")).collect::<Vec<_>>().join(", ")
        };
        writeln!(
            s,
            "    OpInfo {{ op: Op::{}, mnemonic: {:?}, form: Form::{}, group: Group::{}, desc: {:?}, opcode: 0x{:08X}, mask: 0x{:08X}, value: 0x{:08X}, sync: {}, reads: &[{}], writes: &[{}], disasm: {:?} }},",
            i.ident, i.mnem, i.form, i.group, i.desc, i.opcode, i.mask, i.opcode & i.mask, i.sync,
            list(&i.reads), list(&i.writes), i.disasm
        )
        .unwrap();
    }
    s.push_str("];\n\n");

    // Candidates per primary opcode, most specific (most mask bits) first, then table order.
    s.push_str("/// Indices into `OPS` per primary opcode, most specific encoding first.\n");
    s.push_str("pub(crate) static BY_PRIMARY: [&[u16]; 64] = [\n");
    for primary in 0..64u32 {
        let mut idx: Vec<usize> = (0..insns.len()).filter(|&n| insns[n].opcode >> 26 == primary).collect();
        idx.sort_by_key(|&n| (std::cmp::Reverse(insns[n].mask.count_ones()), n));
        let list = idx.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
        writeln!(s, "    &[{list}],").unwrap();
    }
    s.push_str("];\n");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("table.rs");
    fs::write(out, s).expect("write table.rs");
}
