//! Static analysis of the game executable for the recompiler: ABI helpers,
//! switch tables, indirect-branch classification, function boundaries, import
//! thunks, and lists of code needing special handling (MMIO, SEH, longjmp).
//!
//! Reads the XEX purely as data. Nothing here executes guest code. The
//! recompiler calls [`analyse`] directly; the `analyse` binary prints and
//! exports the same results for review.

pub mod code;
pub mod gpu;
pub mod coverage;
pub mod functions;
pub mod helpers;
pub mod remap;
pub mod switches;

use std::collections::BTreeMap;

use ppc::Op;
use xex::Xex;

pub use code::Code;
pub use functions::{DataKind, Discovery, Function, Source};
pub use helpers::Helper;
pub use switches::{BctrKind, BctrSite, SwitchTable};

/// Everything the recompiler needs to know about the executable's code.
pub struct Analysis {
    pub helpers: Vec<Helper>,
    pub bctr_sites: Vec<BctrSite>,
    pub discovery: Discovery,
    /// Import thunk address → `(library, function name)`.
    pub import_thunks: BTreeMap<u32, (String, String)>,
}

impl Analysis {
    pub fn switch_tables(&self) -> impl Iterator<Item = &SwitchTable> {
        self.bctr_sites.iter().filter_map(|s| match &s.kind {
            BctrKind::Switch(t) => Some(t),
            _ => None,
        })
    }

    /// `bctr` sites the analysis couldn't classify. The recompiler refuses to
    /// build while any remain.
    pub fn unclassified_bctr(&self) -> impl Iterator<Item = &BctrSite> {
        self.bctr_sites
            .iter()
            .filter(|s| matches!(s.kind, BctrKind::Unknown { .. } | BctrKind::TableWithoutBound { .. }))
    }
}

pub fn analyse(xex: &Xex) -> Result<Analysis, String> {
    let code = Code::new(xex);
    let pdata = xex.pdata().map_err(|e| e.to_string())?;
    let helpers = helpers::find(&code)?;
    let bctr_sites = switches::find_all(&code);
    let tables: Vec<SwitchTable> = bctr_sites
        .iter()
        .filter_map(|s| match &s.kind {
            BctrKind::Switch(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let mut import_thunks = BTreeMap::new();
    for lib in &xex.imports {
        for r in lib.functions() {
            import_thunks.insert(r.address, (lib.name.clone(), r.display_name()));
        }
    }
    let thunk_addrs: Vec<u32> = import_thunks.keys().copied().collect();
    let discovery = functions::discover(&code, &pdata, &helpers, &tables, &thunk_addrs);
    Ok(Analysis { helpers, bctr_sites, discovery, import_thunks })
}

/// Functions containing `lis rX, hi` where `hi` is the high half of a hardware
/// register address: the GPU registers (`0x7FC8`..`0x7FCF`) or the audio/XMA
/// block (`0x7FEA`). Other `0x7Fxx` values are ordinary constants: `0x7FC0` and
/// `0x7FF0` are the high words of float and double NaN/infinity, `0x7FFF` is
/// usually `INT_MAX`.
/// These functions must be replaced by native code. Under the no-emulation rule,
/// recompiled code never touches hardware.
/// High halves of the hardware register windows that recompiled code must never touch.
pub fn is_mmio_high(hi: u16) -> bool {
    (0x7FC8..=0x7FCF).contains(&hi) || hi == 0x7FEA
}

pub fn mmio_functions(xex: &Xex, discovery: &Discovery) -> BTreeMap<u32, Vec<(u32, u16)>> {
    let code = Code::new(xex);
    let mut out: BTreeMap<u32, Vec<(u32, u16)>> = BTreeMap::new();
    for (addr, word) in code.words() {
        let Some(i) = ppc::Instr::decode(addr, word) else { continue };
        if i.op == Op::addis && i.ra() == 0 {
            let hi = i.uimm16() as u16;
            if is_mmio_high(hi)
                && let Some(f) = discovery.function_at(addr)
            {
                out.entry(f.begin).or_default().push((addr, hi));
            }
        }
    }
    out
}

/// Functions that call the given import thunk.
pub fn callers_of(xex: &Xex, discovery: &Discovery, thunk: u32) -> Vec<(u32, u32)> {
    let code = Code::new(xex);
    code.words()
        .filter_map(|(a, w)| ppc::Instr::decode(a, w))
        .filter(|i| i.op == Op::bx && i.lk() && i.branch_target() == Some(thunk))
        .filter_map(|i| discovery.function_at(i.address).map(|f| (f.begin, i.address)))
        .collect()
}

/// Returns that switch stacks: an unconditional `blr` whose block last set r1
/// by loading it through a register other than r1. A normal epilogue restores
/// r1 from its own frame (`addi r1, r1, N` or `lwz r1, 0(r1)`); this pattern
/// instead jumps to a frame further up the stack, as `longjmp` does. The
/// recompiler turns each such return into a native unwind, and refuses to
/// build if one isn't listed in the config.
pub fn stack_switch_returns(xex: &Xex, discovery: &Discovery) -> Vec<u32> {
    let code = Code::new(xex);
    let mut out = Vec::new();
    for f in &discovery.functions {
        let mut a = f.begin;
        while a < f.end {
            let Some(i) = code.instr(a) else {
                a += 4;
                continue;
            };
            if i.op == Op::bclrx && !i.lk() && code::bo_always(&i) {
                // Walk back through the block to the last write of r1.
                let mut p = a;
                while p > f.begin {
                    p -= 4;
                    let Some(prev) = code.instr(p) else { break };
                    if code::is_branch(&prev) {
                        break;
                    }
                    if code::gprs_written(&prev).any(|r| r == 1) {
                        let load = matches!(prev.op, Op::ld | Op::lwz | Op::ldx | Op::lwzx | Op::ldu | Op::lwzu);
                        if load && prev.ra() != 1 {
                            out.push(a);
                        }
                        break;
                    }
                }
            }
            a += 4;
        }
    }
    out
}
