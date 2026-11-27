//! Support code for recompiled PowerPC functions.
//!
//! Every guest instruction is translated to Rust ahead of time. This crate gives
//! that generated code its register context, big-endian guest-memory access,
//! and the table for indirect calls. There is no interpreter and no fallback:
//! anything the recompiler didn't handle stops with a diagnostic (see CLAUDE.md).

#![allow(clippy::missing_safety_doc)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

mod vmx;
pub use vmx::*;

/// Signature of every recompiled function: guest register context and the host
/// pointer to guest address 0. The "C-unwind" ABI lets a `longjmp` (and later
/// SEH) unwind through recompiled frames, and lets a fatal diagnostic be caught
/// by a test harness instead of aborting.
pub type GuestFn = unsafe extern "C-unwind" fn(&mut Ctx, *mut u8);

/// One condition-register field.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Cr {
    pub lt: bool,
    pub gt: bool,
    pub eq: bool,
    pub so: bool,
}

impl Cr {
    /// The field as its four bits in the condition register: LT, GT, EQ, SO
    /// from the most significant.
    #[inline(always)]
    pub fn nibble(self) -> u32 {
        ((self.lt as u32) << 3) | ((self.gt as u32) << 2) | ((self.eq as u32) << 1) | self.so as u32
    }

    /// The field whose four bits are the low bits of `n`.
    #[inline(always)]
    pub fn from_nibble(n: u32) -> Cr {
        Cr { lt: n & 8 != 0, gt: n & 4 != 0, eq: n & 2 != 0, so: n & 1 != 0 }
    }
}

/// Guest register state, one per guest thread.
///
/// Recompiled functions keep most registers in host locals while they run
/// (`recomp`'s register analysis). What is always here: `r1`-`r10`, `r13`,
/// `f1`-`f13`, `v1`-`v13`, LR, MSR and FPSCR. The others are here only where
/// the analysis found a function hands them to another (a diagnostic dump of
/// them shows stale values otherwise).
///
/// Vector registers are kept as `u128` holding the 16 bytes in memory order,
/// big-endian. Word 0 (the lowest address) is the most significant 32 bits.
#[repr(C, align(16))]
pub struct Ctx {
    pub v: [u128; 128],
    pub r: [u64; 32],
    pub f: [f64; 32],
    pub cr: [Cr; 8],
    pub lr: u64,
    pub ctr: u64,
    pub xer_so: bool,
    pub xer_ov: bool,
    pub xer_ca: bool,
    /// Value loaded by the last `lwarx`/`ldarx` (reservation emulated by compare-and-swap).
    pub reserved: u64,
    pub fpscr: u32,
    pub msr: u64,
}

impl Ctx {
    pub fn new() -> Box<Ctx> {
        Box::new(Ctx {
            v: [0; 128],
            r: [0; 32],
            f: [0.0; 32],
            cr: [Cr::default(); 8],
            lr: 0,
            ctr: 0,
            xer_so: false,
            xer_ov: false,
            xer_ca: false,
            reserved: 0,
            fpscr: 0,
            msr: 0,
        })
    }

    pub fn xer(&self) -> u64 {
        ((self.xer_so as u64) << 31) | ((self.xer_ov as u64) << 30) | ((self.xer_ca as u64) << 29)
    }

    pub fn set_xer(&mut self, v: u64) {
        self.xer_so = v & (1 << 31) != 0;
        self.xer_ov = v & (1 << 30) != 0;
        self.xer_ca = v & (1 << 29) != 0;
    }

    pub fn cr_word(&self) -> u32 {
        self.cr.iter().fold(0u32, |acc, f| (acc << 4) | f.nibble())
    }

    /// Condition register bit `n` (0 = CR0[LT], 31 = CR7[SO]).
    #[inline(always)]
    pub fn cr_bit(&self, n: u32) -> bool {
        let f = &self.cr[(n / 4) as usize];
        [f.lt, f.gt, f.eq, f.so][(n % 4) as usize]
    }

