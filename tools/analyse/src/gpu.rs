//! Where the game talks to the GPU.
//!
//! Xenos is driven through PM4 command packets in memory. A type-3 packet
//! header is `3 << 30 | (count - 1) << 16 | opcode << 8 | predicate`, and code
//! that writes one forms that constant with `lis` + `ori`/`addi`. Every
//! function that does so (with the functions that touch MMIO or call the
//! GPU-facing `Vd*` imports) is part of the hardware layer under D3D, which
//! the port replaces natively: no command packet is ever executed or parsed
//! at run time. This module only finds that code, statically.

use std::collections::{BTreeMap, BTreeSet};

use ppc::Op;
use xex::Xex;

use crate::{Analysis, Code, Source};

/// PM4 type-3 opcode names (Xenos command processor).
pub fn opcode_name(op: u32) -> Option<&'static str> {
    Some(match op {
        0x10 => "NOP",
        0x21 => "REG_RMW",
        0x22 => "DRAW_INDX",
        0x23 => "VIZ_QUERY",
        0x26 => "WAIT_FOR_IDLE",
        0x27 => "IM_LOAD",
        0x2B => "IM_LOAD_IMMEDIATE",
        0x2D => "SET_CONSTANT",
        0x2F => "LOAD_ALU_CONSTANT",
        0x36 => "DRAW_INDX_2",
        0x37 => "INDIRECT_BUFFER_PFD",
        0x3B => "INVALIDATE_STATE",
        0x3C => "WAIT_REG_MEM",
        0x3D => "MEM_WRITE",
        0x3E => "REG_TO_MEM",
        0x3F => "INDIRECT_BUFFER",
        0x45 => "COND_WRITE",
        0x46 => "EVENT_WRITE",
        0x48 => "ME_INIT",
        0x4C => "FIX_2_FLT_REG",
        0x55 => "SET_CONSTANT2",
        0x57 => "INTERRUPT",
        0x58 => "EVENT_WRITE_SHD",
        0x59 => "EVENT_WRITE_EXT",
        0x5A => "EVENT_WRITE_ZPD",
        0x5E => "CONTEXT_UPDATE",
        0x60 => "SET_BIN_MASK_LO",
        0x61 => "SET_BIN_MASK_HI",
        0x62 => "SET_BIN_SELECT_LO",
        0x63 => "SET_BIN_SELECT_HI",
        _ => return None,
    })
}

/// GPU evidence in one function.
#[derive(Debug, Default, Clone)]
pub struct GpuUse {
    /// PM4 type-3 headers formed: opcode → count of sites.
    pub packets: BTreeMap<u32, usize>,
    /// `Vd*` and other GPU-facing imports called.
    pub imports: BTreeSet<String>,
    /// `lis` of an MMIO high half.
    pub mmio: usize,
}

impl GpuUse {
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.imports.is_empty() && self.mmio == 0
    }
}

/// Imports that drive the GPU: the video driver's (`Vd*`) and L2 locking,
/// except the queries and notifications that the native kernel implements as
/// plain API contracts (the video mode and flags, display gamma and
/// information, graphics notification routines, display persistence). Calling
/// those reaches no hardware. `MmGetPhysicalAddress` is ordinary address
/// translation, whoever uses the result: the code that would hand it to the
/// hardware is found by its packets or MMIO.
fn gpu_import(name: &str) -> bool {
    const NATIVE_QUERIES: [&str; 6] = [
        "VdQueryVideoMode",
        "VdQueryVideoFlags",
        "VdGetCurrentDisplayGamma",
        "VdGetCurrentDisplayInformation",
        "VdCallGraphicsNotificationRoutines",
        "VdPersistDisplay",
    ];
    (name.starts_with("Vd") && !NATIVE_QUERIES.contains(&name)) || matches!(name, "KeLockL2" | "KeUnlockL2")
}

