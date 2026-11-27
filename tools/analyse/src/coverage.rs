//! Decode coverage: does every word in the code sections decode, and unambiguously?

use std::collections::{BTreeMap, HashMap};
use std::error::Error;

use ppc::{Group, Instr, Op};
use xex::Xex;

fn pct(n: u64, d: u64) -> f64 {
    100.0 * n as f64 / d.max(1) as f64
}

/// Sorted, non-overlapping `[begin, end)` ranges with membership lookup.
struct Ranges(Vec<(u32, u32)>);

impl Ranges {
    fn contains(&self, addr: u32) -> bool {
        let i = self.0.partition_point(|&(begin, _)| begin <= addr);
        i > 0 && addr < self.0[i - 1].1
    }
}

#[derive(Default)]
struct Bucket {
    count: u64,
    example: Option<(u32, u32)>,
}

impl Bucket {
    fn add(&mut self, addr: u32, word: u32) {
        self.count += 1;
        self.example.get_or_insert((addr, word));
    }
}

/// Decodes every word of every code section and reports what doesn't decode,
/// what decodes ambiguously, and how instructions are distributed.
pub fn run(x: &Xex, examples: usize) -> Result<(), Box<dyn Error>> {
    let mut functions: Vec<(u32, u32)> = x.pdata()?.iter().map(|f| (f.begin, f.end())).collect();
    functions.sort_unstable();
    let functions = Ranges(functions);

    let (mut total, mut zero, mut decoded, mut in_fn, mut outside_decoded, mut outside_invalid) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut invalid_in_fn: Vec<(u32, u32)> = Vec::new();
    let mut invalid_by_primary: BTreeMap<u32, Bucket> = BTreeMap::new();
    let mut ambiguous: BTreeMap<(Op, Op), Bucket> = BTreeMap::new();
    let mut by_op: HashMap<Op, u64> = HashMap::new();
    let mut by_group: BTreeMap<String, u64> = BTreeMap::new();
    let mut vmx128 = 0u64;
    let mut first_bad: Vec<String> = Vec::new();

    for section in x.code_sections() {
        let start = section.rva as usize;
        let data = &x.image[start..start + section.virtual_size as usize];
        for (i, chunk) in data.chunks_exact(4).enumerate() {
            let addr = section.address + i as u32 * 4;
            let word = u32::from_be_bytes(chunk.try_into().unwrap());
            total += 1;
            let inside = functions.contains(addr);
            in_fn += inside as u64;
            if word == 0 {
                zero += 1;
                continue;
            }
            let mut matches = ppc::decode_all(word);
            match matches.next() {
                None => {
                    invalid_by_primary.entry(word >> 26).or_default().add(addr, word);
                    if inside {
                        invalid_in_fn.push((addr, word));
                    } else {
                        outside_invalid += 1;
                    }
                }
                Some(op) => {
                    decoded += 1;
                    outside_decoded += !inside as u64;
                    *by_op.entry(op).or_default() += 1;
                    *by_group.entry(format!("{:?}", op.info().group)).or_default() += 1;
                    vmx128 += op.is_vmx128() as u64;
                    if let Some(other) = matches.next() {
                        let bucket = ambiguous.entry((op, other)).or_default();
                        if bucket.count == 0 && first_bad.len() < examples {
                            first_bad.push(format!("0x{addr:08X}: {word:08X} = {op:?} or {other:?}"));
                        }
                        bucket.add(addr, word);
                    }
                }
            }
        }
    }

    let nonzero = total - zero;
    let invalid = nonzero - decoded;
    println!("code words        : {total} ({zero} zero/padding, {nonzero} non-zero)");
    println!("decoded           : {decoded} ({:.4}% of non-zero words)", pct(decoded, nonzero));
    println!("undecodable       : {invalid} ({} inside .pdata functions, {outside_invalid} outside)", invalid_in_fn.len());
    println!("inside .pdata fns : {in_fn} words ({:.2}%)", pct(in_fn, total));
    println!("outside .pdata    : {} words; {outside_decoded} decode as instructions (candidates for functions without .pdata)", total - in_fn);
    println!("VMX128            : {vmx128} instructions");
    println!("by group          : {}", by_group.iter().map(|(g, n)| format!("{g}={n}")).collect::<Vec<_>>().join(" "));

    let ambiguous_words: u64 = ambiguous.values().map(|b| b.count).sum();
    println!("ambiguous words   : {ambiguous_words} in {} distinct opcode pairs", ambiguous.len());
    for ((a, b), bucket) in &ambiguous {
        let (addr, word) = bucket.example.unwrap();
        println!("  {a:?} / {b:?}: {} words, e.g. 0x{addr:08X} {word:08X}", bucket.count);
    }

    if !invalid_by_primary.is_empty() {
        println!("undecodable by primary opcode:");
        for (primary, bucket) in &invalid_by_primary {
            let (addr, word) = bucket.example.unwrap();
            println!("  op {primary:>2}: {:>7} words, e.g. 0x{addr:08X} {word:08X}", bucket.count);
        }
    }
    if !invalid_in_fn.is_empty() {
        println!("first undecodable words inside .pdata functions:");
        for (addr, word) in invalid_in_fn.iter().take(examples) {
            println!("  0x{addr:08X}: {word:08X}");
        }
    }

    let mut ops: Vec<(Op, u64)> = by_op.into_iter().collect();
    ops.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("distinct opcodes used: {} of {}", ops.len(), ppc::OP_COUNT);
    println!("most frequent:");
    for chunk in ops.iter().take(48).collect::<Vec<_>>().chunks(4) {
        println!("  {}", chunk.iter().map(|(op, n)| format!("{:<14}{:>8}", op.mnemonic(), n)).collect::<Vec<_>>().join("   "));
    }
    let rare: Vec<String> = ops.iter().rev().take(40).map(|(op, n)| format!("{}={n}", op.mnemonic())).collect();
    println!("rarest: {}", rare.join(" "));
    let vector_ops = ops.iter().filter(|(op, _)| op.info().group == Group::v).count();
    println!("distinct vector opcodes used: {vector_ops}");

    // Show a few decoded instructions as a smoke test of the disassembler.
    if let Some(ep) = x.entry_point() {
        println!("entry point disassembly:");
        for k in 0..8u32 {
            let addr = ep + k * 4;
            let word = u32::from_be_bytes(x.bytes_at(addr, 4).unwrap().try_into().unwrap());
            match Instr::decode(addr, word) {
                Some(i) => println!("  {addr:08X}  {word:08X}  {i}"),
                None => println!("  {addr:08X}  {word:08X}  <undecodable>"),
            }
        }
    }
    Ok(())
}