    #[inline(always)]
    pub fn set_cr_bit(&mut self, n: u32, v: bool) {
        let f = &mut self.cr[(n / 4) as usize];
        match n % 4 {
            0 => f.lt = v,
            1 => f.gt = v,
            2 => f.eq = v,
            _ => f.so = v,
        }
    }

    /// `mtcrf`: update the fields selected by `mask` (bit 7 = CR0) from `value`.
    pub fn set_cr_word(&mut self, mask: u32, value: u32) {
        for (n, field) in self.cr.iter_mut().enumerate() {
            if mask & (0x80 >> n) != 0 {
                *field = Cr::from_nibble(value >> (28 - 4 * n));
            }
        }
    }
}

// ---- Guest memory ---------------------------------------------------------
//
// Accesses are volatile so the compiler can't cache guest memory in registers
// across loops that another guest thread is updating (spin-waits). The packed
// wrapper has alignment 1, so unaligned guest accesses are well defined and
// still compile to single loads/stores on x86-64.

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Unaligned<T: Copy>(T);

macro_rules! mem_access {
    ($ld:ident, $st:ident, $t:ty) => {
        #[inline(always)]
        pub unsafe fn $ld(base: *mut u8, ea: u32) -> $t {
            unsafe { <$t>::from_be(core::ptr::read_volatile(base.add(ea as usize) as *const Unaligned<$t>).0) }
        }
        #[inline(always)]
        pub unsafe fn $st(base: *mut u8, ea: u32, v: $t) {
            unsafe { core::ptr::write_volatile(base.add(ea as usize) as *mut Unaligned<$t>, Unaligned(v.to_be())) }
        }
    };
}

mem_access!(ld8, st8, u8);
mem_access!(ld16, st16, u16);
mem_access!(ld32, st32, u32);
mem_access!(ld64, st64, u64);
mem_access!(ldv, stv, u128);

/// Byte-reversed loads and stores (`lwbrx`, `sthbrx`, ...).
#[inline(always)]
pub unsafe fn ld32r(base: *mut u8, ea: u32) -> u32 {
    unsafe { ld32(base, ea).swap_bytes() }
}
#[inline(always)]
pub unsafe fn ld16r(base: *mut u8, ea: u32) -> u16 {
    unsafe { ld16(base, ea).swap_bytes() }
}
#[inline(always)]
pub unsafe fn st32r(base: *mut u8, ea: u32, v: u32) {
    unsafe { st32(base, ea, v.swap_bytes()) }
}
#[inline(always)]
pub unsafe fn st16r(base: *mut u8, ea: u32, v: u16) {
    unsafe { st16(base, ea, v.swap_bytes()) }
}

/// `lvlx`: the bytes from `ea` to the end of its 16-byte block, moved to the
/// left (low addresses) of the register; the rest is zero.
#[inline(always)]
pub unsafe fn ldvl(base: *mut u8, ea: u32) -> u128 {
    unsafe { ldv(base, ea & !15) << (8 * (ea & 15)) }
}

/// `lvrx`: the bytes of `ea`'s 16-byte block that come before `ea`, moved to
/// the right of the register; the rest is zero.
#[inline(always)]
pub unsafe fn ldvr(base: *mut u8, ea: u32) -> u128 {
    let n = ea & 15;
    if n == 0 { 0 } else { unsafe { ldv(base, ea & !15) >> (8 * (16 - n)) } }
}

/// `stvlx`: stores the leftmost bytes of `v` from `ea` to the end of its block.
#[inline(always)]
pub unsafe fn stvl(base: *mut u8, ea: u32, v: u128) {
    let bytes = v.to_be_bytes();
    let n = (16 - (ea & 15)) as usize;
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(ea as usize), n) }
}

/// `stvrx`: stores the rightmost `ea & 15` bytes of `v` before `ea` in its block.
#[inline(always)]
pub unsafe fn stvr(base: *mut u8, ea: u32, v: u128) {
    let bytes = v.to_be_bytes();
    let n = (ea & 15) as usize;
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr().add(16 - n), base.add((ea & !15) as usize), n) }
}