/// Per-function GPU evidence, for every function that has some.
pub fn census(xex: &Xex, a: &Analysis) -> BTreeMap<u32, GpuUse> {
    let code = Code::new(xex);
    let mut out: BTreeMap<u32, GpuUse> = BTreeMap::new();
    for f in &a.discovery.functions {
        if matches!(f.source, Source::ImportThunk | Source::SecondaryEntry) {
            continue;
        }
        let embedded: Vec<(u32, u32)> =
            a.discovery.embedded.iter().copied().filter(|&(s, e)| s >= f.begin && e <= f.end).collect();
        let mut hi: [Option<u32>; 32] = [None; 32];
        let mut u = GpuUse::default();
        let mut p = f.begin;
        while p < f.end {
            if let Some(&(_, e)) = embedded.iter().find(|&&(s, e)| p >= s && p < e) {
                p = e;
                continue;
            }
            let Some(i) = code.instr(p) else {
                p += 4;
                continue;
            };
            // `ori rA, rS, imm` reads the RS field (decoded as `rt`); `addi rD, rA, simm` reads RA.
            let formed = match i.op {
                Op::ori => hi[i.rt() as usize].map(|h| h | i.uimm16()),
                Op::addi if i.ra() != 0 => hi[i.ra() as usize].map(|h| h.wrapping_add(i.simm16() as u32)),
                _ => None,
            };
            if let Some(v) = formed {
                let op = (v >> 8) & 0x7F;
                if v >> 30 == 3 && v & 0xFE == 0 && v & 0x8000 == 0 && opcode_name(op).is_some() {
                    *u.packets.entry(op).or_default() += 1;
                }
            }
            if i.op == Op::addis && i.ra() == 0 {
                let h = i.uimm16() as u16;
                hi[i.rt() as usize] = Some((h as u32) << 16);
                if crate::is_mmio_high(h) {
                    u.mmio += 1;
                }
            } else {
                for r in crate::code::gprs_written(&i) {
                    hi[r as usize] = None;
                }
            }
            if i.op == Op::bx && i.lk()
                && let Some(t) = i.branch_target()
                && let Some((_, name)) = a.import_thunks.get(&t)
                && gpu_import(name)
            {
                u.imports.insert(name.clone());
            }
            if crate::code::is_branch(&i) {
                hi = [None; 32];
            }
            p += 4;
        }
        if !u.is_empty() {
            out.insert(f.begin, u);
        }
    }
    out
}

/// Direct calls (`bl`) between functions: `(caller, callee)` pairs, one per site.
pub fn call_edges(xex: &Xex, a: &Analysis) -> Vec<(u32, u32)> {
    let code = Code::new(xex);
    let mut out = Vec::new();
    for f in &a.discovery.functions {
        if matches!(f.source, Source::ImportThunk | Source::SecondaryEntry) {
            continue;
        }
        let mut p = f.begin;
        while p < f.end {
            if let Some(i) = code.instr(p)
                && i.op == Op::bx
                && i.lk()
                && let Some(t) = i.branch_target()
            {
                out.push((f.begin, t));
            }
            p += 4;
        }
    }
    out
}

/// Calls and tail calls between functions: `bl` to any target, and `b` to
/// another function's entry. `(caller entry, callee)`.
pub fn call_and_tail_edges(xex: &Xex, a: &Analysis) -> Vec<(u32, u32)> {
    let code = Code::new(xex);
    let entries: BTreeSet<u32> = a.discovery.functions.iter().map(|f| f.begin).collect();
    let mut out = Vec::new();
    for f in &a.discovery.functions {
        if matches!(f.source, Source::ImportThunk | Source::SecondaryEntry) {
            continue;
        }
        let mut p = f.begin;
        while p < f.end {
            if let Some(i) = code.instr(p)
                && i.op == Op::bx
                && let Some(t) = i.branch_target()
                && (i.lk() || (t != f.begin && entries.contains(&t) && !(f.begin..f.end).contains(&t)))
            {
                out.push((f.begin, t));
            }
            p += 4;
        }
    }
    out
}

