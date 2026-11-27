//! Differential test of the register analysis on real game code.
//!
//! Each function in `kat::GAME` was translated twice at build time: once with
//! every register in the context, once with registers in host locals
//! (`recomp::locals`). Both run here on the same inputs, and must leave the
//! same registers and the same memory. See `build/game.rs` for which
//! functions are taken and why they are sure to end.

use std::panic::{AssertUnwindSafe, catch_unwind};

use kat::{GAME, GuestFn};
use ppc_rt::{Cr, Ctx, GuestMemory};

/// Where the inputs point: a window of guest memory filled with data that
/// mostly points back into itself, so loads through loaded pointers stay there.
const BASE: u32 = 0x4000_0000;
const WINDOW: u32 = 0x4_0000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pointer(&mut self) -> u64 {
        (BASE + 0x8000 + (self.next() as u32 % (WINDOW - 0x1_0000)) & !7) as u64
    }

    fn value(&mut self) -> u64 {
        match self.next() % 4 {
            0 | 1 => self.pointer(),
            2 => self.next() % 64,
            _ => self.next() as u32 as u64,
        }
    }
}

/// Is this a single-precision NaN? (The high word of a double NaN is one too.)
fn nan32(w: u32) -> bool {
    w & 0x7F80_0000 == 0x7F80_0000 && w & 0x007F_FFFF != 0
}

/// Two values that are the same NaN up to its sign count as equal. The sign of
/// a NaN an operation makes up is not fixed by Rust or the host: the same
/// source compiled twice may give +NaN once and -NaN the other time, and the
/// two translations are two compilations.
fn same32(a: u32, b: u32) -> bool {
    a == b || (a ^ b == 0x8000_0000 && nan32(a))
}

fn same64(a: u64, b: u64) -> bool {
    a == b || (a ^ b == 1 << 63 && nan32((a >> 32) as u32))
}

/// A general register can hold a float's bits too (loaded from where one was
/// stored): either half may be a NaN.
fn same_gpr(a: u64, b: u64) -> bool {
    same32((a >> 32) as u32, (b >> 32) as u32) && same32(a as u32, b as u32)
}

fn same128(a: u128, b: u128) -> bool {
    (0..4).all(|k| same32((a >> (32 * k)) as u32, (b >> (32 * k)) as u32))
}

/// Everything a run leaves behind.
struct Outcome {
    r: [u64; 32],
    f: [u64; 32],
    v: [u128; 128],
    cr: [Cr; 8],
    lr: u64,
    ctr: u64,
    xer: (bool, bool, bool),
    reserved: u64,
    fpscr: u32,
    msr: u64,
    memory: u64,
}