/// `lvsl`: the permute control for a left shift by `ea & 15` bytes.
#[inline(always)]
pub fn lvsl(ea: u32) -> u128 {
    let sh = (ea & 15) as u8;
    u128::from_be_bytes(core::array::from_fn(|i| sh + i as u8))
}

/// `lvsr`: the permute control for a right shift by `ea & 15` bytes.
#[inline(always)]
pub fn lvsr(ea: u32) -> u128 {
    let sh = (ea & 15) as u8;
    u128::from_be_bytes(core::array::from_fn(|i| 16 - sh + i as u8))
}

/// Effective address `(ra) + d` in the 32-bit guest address space.
#[inline(always)]
pub fn ea(ra: u64, d: u32) -> u32 {
    (ra as u32).wrapping_add(d)
}

/// `dcbz` / `dcbz128`: zero one cache line.
#[inline(always)]
pub unsafe fn zero_line(base: *mut u8, ea: u32, size: u32) {
    unsafe { core::ptr::write_bytes(base.add((ea & !(size - 1)) as usize), 0, size as usize) }
}

/// `stwcx.`: store `new` if memory still holds the value `lwarx` loaded.
#[inline(always)]
pub unsafe fn cas32(base: *mut u8, ea: u32, expected: u32, new: u32) -> bool {
    let p = unsafe { &*(base.add(ea as usize) as *const AtomicU32) };
    p.compare_exchange(expected.to_be(), new.to_be(), Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// `stdcx.`
#[inline(always)]
pub unsafe fn cas64(base: *mut u8, ea: u32, expected: u64, new: u64) -> bool {
    let p = unsafe { &*(base.add(ea as usize) as *const AtomicU64) };
    p.compare_exchange(expected.to_be(), new.to_be(), Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

// ---- Guest address space ------------------------------------------------------

/// The guest's 4 GiB address space as one host allocation, so guest address
/// `a` is host address `base + a`. Pages are zero until first touched, so
/// untouched guest memory costs address space but no RAM.
///
/// This is the address-space half of the kernel's memory API. Protection,
/// commit-on-allocate and the physical-memory views come with the kernel
/// (Phase 3).
pub struct GuestMemory {
    base: *mut u8,
}

/// 4 GiB plus slack, so a 16-byte access at the very top doesn't run off the end.
const GUEST_SPAN: usize = (1usize << 32) + 0x1_0000;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(addr: *mut core::ffi::c_void, size: usize, kind: u32, protect: u32) -> *mut core::ffi::c_void;
    fn VirtualFree(addr: *mut core::ffi::c_void, size: usize, kind: u32) -> i32;
}

impl GuestMemory {
    #[cfg(windows)]
    pub fn new() -> std::io::Result<GuestMemory> {
        const MEM_COMMIT: u32 = 0x1000;
        const MEM_RESERVE: u32 = 0x2000;
        const PAGE_READWRITE: u32 = 0x04;
        let p = unsafe { VirtualAlloc(core::ptr::null_mut(), GUEST_SPAN, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        if p.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(GuestMemory { base: p.cast() })
    }

    #[cfg(not(windows))]
    pub fn new() -> std::io::Result<GuestMemory> {
        let layout = std::alloc::Layout::from_size_align(GUEST_SPAN, 1 << 16).unwrap();
        let p = unsafe { std::alloc::alloc_zeroed(layout) };
        if p.is_null() {
            return Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory));
        }
        Ok(GuestMemory { base: p })
    }

    /// Host pointer to guest address 0, the `b` argument of every recompiled function.
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    pub fn write(&self, addr: u32, bytes: &[u8]) {
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(addr as usize), bytes.len()) }
    }

    pub fn read(&self, addr: u32, len: usize) -> Vec<u8> {
        let mut out = vec![0; len];
        unsafe { core::ptr::copy_nonoverlapping(self.base.add(addr as usize), out.as_mut_ptr(), len) };
        out
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            const MEM_RELEASE: u32 = 0x8000;
            VirtualFree(self.base.cast(), 0, MEM_RELEASE);
        }
        #[cfg(not(windows))]
        unsafe {
            std::alloc::dealloc(self.base, std::alloc::Layout::from_size_align(GUEST_SPAN, 1 << 16).unwrap());
        }
    }
}