/// Functions whose address is taken: a word in a data section equal to a
/// function entry (vtables, callback tables), or an address formed in code with
/// `lis` + `addi`/`ori` (callbacks, thread starts, tables built at run time).
/// Any of them can be called indirectly from anywhere.
pub fn address_taken(xex: &Xex, a: &Analysis) -> BTreeSet<u32> {
    let code = Code::new(xex);
    let entries: BTreeSet<u32> = a
        .discovery
        .functions
        .iter()
        .filter(|f| !matches!(f.source, Source::SecondaryEntry))
        .map(|f| f.begin)
        .collect();
    let mut out = BTreeSet::new();
    // `.pdata` lists every function and `.reloc` every relocated word: neither
    // is a program reference.
    for s in xex.sections.iter().filter(|s| !s.is_code() && !matches!(s.name.as_str(), ".pdata" | ".reloc")) {
        let start = s.address & !3;
        let mut p = start;
        while p + 4 <= s.address + s.virtual_size.min(s.raw_size) {
            if let Some(b) = xex.bytes_at(p, 4) {
                let w = u32::from_be_bytes(b.try_into().unwrap());
                if entries.contains(&w) {
                    out.insert(w);
                }
            }
            p += 4;
        }
    }
    for f in &a.discovery.functions {
        if matches!(f.source, Source::ImportThunk | Source::SecondaryEntry) {
            continue;
        }
        let mut hi: [Option<u32>; 32] = [None; 32];
        let mut p = f.begin;
        while p < f.end {
            if let Some(i) = code.instr(p) {
                let formed = match i.op {
                    Op::addi if i.ra() != 0 => hi[i.ra() as usize].map(|h| h.wrapping_add(i.simm16() as u32)),
                    Op::ori => hi[i.rt() as usize].map(|h| h | i.uimm16()),
                    _ => None,
                };
                if let Some(v) = formed
                    && entries.contains(&v)
                {
                    out.insert(v);
                }
                if i.op == Op::addis && i.ra() == 0 {
                    hi[i.rt() as usize] = Some((i.uimm16()) << 16);
                } else {
                    for r in crate::code::gprs_written(&i) {
                        hi[r as usize] = None;
                    }
                }
                if crate::code::is_branch(&i) {
                    hi = [None; 32];
                }
            }
            p += 4;
        }
    }
    out
}

/// GPU-layer functions (from [`census`]) that recompiled code can still reach
/// without going through a replaced function: each with one call path from a
/// root (the entry point or an address-taken function). Replaced functions'
/// bodies never run, so the search doesn't continue through them.
pub fn reachable_gpu(
    xex: &Xex,
    a: &Analysis,
    census: &BTreeMap<u32, GpuUse>,
    replaced: &BTreeSet<u32>,
) -> Vec<(u32, Vec<u32>)> {
    let mut graph: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (from, to) in call_and_tail_edges(xex, a) {
        graph.entry(from).or_default().push(to);
    }
    let mut roots: Vec<u32> = xex.entry_point().into_iter().collect();
    roots.extend(address_taken(xex, a));
    let mut parent: BTreeMap<u32, u32> = BTreeMap::new();
    let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
    for r in roots {
        if parent.insert(r, r).is_none() {
            queue.push_back(r);
        }
    }
    while let Some(f) = queue.pop_front() {
        if replaced.contains(&f) {
            continue;
        }
        for &t in graph.get(&f).map(Vec::as_slice).unwrap_or(&[]) {
            if let std::collections::btree_map::Entry::Vacant(e) = parent.entry(t) {
                e.insert(f);
                queue.push_back(t);
            }
        }
    }
    census
        .keys()
        .filter(|f| parent.contains_key(f) && !replaced.contains(f))
        .map(|&f| {
            let mut path = vec![f];
            let mut cur = f;
            while let Some(&p) = parent.get(&cur) {
                if p == cur || path.len() > 64 {
                    break;
                }
                path.push(p);
                cur = p;
            }
            path.reverse();
            (f, path)
        })
        .collect()
}
