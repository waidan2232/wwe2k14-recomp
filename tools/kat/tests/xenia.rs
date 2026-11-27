//! Runs every instruction test vector as compiled code and checks the results.
//!
//! A wrong answer fails the test. Tests that use an instruction the
//! recompiler doesn't translate yet are listed by instruction and don't fail;
//! set `KAT_STRICT=1` to fail on those as well.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use kat::{FUNCTIONS, Reg, SKIPPED, TESTS, Test};
use ppc_rt::{Ctx, GuestMemory};

fn run(t: &Test) -> Result<(), String> {
    let mem = GuestMemory::new().map_err(|e| format!("can't reserve guest memory: {e}"))?;
    for (addr, bytes) in t.mem_in {
        mem.write(*addr, bytes);
    }
    let mut c = Ctx::new();
    for r in t.regs_in {
        match *r {
            Reg::R(n, v) => c.r[n as usize] = v,
            Reg::F(n, v) => c.f[n as usize] = f64::from_bits(v),
            Reg::V(n, v) => c.v[n as usize] = v,
            Reg::Cr(v) => c.set_cr_word(0xFF, v),
        }
    }
    let base = mem.base();
    catch_unwind(AssertUnwindSafe(|| unsafe { (t.entry)(&mut c, base) })).map_err(|p| {
        p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    })?;

    let mut wrong = Vec::new();
    for r in t.regs_out {
        match *r {
            Reg::R(n, v) if c.r[n as usize] != v => wrong.push(format!("r{n} = 0x{:016X}, want 0x{v:016X}", c.r[n as usize])),
            Reg::F(n, v) if c.f[n as usize].to_bits() != v => wrong.push(format!(
                "f{n} = 0x{:016X} ({}), want 0x{v:016X} ({})",
                c.f[n as usize].to_bits(),
                c.f[n as usize],
                f64::from_bits(v)
            )),
            Reg::V(n, v) if c.v[n as usize] != v => wrong.push(format!("v{n} = {}, want {}", words(c.v[n as usize]), words(v))),
            Reg::Cr(v) if c.cr_word() != v => wrong.push(format!("cr = 0x{:08X}, want 0x{v:08X}", c.cr_word())),
            _ => {}
        }
    }
    for (addr, bytes) in t.mem_out {
        let got = mem.read(*addr, bytes.len());
        if got != *bytes {
            wrong.push(format!("memory at 0x{addr:08X} = {got:02X?}, want {bytes:02X?}"));
        }
    }
    if wrong.is_empty() { Ok(()) } else { Err(wrong.join("; ")) }
}

fn words(v: u128) -> String {
    format!("[{:08X}, {:08X}, {:08X}, {:08X}]", (v >> 96) as u32, (v >> 64) as u32, (v >> 32) as u32, v as u32)
}

#[test]
fn xenia_instruction_vectors() {
    ppc_rt::install_function_table(&FUNCTIONS);
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut passed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut broken: Vec<String> = Vec::new();
    let mut todo: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for t in TESTS {
        if !t.errors.is_empty() {
            broken.push(format!("{} {}: {}", t.file, t.name, t.errors.join("; ")));
            continue;
        }
        if !t.todo.is_empty() {
            for op in t.todo {
                todo.entry(op).or_default().push(t.name);
            }
            continue;
        }
        match run(t) {
            Ok(()) => passed += 1,
            Err(e) => failed.push(format!("{} {}: {e}", t.file, t.name)),
        }
    }
    std::panic::set_hook(quiet);

    let untranslated = TESTS.iter().filter(|t| t.errors.is_empty() && !t.todo.is_empty()).count();
    eprintln!(
        "{} tests: {passed} passed, {} wrong, {untranslated} need untranslated instructions, {} unreadable, {} files skipped",
        TESTS.len(),
        failed.len(),
        broken.len(),
        SKIPPED.len()
    );
    for (f, why) in SKIPPED {
        eprintln!("  skipped {f}: {why}");
    }
    for (op, names) in &todo {
        eprintln!("  untranslated {op}: {} tests", names.len());
    }
    for b in &broken {
        eprintln!("  UNREADABLE {b}");
    }
    for f in &failed {
        eprintln!("  WRONG {f}");
    }
    assert!(broken.is_empty(), "{} tests couldn't be read or assembled", broken.len());
    assert!(failed.is_empty(), "{} tests produced wrong results", failed.len());
    if std::env::var("KAT_STRICT").is_ok_and(|v| v == "1") {
        assert_eq!(untranslated, 0, "tests still need untranslated instructions");
    }
}