// ---- Integer helpers --------------------------------------------------------

#[inline(always)]
pub fn cmp_s(a: i64, b: i64, so: bool) -> Cr {
    Cr { lt: a < b, gt: a > b, eq: a == b, so }
}

#[inline(always)]
pub fn cmp_u(a: u64, b: u64, so: bool) -> Cr {
    Cr { lt: a < b, gt: a > b, eq: a == b, so }
}

/// FPU compare: unordered (NaN) sets the fourth bit.
#[inline(always)]
pub fn cmp_f(a: f64, b: f64) -> Cr {
    Cr { lt: a < b, gt: a > b, eq: a == b, so: a.is_nan() || b.is_nan() }
}

/// `fctiwz`: truncate to a 32-bit integer, saturating; NaN gives `i32::MIN`.
#[inline(always)]
pub fn fctiwz(x: f64) -> i32 {
    if x.is_nan() { i32::MIN } else { x as i32 }
}

/// `fctid`: round to a 64-bit integer in the current mode (always
/// round-to-nearest-even; see [`set_fpscr`]), saturating; NaN gives `i64::MIN`.
#[inline(always)]
pub fn fctid(x: f64) -> i64 {
    if x.is_nan() { i64::MIN } else { x.round_ties_even() as i64 }
}

/// `mtfsf`: writes FPSCR. Recompiled code computes with the host's default
/// rounding (to nearest) and no FP exceptions, so a request for another
/// rounding mode, non-IEEE mode or enabled exceptions can't be honoured and
/// stops here with a diagnostic instead of silently computing differently.
#[cold]
pub fn set_fpscr(c: &mut Ctx, value: u32) {
    const RN: u32 = 0x3;
    const NI: u32 = 0x4;
    const ENABLES: u32 = 0xF8;
    if value & (RN | NI | ENABLES) != 0 {
        panic!(
            "FATAL: guest set FPSCR to 0x{value:08X} (rounding mode {}, non-IEEE {}, exception enables 0x{:02X}); \
             only round-to-nearest with exceptions masked is supported",
            value & RN,
            value & NI != 0,
            (value & ENABLES) >> 3
        );
    }
    c.fpscr = value;
}

/// `fctidz`: truncate to a 64-bit integer, saturating; NaN gives `i64::MIN`.
#[inline(always)]
pub fn fctidz(x: f64) -> i64 {
    if x.is_nan() { i64::MIN } else { x as i64 }
}

/// Mask with IBM bits `mb..=me` set (bit 0 = MSB), wrapping when `mb > me`.
#[inline(always)]
pub const fn mask64(mb: u32, me: u32) -> u64 {
    let a = u64::MAX >> mb;
    let b = u64::MAX << (63 - me);
    if mb <= me { a & b } else { a | b }
}

/// 32-bit rotate as PPC64 defines it for `rlw*`: the low word duplicated into both halves.
#[inline(always)]
pub fn rotl32(x: u64, sh: u32) -> u64 {
    let lo = x as u32 as u64;
    (lo | (lo << 32)).rotate_left(sh)
}

// ---- Time ------------------------------------------------------------------

/// The 360's time base runs at 49.875 MHz. `mftb` reads the host monotonic
/// clock scaled to that rate (a host clock API, not hardware emulation).
pub fn timebase() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let ns = START.get_or_init(std::time::Instant::now).elapsed().as_nanos();
    (ns * 49_875_000 / 1_000_000_000) as u64
}

// ---- Indirect calls ---------------------------------------------------------

static FUNCTIONS: OnceLock<&'static [(u32, GuestFn)]> = OnceLock::new();