fn run(f: GuestFn, address: u32, seed: u64, mem: &GuestMemory) -> Result<Outcome, String> {
    // The same start for every run: the stack and the lowest pages zeroed, the
    // window refilled below. The image stays as loaded (each translation has
    // its own copy of guest memory, so what one writes the other never sees).
    mem.write(0, &[0u8; 0x1_0000]);
    mem.write(BASE - 0x1_0000, &[0u8; 0x1_0000]);
    mem.write(BASE + WINDOW, &[0u8; 0x1_0000]);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((address as u64) << 20) ^ seed.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let mut fill = Vec::with_capacity(WINDOW as usize);
    while fill.len() < WINDOW as usize {
        let word = if rng.next() % 2 == 0 { rng.pointer() as u32 } else { (rng.next() % 256) as u32 };
        fill.extend_from_slice(&word.to_be_bytes());
    }
    mem.write(BASE, &fill);
    let mut c = Ctx::new();
    for n in 0..32 {
        c.r[n] = rng.value();
        c.f[n] = match rng.next() % 6 {
            0 => 0.0,
            1 => 1.0,
            2 => -2.5,
            3 => 1.0e-3,
            4 => 3.0e9,
            // Any finite double. No NaN or infinity goes in: which of two NaN
            // operands an operation hands on depends on the operand order the
            // compiler picked, so NaN inputs make the two compilations differ
            // for reasons that have nothing to do with the registers.
            _ => {
                let bits = rng.next();
                f64::from_bits(if bits >> 52 & 0x7FF == 0x7FF { bits & !(1 << 62) } else { bits })
            }
        };
    }
    // The stack sits just below the window, apart from everything the inputs
    // point to.
    c.r[1] = (BASE - 0x1000) as u64;
    // On odd seeds the registers a callee preserves start at zero. A function
    // that stores them in its frame then writes what the stack already holds,
    // so the frame looks the same to the translation that skips those stores:
    // code that reads a stale stack slot (a 16-byte load around a 4-byte
    // local, say) then gets the same bytes in both.
    if seed % 2 == 1 {
        for n in 14..32 {
            c.r[n] = 0;
            c.f[n] = 0.0;
            c.v[n] = 0;
        }
        for n in 64..128 {
            c.v[n] = 0;
        }
    }
    for n in 0..128 {
        // Four lanes, each a small integer or a finite float.
        c.v[n] = (0..4).fold(0u128, |v, _| {
            let bits = rng.next() as u32;
            let lane = match rng.next() % 2 {
                0 => bits % 256,
                _ if bits >> 23 & 0xFF == 0xFF => bits & !(1 << 30),
                _ => bits,
            };
            (v << 32) | lane as u128
        });
    }
    c.set_cr_word(0xFF, rng.next() as u32);
    c.ctr = rng.next() % 8;
    c.xer_ca = rng.next() % 2 == 0;
    c.reserved = rng.next() as u32 as u64;
    let base = mem.base();
    catch_unwind(AssertUnwindSafe(|| unsafe { f(&mut c, base) })).map_err(|p| {
        p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    })?;
    // The memory the inputs point into, and the lowest pages (where a null
    // pointer plus an offset lands). Not the stack: the frames of the function
    // and its callees are dead once it returns, and they differ by design. The
    // translation with locals never writes a preserved register's save slot.
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for (start, len) in [(0u32, 0x1_0000usize), (BASE, (WINDOW + 0x1_0000) as usize)] {
        for word in mem.read(start, len).chunks_exact(4) {
            // A stored NaN counts without its sign (see same32).
            let w = u32::from_be_bytes(word.try_into().unwrap());
            let w = if nan32(w) { w & 0x7FFF_FFFF } else { w };
            hash = (hash ^ w as u64).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    Ok(Outcome {
        r: c.r,
        f: c.f.map(f64::to_bits),
        v: c.v,
        cr: c.cr,
        lr: c.lr,
        ctr: c.ctr,
        xer: (c.xer_so, c.xer_ov, c.xer_ca),
        reserved: c.reserved,
        fpscr: c.fpscr,
        msr: c.msr,
        memory: hash,
    })
}

/// What differs between two outcomes: the vector registers, and everything else.
fn difference(a: &Outcome, b: &Outcome) -> (String, String) {
    let mut out = Vec::new();
    for n in 0..32 {
        if !same_gpr(a.r[n], b.r[n]) {
            out.push(format!("r{n} {:#X} / {:#X}", a.r[n], b.r[n]));
        }
        if !same64(a.f[n], b.f[n]) {
            out.push(format!("f{n} {:#X} / {:#X}", a.f[n], b.f[n]));
        }
    }
    let vectors: Vec<String> = (0..128).filter(|&n| !same128(a.v[n], b.v[n])).map(|n| format!("v{n}")).collect();
    for n in 0..8 {
        if a.cr[n] != b.cr[n] {
            out.push(format!("cr{n} {:?} / {:?}", a.cr[n], b.cr[n]));
        }
    }
    if a.lr != b.lr {
        out.push("lr".into());
    }
    if a.ctr != b.ctr {
        out.push(format!("ctr {:#X} / {:#X}", a.ctr, b.ctr));
    }
    if a.xer != b.xer {
        out.push(format!("xer {:?} / {:?}", a.xer, b.xer));
    }
    if a.reserved != b.reserved {
        out.push("reserved".into());
    }
    if a.fpscr != b.fpscr || a.msr != b.msr {
        out.push("fpscr/msr".into());
    }
    if a.memory != b.memory {
        out.push("memory".into());
    }
    (vectors.join(", "), out.join(", "))
}

#[test]
fn locals_match_context_on_game_functions() {
    if GAME.is_empty() {
        eprintln!("no game executable (private/game/default.xex): nothing to compare");
        return;
    }
    // The executable's image, so the functions find their constants: one copy
    // of guest memory for each translation.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../private/game/default.xex");
    let xex = xex::Xex::parse(&std::fs::read(path).expect(path)).expect("the executable parses");
    let memory = || {
        let mem = GuestMemory::new().expect("guest memory");
        mem.write(xex.base_address, &xex.image);
        mem
    };
    let (mem_context, mem_locals) = (memory(), memory());
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut wrong: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let (mut ran, mut stopped) = (0usize, 0usize);
    for &(address, context, locals) in GAME {
        for seed in 0..4 {
            ran += 1;
            match (run(context, address, seed, &mem_context), run(locals, address, seed, &mem_locals)) {
                (Ok(a), Ok(b)) => match difference(&a, &b) {
                    (vectors, rest) if vectors.is_empty() && rest.is_empty() => {}
                    // With random values in the preserved registers (even
                    // seeds) the two stacks differ where one translation
                    // stores them and the other doesn't. A 16-byte vector load
                    // around a smaller local picks that up in lanes the code
                    // never uses. Only vector registers can show it, and the
                    // odd seeds, where the stacks match, must agree in full.
                    (vectors, rest) if rest.is_empty() && seed % 2 == 0 => stale.push(format!("sub_{address:08X} seed {seed}: {vectors}")),
                    (vectors, rest) => wrong.push(format!("sub_{address:08X} seed {seed}: context / locals differ in {vectors} {rest}")),
                },
                // A fatal diagnostic (a guest trap, say) must be the same one in both.
                (Err(a), Err(b)) if a == b => stopped += 1,
                (a, b) => wrong.push(format!(
                    "sub_{address:08X} seed {seed}: context {}, locals {}",
                    a.err().map_or("returned".into(), |e| format!("stopped ({e})")),
                    b.err().map_or("returned".into(), |e| format!("stopped ({e})")),
                )),
            }
        }
    }
    std::panic::set_hook(quiet);
    eprintln!(
        "{} game functions, {ran} runs: {} agree ({stopped} stopped the same way in both), {} differ; \
         {} more differ only in vector lanes loaded from stale stack",
        GAME.len(),
        ran - wrong.len() - stale.len(),
        wrong.len(),
        stale.len()
    );
    for s in &stale {
        eprintln!("  stale stack: {s}");
    }
    for w in wrong.iter().take(40) {
        eprintln!("  DIFFER {w}");
    }
    assert!(wrong.is_empty(), "{} runs differ between the two translations", wrong.len());
}
