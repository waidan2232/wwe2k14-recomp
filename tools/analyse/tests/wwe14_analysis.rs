//! Phase 1 exit gate against the real WWE 2K14 executable.
//! Runs only when `WWE14_XEX` points at the extracted `default.xex`.

use analyse::{BctrKind, DataKind, Source};
use xex::Xex;

#[test]
fn phase1_gate() {
    let Ok(path) = std::env::var("WWE14_XEX") else {
        eprintln!("WWE14_XEX not set; skipping");
        return;
    };
    let xex = Xex::parse(&std::fs::read(path).unwrap()).unwrap();
    let a = analyse::analyse(&xex).expect("analysis");
    let d = &a.discovery;

    // ABI helpers at the addresses found by byte pattern.
    let helpers: Vec<(&str, u32)> = a.helpers.iter().map(|h| (h.kind.name, h.address)).collect();
    assert!(helpers.contains(&("savegprlr_14", 0x82C7_8DC0)));
    assert!(helpers.contains(&("restvmx_64", 0x82C7_ACFC)));

    // Stack-switching returns (longjmp). Each needs a config entry.
    let switches = analyse::stack_switch_returns(&xex, d);
    eprintln!("stack-switching returns: {:X?}", switches);
    assert_eq!(switches, vec![0x8343_8D38]);

    // Every unconditional bctr classified.
    assert_eq!(a.bctr_sites.len(), 2960);
    assert_eq!(a.unclassified_bctr().count(), 0);
    assert_eq!(a.switch_tables().count(), 1159);
    let escaping: Vec<String> = a
        .switch_tables()
        .filter_map(|t| {
            let owner = d.function_at(t.bctr).expect("switch inside a function");
            let outside: Vec<String> = t
                .labels
                .iter()
                .filter(|l| !(owner.begin..owner.end).contains(l))
                .map(|l| {
                    let target = d.function_at(*l).map(|f| format!("{:08X}({:?})", f.begin, f.source)).unwrap_or_default();
                    format!("{l:08X} in {target}")
                })
                .collect();
            (!outside.is_empty()).then(|| {
                format!("bctr {:08X} in {:08X}..{:08X} ({:?}): {}", t.bctr, owner.begin, owner.end, owner.source, outside.join(", "))
            })
        })
        .collect();
    assert!(escaping.is_empty(), "{} switches jump outside their function:\n{}", escaping.len(), escaping.join("\n"));
    assert!(a.bctr_sites.iter().all(|s| !matches!(s.kind, BctrKind::Unknown { .. })));

    // Every code byte owned exactly once; nothing undecodable left.
    assert!(d.problems.is_empty(), "{:?}", &d.problems[..d.problems.len().min(5)]);
    assert!(!d.data.iter().any(|r| matches!(r.2, DataKind::Undecodable | DataKind::InvalidFlow)));
    let count = |s: Source| d.functions.iter().filter(|f| f.source == s).count();
    assert_eq!(count(Source::ImportThunk), 270);
    assert_eq!(count(Source::AbiHelper), 8);
    assert_eq!(count(Source::Pdata), 58_502);
    assert_eq!(count(Source::SecondaryEntry), 29);
}

/// Disassembles every decodable instruction in the game and assembles the text
/// again. A mismatch means the decoder, the disassembler or the assembler gets
/// a field wrong (or the text drops bits the decoder ignores).
#[test]
fn assembler_round_trips_every_instruction() {
    let Ok(path) = std::env::var("WWE14_XEX") else {
        eprintln!("WWE14_XEX not set; skipping");
        return;
    };
    let xex = Xex::parse(&std::fs::read(path).unwrap()).unwrap();
    let code = analyse::Code::new(&xex);
    let mut total = 0usize;
    let mut no_template = 0usize;
    let mut bad: std::collections::BTreeMap<&str, (usize, Vec<String>)> = Default::default();
    for (addr, word) in code.words() {
        let Some(i) = ppc::Instr::decode(addr, word) else { continue };
        if i.op.info().disasm.is_none() {
            no_template += 1;
            continue;
        }
        total += 1;
        let text = i.to_string();
        let back = ppc::asm::assemble(&text, addr, &|_| None);
        if back != Ok(word) {
            let e = bad.entry(i.op.mnemonic()).or_default();
            e.0 += 1;
            if e.1.len() < 3 {
                e.1.push(format!("{addr:08X}: {word:08X} {text:?} -> {back:X?}"));
            }
        }
    }
    let wrong: usize = bad.values().map(|b| b.0).sum();
    eprintln!("round-tripped {total} instructions ({no_template} have no template); {wrong} mismatches");
    for (op, (count, examples)) in &bad {
        eprintln!("  {op}: {count}");
        for e in examples {
            eprintln!("      {e}");
        }
    }
    assert_eq!(wrong, 0);
}