/// Every recompiled function by guest address, one slot per instruction word
/// from the first function to the last (about 4.7 million slots, 38 MB): an
/// indirect call is one load. The game's C++ makes virtual calls everywhere
/// (a sort calls its comparator through one per comparison), and a binary
/// search of the 83,000 functions cost about 17 dependent loads each
/// (2026-11-25).
struct Direct {
    base: u32,
    slots: Box<[Option<GuestFn>]>,
}

static DIRECT: OnceLock<Direct> = OnceLock::new();

/// Installs the table of every recompiled function, sorted by guest address.
pub fn install_function_table(table: &'static [(u32, GuestFn)]) {
    assert!(table.windows(2).all(|w| w[0].0 < w[1].0), "function table must be sorted and unique");
    FUNCTIONS.set(table).ok().expect("function table installed twice");
    if let (Some(first), Some(last)) = (table.first(), table.last()) {
        let base = first.0 & !3;
        let mut slots = vec![None; ((last.0 - base) / 4 + 1) as usize].into_boxed_slice();
        for &(addr, f) in table {
            // Functions start on instruction words; any other address keeps
            // the binary search (none does in this game).
            if addr & 3 == 0 {
                slots[((addr - base) / 4) as usize] = Some(f);
            }
        }
        let _ = DIRECT.set(Direct { base, slots });
    }
}

/// Native functions given guest addresses at run time, outside the image: the
/// kernel's answers to `XexGetProcedureAddress`. Consulted only when the
/// recompiled table misses.
static EXTRA: std::sync::RwLock<Vec<(u32, GuestFn)>> = std::sync::RwLock::new(Vec::new());

/// Makes `f` callable through guest address `addr` (which must not be a
/// recompiled function's address).
pub fn register_function(addr: u32, f: GuestFn) {
    assert!(FUNCTIONS.get().is_none_or(|t| t.binary_search_by_key(&addr, |e| e.0).is_err()));
    let mut extra = EXTRA.write().unwrap();
    match extra.binary_search_by_key(&addr, |e| e.0) {
        Ok(i) => extra[i].1 = f,
        Err(i) => extra.insert(i, (addr, f)),
    }
}

/// The recompiled function at `target`, from the direct table.
#[inline(always)]
fn lookup_direct(target: u32) -> Option<GuestFn> {
    let d = DIRECT.get()?;
    if target & 3 != 0 {
        return None;
    }
    *d.slots.get((target.wrapping_sub(d.base) / 4) as usize)?
}

#[inline]
pub fn lookup(target: u32) -> Option<GuestFn> {
    match lookup_direct(target) {
        Some(f) => Some(f),
        None => lookup_slow(target),
    }
}

/// Addresses outside the direct table, or not word-aligned: the sorted table,
/// then native functions registered at run time.
#[cold]
fn lookup_slow(target: u32) -> Option<GuestFn> {
    let table = FUNCTIONS.get()?;
    match table.binary_search_by_key(&target, |e| e.0) {
        Ok(i) => Some(table[i].1),
        Err(_) => lookup_extra(target),
    }
}

#[cold]
fn lookup_extra(target: u32) -> Option<GuestFn> {
    let extra = EXTRA.read().unwrap();
    extra.binary_search_by_key(&target, |e| e.0).ok().map(|i| extra[i].1)
}

/// A recompiled function returned with a register the ABI has it preserve
/// changed. The register analysis took it to restore the register without
/// being able to prove it (`recomp::locals`), so its callers kept their own
/// copies; going on would run them with the wrong value.
#[cold]
#[inline(never)]
pub fn not_preserved(function: u32, regs: &str) {
    panic!(
        "FATAL: sub_{function:08X} returned with a preserved register changed (one of {regs}). The register analysis \
         took it to restore them. Fix it statically: list it under [locals] untrusted in the config; there is no fallback."
    );
}

// ---- Imports ------------------------------------------------------------------
//
// Every imported kernel/XAM function has a thunk in the image. The recompiler
// turns each thunk into a stub that calls `call_import` with the import's
// index; the kernel installs the native implementations by name at boot.

/// One imported function, as listed by the recompiler.
#[derive(Debug, Clone, Copy)]
pub struct ImportDef {
    /// Guest address of the import thunk.
    pub thunk: u32,
    pub library: &'static str,
    pub name: &'static str,
    pub ordinal: u16,
}

struct ImportTable {
    defs: &'static [ImportDef],
    fns: Box<[Option<GuestFn>]>,
    trace: Option<fn(&ImportDef, &mut Ctx, *mut u8, GuestFn)>,
}

static IMPORTS: OnceLock<ImportTable> = OnceLock::new();

/// Installs the native implementation of each import (`None` = not
/// implemented; calling it is fatal). `trace`, if given, wraps every call.
pub fn install_imports(
    defs: &'static [ImportDef],
    fns: Vec<Option<GuestFn>>,
    trace: Option<fn(&ImportDef, &mut Ctx, *mut u8, GuestFn)>,
) {
    assert_eq!(defs.len(), fns.len());
    IMPORTS.set(ImportTable { defs, fns: fns.into_boxed_slice(), trace }).ok().expect("imports installed twice");
}

// ---- Native replacements ----------------------------------------------------------------
//
// A `[[replace]]` entry in the recompiler config turns a guest function into a
// stub that calls a native implementation, installed by name at boot. The
// original recompiled body stays available (`__imp__sub_X`), so a replacement
// can still run part of it.

/// One replaced function, as listed by the recompiler.
#[derive(Debug, Clone, Copy)]
pub struct ReplaceDef {
    pub address: u32,
    pub name: &'static str,
    /// The original function's recompiled body.
    pub original: GuestFn,
}

struct ReplaceTable {
    defs: &'static [ReplaceDef],
    fns: Box<[Option<GuestFn>]>,
}

static REPLACEMENTS: OnceLock<ReplaceTable> = OnceLock::new();

/// Installs the native implementation of each replaced function (`None` =
/// not implemented yet; calling it is fatal).
pub fn install_replacements(defs: &'static [ReplaceDef], fns: Vec<Option<GuestFn>>) {
    assert_eq!(defs.len(), fns.len());
    REPLACEMENTS.set(ReplaceTable { defs, fns: fns.into_boxed_slice() }).ok().expect("replacements installed twice");
}

/// Body of every replaced function's stub.
#[inline]
pub unsafe fn call_replacement(c: &mut Ctx, base: *mut u8, index: usize) {
    let Some(t) = REPLACEMENTS.get() else { return unreplaced("?", 0) };
    match t.fns[index] {
        Some(f) => unsafe { f(c, base) },
        None => unreplaced(t.defs[index].name, t.defs[index].address),
    }
}

/// The original body of the replaced function at `address`.
pub fn original(address: u32) -> Option<GuestFn> {
    let t = REPLACEMENTS.get()?;
    t.defs.iter().find(|d| d.address == address).map(|d| d.original)
}

#[cold]
#[inline(never)]
pub fn unreplaced(name: &str, address: u32) {
    panic!(
        "FATAL: {name} (0x{address:08X}) is replaced by native code in the config, \
         but no native implementation is registered yet"
    );
}

/// Body of every import thunk stub.
#[inline]
pub unsafe fn call_import(c: &mut Ctx, base: *mut u8, index: usize) {
    let Some(t) = IMPORTS.get() else { return unimplemented_import("?", "(imports not installed)") };
    match t.fns[index] {
        Some(f) => match t.trace {
            None => unsafe { f(c, base) },
            Some(trace) => trace(&t.defs[index], c, base, f),
        },
        None => unimplemented_import(t.defs[index].library, t.defs[index].name),
    }
}

/// Calls the recompiled function at guest address `target`: virtual calls,
/// function pointers and tail calls through CTR.
#[inline]
pub unsafe fn call_indirect(c: &mut Ctx, base: *mut u8, target: u32) {
    match lookup_direct(target) {
        Some(f) => unsafe { f(c, base) },
        None => unsafe { call_indirect_slow(c, base, target) },
    }
}

/// A target outside the direct table: a native function registered at run
/// time, or nothing. Recompiled callers keep CTR in a host local, so the
/// context gets the target here: natives handed out by address (the kernel's
/// answers to `XexGetProcedureAddress`) read it to tell which one was called.
#[cold]
#[inline(never)]
unsafe fn call_indirect_slow(c: &mut Ctx, base: *mut u8, target: u32) {
    match lookup_slow(target) {
        Some(f) => {
            c.ctr = target as u64;
            unsafe { f(c, base) }
        }
        None => no_function(target),
    }
}

/// The indirect-call table's entry for a recompiled function that takes
/// registers from its caller outside the calling convention (the register
/// analysis found it, and found no pointer to it): a call through a pointer
/// would not supply them.
#[cold]
#[inline(never)]
pub fn not_indirect(target: u32, why: &str) {
    panic!(
        "FATAL: indirect call to sub_{target:08X}, which the register analysis did not expect to be called \
         through a pointer: {why}. Fix it statically (list it under [locals] indirect in the config); there is no fallback."
    );
}

// ---- setjmp / longjmp ------------------------------------------------------------
//
// Guest `setjmp` runs as recompiled code and fills the guest jmp_buf. Guest
// `longjmp` also runs as recompiled code: it restores the saved registers,
// stack pointer and LR, and its final `blr` (which would only return to its
// caller) is translated to `longjmp_unwind`. That unwinds the host stack to the
// recompiled function that called `setjmp`, which resumes after the call.

/// Unwind payload for a guest `longjmp`.
#[derive(Debug)]
pub struct LongJmp {
    /// Stack pointer restored from the jmp_buf: the frame that called setjmp.
    pub r1: u32,
    /// Return address restored from the jmp_buf: just after the setjmp call.
    pub target: u32,
}

/// Translation of `longjmp`'s final `blr`.
#[cold]
pub fn longjmp_unwind(c: &mut Ctx) {
    std::panic::resume_unwind(Box::new(LongJmp { r1: c.r[1] as u32, target: c.lr as u32 }));
}

/// Called by a function that calls `setjmp` when an unwind reaches it. Returns
/// the block to resume at if the unwind is a longjmp to this activation (its
/// stack pointer and one of its setjmp return addresses); otherwise keeps
/// unwinding.
#[cold]
pub fn longjmp_target(c: &Ctx, payload: Box<dyn std::any::Any + Send>, frame: u32, sites: &[u32]) -> u32 {
    if let Some(j) = payload.downcast_ref::<LongJmp>() {
        if j.r1 == frame && j.r1 == c.r[1] as u32 && sites.contains(&j.target) {
            return j.target;
        }
    }
    std::panic::resume_unwind(payload)
}

// ---- Fatal diagnostics --------------------------------------------------------
//
// These are deliberately not `-> !` so the optimizer treats them like any other
// call. That keeps the spike's compile-time measurements honest.

#[cold]
#[inline(never)]
pub fn no_function(target: u32) {
    panic!(
        "FATAL: indirect call to 0x{target:08X}, which is not a recompiled function. \
         Fix it statically (analysis or config); there is no fallback."
    );
}

#[cold]
#[inline(never)]
pub fn todo_instr(addr: u32, word: u32) {
    panic!("FATAL: instruction 0x{word:08X} at 0x{addr:08X} is not implemented by the recompiler yet");
}

#[cold]
#[inline(never)]
pub fn bad_pc(pc: u32) {
    panic!("FATAL: control reached 0x{pc:08X}, which is not a block start in its function");
}

#[cold]
#[inline(never)]
pub fn fell_off(addr: u32) {
    panic!("FATAL: execution ran past the end of the function at 0x{addr:08X}");
}

#[cold]
#[inline(never)]
pub fn trap(addr: u32) {
    panic!("FATAL: guest trap (assertion) at 0x{addr:08X}");
}

#[cold]
#[inline(never)]
pub fn unimplemented_import(library: &str, name: &str) {
    panic!("FATAL: imported function {library}!{name} has no native implementation yet");
}


#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C-unwind" fn fa(_: &mut Ctx, _: *mut u8) {}
    unsafe extern "C-unwind" fn fb(_: &mut Ctx, _: *mut u8) {}
    unsafe extern "C-unwind" fn fc(_: &mut Ctx, _: *mut u8) {}

    /// The direct table finds every function, misses everything else, and
    /// natives registered at run time still resolve through the fallback.
    #[test]
    fn indirect_lookup() {
        static TABLE: [(u32, GuestFn); 3] = [(0x8200_0000, fa), (0x8200_0010, fb), (0x8200_1000, fc)];
        install_function_table(&TABLE);
        let is = |t: u32, f: GuestFn| lookup(t).is_some_and(|g| g as usize == f as usize);
        assert!(is(0x8200_0000, fa) && is(0x8200_0010, fb) && is(0x8200_1000, fc));
        for miss in [0x8200_0004, 0x8200_0011, 0x81FF_FFFC, 0x8200_1004, 0, u32::MAX] {
            assert!(lookup(miss).is_none(), "{miss:08X}");
        }
        register_function(0x9000_0000, fb);
        assert!(is(0x9000_0000, fb));
    }

    /// The shuffle form of `vperm` gives the byte-by-byte answer for every
    /// control value in every position, and for arbitrary operands.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn vperm_shuffle_matches_bytes() {
        if !(std::arch::is_x86_feature_detected!("ssse3") && std::arch::is_x86_feature_detected!("sse4.1")) {
            return;
        }
        let a = 0x0001_0203_0405_0607_0809_0A0B_0C0D_0E0Fu128;
        let b = 0x1011_1213_1415_1617_1819_1A1B_1C1D_1E1Fu128;
        for value in 0..=255u8 {
            for position in 0..16 {
                let c = (value as u128) << (8 * position) | 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10u128 & !(0xFFu128 << (8 * position));
                assert_eq!(unsafe { vperm_shuffle(a, b, c) }, vperm_bytes(a, b, c), "control {c:032X}");
            }
        }
        // A fixed-seed generator: arbitrary operands and controls.
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            let mut v = 0u128;
            for _ in 0..2 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                v = (v << 64) | s as u128;
            }
            v
        };
        for _ in 0..20_000 {
            let (a, b, c) = (next(), next(), next());
            assert_eq!(unsafe { vperm_shuffle(a, b, c) }, vperm_bytes(a, b, c));
        }
    }

    #[test]
    fn masks_match_ibm_numbering() {
        assert_eq!(mask64(32, 63), 0x0000_0000_FFFF_FFFF);
        assert_eq!(mask64(0, 0), 0x8000_0000_0000_0000);
        assert_eq!(mask64(63, 0), 0x8000_0000_0000_0001);
        // rlwinm r0, r3, 2, 0, 29 (slwi 2): mask over the low word's bits 0..29
        assert_eq!(rotl32(0x1234_5678, 2) & mask64(32, 61), 0x48D1_59E0);
    }

    #[test]
    fn memory_is_big_endian_and_unaligned_safe() {
        let mut mem = vec![0u8; 64];
        let b = mem.as_mut_ptr();
        unsafe {
            st32(b, 1, 0x1122_3344);
            assert_eq!(&mem[1..5], &[0x11, 0x22, 0x33, 0x44]);
            assert_eq!(ld32(b, 1), 0x1122_3344);
            assert_eq!(ld16(b, 2), 0x2233);
            stv(b, 16, 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF);
            assert_eq!(mem[16], 0x00);
            assert_eq!(mem[31], 0xFF);
        }
    }

    #[test]
    fn cr_word_round_trip() {
        let mut c = Ctx::new();
        c.set_cr_word(0xFF, 0x8421_0000);
        assert_eq!(c.cr[0], Cr { lt: true, gt: false, eq: false, so: false });
        assert_eq!(c.cr[3], Cr { lt: false, gt: false, eq: false, so: true });
        assert_eq!(c.cr_word(), 0x8421_0000);
    }
}
