//! Pairs the functions and data of two builds of the executable (the base game
//! and a title update), so the guest addresses hard-coded in the config and the
//! runtime can be moved to the update.
//!
//! Both images are compared as data. Each function's instructions are
//! normalized by masking what moves between builds (the displacements of
//! branches that leave the function, and the halves of `lis`/`addi`/load/store
//! pairs that form image addresses) while opcodes, registers and small
//! constants stay. Functions are paired by:
//! 1. identical import thunks (same library and name);
//! 2. a normalized body that occurs exactly once in each image (`exact`);
//! 3. propagation (`propagated`): the n-th direct call of a paired function
//!    pairs with the n-th call of its counterpart, and the unpaired functions
//!    between two consecutive pairs pair up in order when their counts agree;
//! 4. a similarity-scored alignment of the rest of each such gap (`fuzzy`).
//!
//! Data addresses are paired through code: every `hi`/`lo` pair in a paired
//! function, read at the same aligned instruction of its counterpart, gives one
//! (base address, update address) vote.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};

use ppc::{Form, Op};
use xex::Xex;

use crate::code::{Code, gprs_written};
use crate::functions::{DataKind, Source};

/// Marker for words of an inline switch table in a normalized body.
const TABLE_WORD: u32 = 0xEEEE_EEEE;

pub struct Func {
    pub begin: u32,
    pub end: u32,
    pub source: Source,
    /// Normalized instruction words.
    pub norm: Vec<u32>,
    pub hash: u64,
    /// Direct `bl` targets, in order.
    pub calls: Vec<u32>,
    /// `(byte offset of the lo instruction, formed image address)`.
    pub refs: Vec<(u32, u32)>,
    pub import: Option<(String, String)>,
    shingles: Vec<u64>,
}

impl Func {
    pub fn len(&self) -> u32 {
        self.end - self.begin
    }
}

pub struct Image<'a> {
    pub xex: &'a Xex,
    pub code: Code<'a>,
    /// Primary functions and import thunks, sorted by start.
    pub funcs: Vec<Func>,
    pub by_begin: HashMap<u32, usize>,
    /// Secondary entries (calls into the middle of a function).
    pub secondary: Vec<u32>,
    /// Top-level data inside code sections.
    pub code_data: Vec<(u32, u32, DataKind)>,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Loads, stores and adds whose 16-bit displacement can be the low half of an address.
fn takes_lo(op: Op) -> bool {
    if !matches!(op.form(), Form::D | Form::DS) {
        return false;
    }
    let m = op.mnemonic();
    (m.starts_with('l') && m != "lmw") || (m.starts_with("st") && m != "stmw") || m == "addi" || m == "addic"
}

impl<'a> Image<'a> {
    pub fn new(xex: &'a Xex) -> Result<Image<'a>, String> {
        let analysis = crate::analyse(xex)?;
        let code = Code::new(xex);
        let img = (xex.base_address, xex.base_address + xex.image.len() as u32);
        let embedded = &analysis.discovery.embedded;
        let mut funcs = Vec::new();
        let mut secondary = Vec::new();
        for f in &analysis.discovery.functions {
            if f.source == Source::SecondaryEntry {
                secondary.push(f.begin);
                continue;
            }
            let (norm, calls, refs) = normalize(&code, img, embedded, f.begin, f.end);
            let mut h = DefaultHasher::new();
            norm.hash(&mut h);
            let hash = h.finish();
            let shingles = shingles(&norm);
            let import = analysis.import_thunks.get(&f.begin).cloned();
            funcs.push(Func { begin: f.begin, end: f.end, source: f.source, norm, hash, calls, refs, import, shingles });
        }
        funcs.sort_by_key(|f| f.begin);
        let by_begin = funcs.iter().enumerate().map(|(i, f)| (f.begin, i)).collect();
        Ok(Image { xex, code, funcs, by_begin, secondary, code_data: analysis.discovery.data.clone() })
    }

    /// Index of the primary function containing `addr`.
    pub fn containing(&self, addr: u32) -> Option<usize> {
        let i = self.funcs.partition_point(|f| f.begin <= addr).checked_sub(1)?;
        (addr < self.funcs[i].end).then_some(i)
    }

    pub fn section_of(&self, addr: u32) -> Option<&xex::pe::Section> {
        self.xex.sections.iter().find(|s| s.contains(addr))
    }
}

fn normalize(code: &Code, img: (u32, u32), embedded: &[(u32, u32)], begin: u32, end: u32) -> (Vec<u32>, Vec<u32>, Vec<(u32, u32)>) {
    let hi_ok = |v: u32| {
        let a = v << 16;
        a >= img.0 && a < img.1.saturating_add(0x10000)
    };
    let first_table = embedded.partition_point(|&(_, e)| e <= begin);
    let tables: Vec<(u32, u32)> = embedded[first_table..].iter().take_while(|&&(b, _)| b < end).copied().collect();
    let mut norm = Vec::with_capacity(((end - begin) / 4) as usize);
    let mut calls = Vec::new();
    let mut refs = Vec::new();
    let mut hi: [Option<u32>; 32] = [None; 32];
    let mut p = begin;
    while p < end {
        let w = code.word(p).unwrap_or(0);
        if tables.iter().any(|&(b, e)| (b..e).contains(&p)) {
            norm.push(TABLE_WORD);
            p += 4;
            continue;
        }
        let Some(i) = code.instr(p) else {
            norm.push(w);
            p += 4;
            continue;
        };
        let mut nw = w;
        match i.op {
            Op::bx | Op::bcx => {
                let t = i.branch_target().unwrap();
                let internal = !i.bit(1) && (begin..end).contains(&t);
                if !internal {
                    nw = if i.op == Op::bx { w & !0x03FF_FFFC } else { w & !0xFFFC };
                }
                if i.lk() {
                    if i.op == Op::bx && code.is_code(t) {
                        calls.push(t);
                    }
                    // Calls clobber the volatile registers.
                    for r in [0usize, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
                        hi[r] = None;
                    }
                }
            }
            Op::addis if i.ra() == 0 && hi_ok(i.uimm16()) => {
                norm.push(w & !0xFFFF);
                hi[i.rt() as usize] = Some(i.uimm16() << 16);
                p += 4;
                continue;
            }
            Op::ori => {
                if let Some(h) = hi[i.rt() as usize] {
                    nw = w & !0xFFFF;
                    refs.push((p - begin, h | i.uimm16()));
                }
            }
            op if takes_lo(op) && i.ra() != 0 => {
                if let Some(h) = hi[i.ra() as usize] {
                    let (d, mask) = if op.form() == Form::DS {
                        (((i.bits(2, 14) << 2) as u16 as i16) as i32, 0xFFFC)
                    } else {
                        (i.simm16(), 0xFFFF)
                    };
                    nw = w & !mask;
                    refs.push((p - begin, h.wrapping_add(d as u32)));
                }
            }
            _ => {}
        }
        for r in gprs_written(&i) {
            hi[r as usize] = None;
        }
        norm.push(nw);
        p += 4;
    }
    (norm, calls, refs)
}

fn shingles(norm: &[u32]) -> Vec<u64> {
    let mut s: Vec<u64> = if norm.len() < 3 {
        norm.iter().map(|&w| splitmix(w as u64 | 1 << 40)).collect()
    } else {
        norm.windows(3)
            .map(|x| splitmix(x[0] as u64 ^ splitmix(x[1] as u64 ^ splitmix(x[2] as u64))))
            .collect()
    };
    s.sort_unstable();
    s
}

/// Same length, and every differing word is a D/DS-form instruction that differs
/// only in its displacement: the same code with moved data (an address half
/// whose `lis` the linear scan did not see).
pub fn displacement_only(a: &Func, b: &Func) -> bool {
    a.norm.len() == b.norm.len()
        && a.norm.iter().zip(&b.norm).all(|(&x, &y)| {
            x == y || (x >> 16 == y >> 16 && ppc::decode(x).is_some_and(|op| matches!(op.form(), Form::D | Form::DS)))
        })
}

/// Share of shingles in common, 0..=1.
pub fn similarity(a: &Func, b: &Func) -> f64 {
    if a.hash == b.hash {
        return 1.0;
    }
    let (n, m) = (a.norm.len(), b.norm.len());
    if n.min(m) * 3 < n.max(m) {
        return 0.0;
    }
    let (x, y) = (&a.shingles, &b.shingles);
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    common as f64 / x.len().max(y.len()).max(1) as f64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Import,
    Exact,
    Propagated,
    Fuzzy,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Import => "import",
            Method::Exact => "exact",
            Method::Propagated => "propagated",
            Method::Fuzzy => "fuzzy",
        }
    }
}

pub struct Matching {
    /// Per base function: `(tu index, method, confidence)`.
    pub b2t: Vec<Option<(usize, Method, f64)>>,
    pub t2b: Vec<Option<usize>>,
    /// Pairs outside the longest address-ordered chain: the update placed the
    /// function elsewhere (typically identical-code folding kept another copy).
    pub out_of_order: usize,
    pub moved: std::collections::HashSet<usize>,
}

impl Matching {
    fn set(&mut self, b: usize, t: usize, m: Method, c: f64) -> bool {
        if self.b2t[b].is_some() || self.t2b[t].is_some() {
            return false;
        }
        self.b2t[b] = Some((t, m, c));
        self.t2b[t] = Some(b);
        true
    }

    pub fn count(&self) -> usize {
        self.b2t.iter().filter(|x| x.is_some()).count()
    }
}

pub fn match_functions(base: &Image, tu: &Image) -> Matching {
    let mut m = Matching { b2t: vec![None; base.funcs.len()], t2b: vec![None; tu.funcs.len()], out_of_order: 0, moved: Default::default() };

    // 1. Import thunks.
    let tu_imports: HashMap<&(String, String), usize> =
        tu.funcs.iter().enumerate().filter_map(|(i, f)| f.import.as_ref().map(|k| (k, i))).collect();
    for (b, f) in base.funcs.iter().enumerate() {
        if let Some(k) = &f.import
            && let Some(&t) = tu_imports.get(k)
        {
            m.set(b, t, Method::Import, 1.0);
        }
    }

    // 2. Unique normalized bodies.
    let mut groups: HashMap<u64, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for (i, f) in base.funcs.iter().enumerate().filter(|(_, f)| f.import.is_none()) {
        groups.entry(f.hash).or_default().0.push(i);
    }
    for (i, f) in tu.funcs.iter().enumerate().filter(|(_, f)| f.import.is_none()) {
        groups.entry(f.hash).or_default().1.push(i);
    }
    for (bs, ts) in groups.values() {
        if let ([b], [t]) = (bs.as_slice(), ts.as_slice()) {
            m.set(*b, *t, Method::Exact, 1.0);
        }
    }
    // 3/4. Propagate to a fixed point.
    loop {
        let before = m.count();
        propagate_calls(base, tu, &mut m);
        fill_gaps(base, tu, &mut m);
        resolve_duplicates(base, tu, &mut m);
        if m.count() == before {
            break;
        }
    }
    let all: Vec<(usize, usize)> = m.b2t.iter().enumerate().filter_map(|(b, x)| x.map(|x| (b, x.0))).collect();
    let ordered: std::collections::HashSet<usize> = in_order(&all).into_iter().map(|p| p.0).collect();
    m.moved = all.iter().map(|p| p.0).filter(|b| !ordered.contains(b)).collect();
    m.out_of_order = m.moved.len();
    m
}

fn propagate_calls(base: &Image, tu: &Image, m: &mut Matching) {
    let mut fwd: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut back: HashMap<usize, Vec<usize>> = HashMap::new();
    for (b, x) in m.b2t.iter().enumerate() {
        let Some((t, _, _)) = *x else { continue };
        let (cb, ct) = (&base.funcs[b].calls, &tu.funcs[t].calls);
        if cb.len() != ct.len() {
            continue;
        }
        for (&xb, &xt) in cb.iter().zip(ct) {
            let (Some(&ib), Some(&it)) = (base.by_begin.get(&xb), tu.by_begin.get(&xt)) else { continue };
            if m.b2t[ib].is_some() || m.t2b[it].is_some() {
                continue;
            }
            fwd.entry(ib).or_default().push(it);
            back.entry(it).or_default().push(ib);
        }
    }
    let mut fwd: Vec<(usize, Vec<usize>)> = fwd.into_iter().collect();
    fwd.sort_by_key(|x| x.0);
    for (ib, mut ts) in fwd {
        ts.sort_unstable();
        ts.dedup();
        let [it] = ts.as_slice() else { continue };
        let mut bs = back[it].clone();
        bs.sort_unstable();
        bs.dedup();
        if bs.len() != 1 {
            continue;
        }
        let s = similarity(&base.funcs[ib], &tu.funcs[*it]);
        let c = if s >= 1.0 {
            0.98
        } else if displacement_only(&base.funcs[ib], &tu.funcs[*it]) {
            0.95
        } else {
            0.9 * s
        };
        if s >= 0.4 {
            m.set(ib, *it, Method::Propagated, c);
        }
    }
}

/// Unpaired functions whose normalized body occurs several times (destructors,
/// getters that differ only in the address they use): pairs each with the one
/// candidate whose data references and call targets agree with the pairs
/// already known.
fn resolve_duplicates(base: &Image, tu: &Image, m: &mut Matching) {
    let corr = correspondence(base, tu, m, false);
    let fmap = function_map(base, tu, m);
    let mut groups: HashMap<u64, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for (b, f) in base.funcs.iter().enumerate().filter(|(b, f)| m.b2t[*b].is_none() && f.import.is_none()) {
        groups.entry(f.hash).or_default().0.push(b);
    }
    for (t, f) in tu.funcs.iter().enumerate().filter(|(t, f)| m.t2b[*t].is_none() && f.import.is_none()) {
        if let Some(g) = groups.get_mut(&f.hash) {
            g.1.push(t);
        }
    }
    let mut picks: Vec<(usize, usize)> = Vec::new();
    for (bs, ts) in groups.values() {
        if ts.is_empty() {
            continue;
        }
        let agrees = |b: usize, t: usize| -> Option<bool> {
            let (fb, ft) = (&base.funcs[b], &tu.funcs[t]);
            let mut known = 0;
            for (&(_, a), &(_, a2)) in fb.refs.iter().zip(&ft.refs) {
                if let Some(v) = corr.get(&a) {
                    known += 1;
                    if best(v).0 != a2 {
                        return Some(false);
                    }
                }
            }
            for (c, c2) in fb.calls.iter().zip(&ft.calls) {
                if let Some(&x) = fmap.get(c) {
                    known += 1;
                    if x != *c2 {
                        return Some(false);
                    }
                }
            }
            (known > 0).then_some(true)
        };
        let ok: Vec<(usize, usize)> =
            bs.iter().flat_map(|&b| ts.iter().map(move |&t| (b, t))).filter(|&(b, t)| agrees(b, t) == Some(true)).collect();
        for &(b, t) in &ok {
            if ok.iter().filter(|p| p.0 == b).count() == 1 && ok.iter().filter(|p| p.1 == t).count() == 1 {
                picks.push((b, t));
            }
        }
    }
    for (b, t) in picks {
        m.set(b, t, Method::Propagated, 0.97);
    }

    // Pointers in data (vtable slots, callback tables): a word holding an
    // unpaired base function, at a data location whose update address is
    // pinned by code references on both sides, names the update function.
    let unpaired: HashMap<u32, usize> = base
        .funcs
        .iter()
        .enumerate()
        .filter(|(b, f)| m.b2t[*b].is_none() && f.import.is_none())
        .map(|(b, f)| (f.begin, b))
        .collect();
    let mut votes: HashMap<usize, Vec<usize>> = HashMap::new();
    for s in base.xex.sections.iter().filter(|s| !s.is_code()) {
        for loc in (s.address..s.end().saturating_sub(3)).step_by(4) {
            let Some(w) = base.code.word(loc) else { break };
            let Some(&b) = unpaired.get(&w) else { continue };
            let Some(l2) = pinned(&corr, loc) else { continue };
            let Some(w2) = tu.code.word(l2) else { continue };
            if let Some(&t) = tu.by_begin.get(&w2)
                && m.t2b[t].is_none()
                && similarity(&base.funcs[b], &tu.funcs[t]) >= 0.9
            {
                votes.entry(b).or_default().push(t);
            }
        }
    }
    let mut picks: Vec<(usize, usize)> = votes
        .into_iter()
        .filter_map(|(b, mut ts)| {
            ts.sort_unstable();
            ts.dedup();
            (ts.len() == 1).then(|| (b, ts[0]))
        })
        .collect();
    picks.sort_unstable();
    for (b, t) in picks {
        let c = if base.funcs[b].hash == tu.funcs[t].hash { 0.96 } else { 0.9 };
        m.set(b, t, Method::Propagated, c);
    }
}

/// Update address of a base data location when the nearest code references
/// below and above it (or one exactly at it) agree on the delta.
fn pinned(corr: &Corr, x: u32) -> Option<u32> {
    if let Some(v) = corr.get(&x) {
        return Some(best(v).0);
    }
    let (a, va) = corr.range(..x).next_back()?;
    let (c, vc) = corr.range(x + 1..).next()?;
    let (d, e) = (best(va).0.wrapping_sub(*a), best(vc).0.wrapping_sub(*c));
    (d == e).then(|| x.wrapping_add(d))
}

/// The longest run of `pairs` (sorted by base) whose update side also increases.
pub fn in_order(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut tails: Vec<usize> = Vec::new(); // index into pairs of the smallest tail per length
    let mut prev = vec![usize::MAX; pairs.len()];
    for (k, &(_, t)) in pairs.iter().enumerate() {
        let pos = tails.partition_point(|&i| pairs[i].1 < t);
        if pos > 0 {
            prev[k] = tails[pos - 1];
        }
        if pos == tails.len() {
            tails.push(k);
        } else {
            tails[pos] = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied().unwrap_or(usize::MAX);
    while k != usize::MAX {
        out.push(pairs[k]);
        k = prev[k];
    }
    out.reverse();
    out
}

fn fill_gaps(base: &Image, tu: &Image, m: &mut Matching) {
    let all: Vec<(usize, usize)> = m.b2t.iter().enumerate().filter_map(|(b, x)| x.map(|x| (b, x.0))).collect();
    let anchors = in_order(&all);
    for w in anchors.windows(2) {
        let ((b0, t0), (b1, t1)) = (w[0], w[1]);
        if t1 <= t0 || b1 == b0 + 1 && t1 == t0 + 1 {
            continue;
        }
        let bs: Vec<usize> = (b0 + 1..b1).filter(|&b| m.b2t[b].is_none() && base.funcs[b].import.is_none()).collect();
        let ts: Vec<usize> = (t0 + 1..t1).filter(|&t| m.t2b[t].is_none() && tu.funcs[t].import.is_none()).collect();
        if bs.is_empty() || ts.is_empty() || bs.len() * ts.len() > 250_000 {
            continue;
        }
        let sim = |i: usize, j: usize| similarity(&base.funcs[bs[i]], &tu.funcs[ts[j]]);
        if bs.len() == ts.len() {
            let s: Vec<f64> = (0..bs.len()).map(|k| sim(k, k)).collect();
            if s.iter().all(|&x| x >= 0.3) {
                for (k, &x) in s.iter().enumerate() {
                    let c = if x >= 1.0 {
                        0.97
                    } else if displacement_only(&base.funcs[bs[k]], &tu.funcs[ts[k]]) {
                        0.95
                    } else {
                        0.9 * x
                    };
                    m.set(bs[k], ts[k], Method::Propagated, c);
                }
                continue;
            }
        }
        // Order-preserving alignment maximizing total similarity.
        let (n, k) = (bs.len(), ts.len());
        let mut score = vec![0.0f64; (n + 1) * (k + 1)];
        let mut s = vec![0.0f64; n * k];
        for i in 0..n {
            for j in 0..k {
                s[i * k + j] = sim(i, j);
            }
        }
        let at = |i: usize, j: usize| i * (k + 1) + j;
        for i in 1..=n {
            for j in 1..=k {
                let x = s[(i - 1) * k + j - 1];
                let diag = if x >= 0.5 { score[at(i - 1, j - 1)] + x } else { f64::MIN };
                score[at(i, j)] = score[at(i - 1, j)].max(score[at(i, j - 1)]).max(diag);
            }
        }
        let (mut i, mut j) = (n, k);
        while i > 0 && j > 0 {
            let x = s[(i - 1) * k + j - 1];
            if x >= 0.5 && score[at(i, j)] == score[at(i - 1, j - 1)] + x {
                let c = if x >= 1.0 {
                    0.95
                } else if displacement_only(&base.funcs[bs[i - 1]], &tu.funcs[ts[j - 1]]) {
                    0.93
                } else {
                    0.85 * x
                };
                m.set(bs[i - 1], ts[j - 1], Method::Fuzzy, c);
                i -= 1;
                j -= 1;
            } else if score[at(i, j)] == score[at(i - 1, j)] {
                i -= 1;
            } else {
                j -= 1;
            }
        }
    }
}

/// For each index of `a`, the index of `b` it aligns to in a longest common subsequence.
pub fn lcs_align(a: &[u32], b: &[u32]) -> Vec<Option<usize>> {
    let (n, m) = (a.len(), b.len());
    let mut out = vec![None; n];
    if n == 0 || m == 0 {
        return out;
    }
    if (n + 1) * (m + 1) > 64_000_000 {
        // Split at the middle of `a` against the proportional point of `b`.
        let (ha, hb) = (n / 2, m * (n / 2) / n);
        let left = lcs_align(&a[..ha], &b[..hb]);
        let right = lcs_align(&a[ha..], &b[hb..]);
        out[..ha].copy_from_slice(&left);
        for (k, x) in right.into_iter().enumerate() {
            out[ha + k] = x.map(|y| y + hb);
        }
        return out;
    }
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if a[i] == b[j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out[i] = Some(j);
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Base data address -> (update address -> votes), from the hi/lo pairs of paired functions.
pub type Corr = BTreeMap<u32, BTreeMap<u32, u32>>;

pub fn data_correspondence(base: &Image, tu: &Image, m: &Matching) -> Corr {
    correspondence(base, tu, m, true)
}

fn correspondence(base: &Image, tu: &Image, m: &Matching, changed_too: bool) -> Corr {
    let mut corr: Corr = BTreeMap::new();
    for (b, x) in m.b2t.iter().enumerate() {
        let Some((t, _, _)) = *x else { continue };
        let (fb, ft) = (&base.funcs[b], &tu.funcs[t]);
        if fb.refs.is_empty() {
            continue;
        }
        let tref: HashMap<u32, u32> = ft.refs.iter().copied().collect();
        if fb.hash == ft.hash {
            for &(off, a) in &fb.refs {
                if let Some(&a2) = tref.get(&off) {
                    *corr.entry(a).or_default().entry(a2).or_default() += 1;
                }
            }
        } else if changed_too && fb.norm.len() * ft.norm.len() <= 4_000_000 {
            let al = lcs_align(&fb.norm, &ft.norm);
            for &(off, a) in &fb.refs {
                if let Some(j) = al[(off / 4) as usize]
                    && let Some(&a2) = tref.get(&(j as u32 * 4))
                {
                    *corr.entry(a).or_default().entry(a2).or_default() += 1;
                }
            }
        }
    }
    corr
}

pub struct Mapped {
    pub tu: Option<u32>,
    pub confidence: f64,
    pub how: String,
}

fn best(votes: &BTreeMap<u32, u32>) -> (u32, u32, u32) {
    let total: u32 = votes.values().sum();
    let (&a, &n) = votes.iter().max_by_key(|&(_, n)| *n).unwrap();
    (a, n, total)
}

/// Maps a base data address through the code references, falling back to the
/// section's delta. `funcs` maps base function starts to update ones (for
/// comparing pointer words in the content check).
pub fn map_data(base: &Image, tu: &Image, corr: &Corr, fmap: &HashMap<u32, u32>, x: u32) -> Mapped {
    let mut how = String::new();
    let mut result: Option<(u32, f64)> = None;
    if let Some(v) = corr.get(&x) {
        let (a, n, total) = best(v);
        let c = if n == total { 1.0 } else { n as f64 / total as f64 };
        let _ = write!(how, "{n}/{total} code refs agree");
        result = Some((a, if total >= 2 { c } else { 0.9 * c }));
    } else {
        let below = corr.range(..x).next_back();
        let above = corr.range(x + 1..).next();
        let delta = |(&a, v): (&u32, &BTreeMap<u32, u32>)| (a, best(v).0.wrapping_sub(a));
        match (below.map(delta), above.map(delta)) {
            (Some((a, d)), Some((c, e))) if d == e => {
                let _ = write!(how, "between refs 0x{a:08X} and 0x{c:08X} with the same delta {:+#X}", d as i32);
                let near = (x - a).min(c - x);
                result = Some((x.wrapping_add(d), if near <= 0x100 { 0.95 } else { 0.85 }));
            }
            (Some((a, d)), other) => {
                let _ = write!(how, "nearest ref below 0x{a:08X} (+0x{:X}) delta {:+#X}", x - a, d as i32);
                if let Some((c, e)) = other {
                    let _ = write!(how, "; next ref 0x{c:08X} has delta {:+#X}", e as i32);
                }
                result = Some((x.wrapping_add(d), if x - a <= 0x40 { 0.7 } else { 0.5 }));
            }
            (None, Some((c, e))) => {
                let _ = write!(how, "only ref above 0x{c:08X} delta {:+#X}", e as i32);
                result = Some((x.wrapping_add(e), 0.4));
            }
            (None, None) => {}
        }
    }
    let sec = base.section_of(x);
    if result.is_none()
        && let Some(s) = sec
        && let Some(s2) = tu.xex.section(&s.name)
    {
        let _ = write!(how, "section {} delta only", s.name);
        result = Some((x - s.address + s2.address, 0.3));
    }
    let Some((t, mut conf)) = result else {
        return Mapped { tu: None, confidence: 0.0, how: "no code refs and not in a section".into() };
    };
    // Content check: 32 bytes, pointer words translated through the maps.
    let (mut same, mut words, mut zero, mut first) = (0, 0, true, false);
    for k in 0..8u32 {
        let (Some(wb), Some(wt)) = (base.code.word(x + 4 * k), tu.code.word(t + 4 * k)) else { break };
        words += 1;
        zero &= wb == 0 && wt == 0;
        let tr = fmap.get(&wb).copied().or_else(|| corr.get(&wb).map(|v| best(v).0)).unwrap_or(wb);
        if tr == wt {
            same += 1;
            first |= k == 0;
        }
    }
    if zero {
        let _ = write!(how, "; contents zero (bss or unset)");
    } else if !first {
        let _ = write!(how, "; FIRST WORD DIFFERS ({same}/{words} words equal)");
        conf *= 0.5;
    } else {
        let _ = write!(how, "; first word equal, {same}/{words} of 32 bytes equal");
        if same == words && conf < 0.9 {
            conf = conf.max(0.8);
        }
    }
    Mapped { tu: Some(t), confidence: conf, how }
}

/// Maps a base code address (function start or an instruction inside one).
pub fn map_code(base: &Image, tu: &Image, m: &Matching, x: u32) -> (String, Mapped) {
    let Some(b) = base.containing(x) else {
        let kind = base
            .code_data
            .iter()
            .find(|&&(s, e, _)| (s..e).contains(&x))
            .map(|d| format!("data in code ({:?})", d.2))
            .unwrap_or_else(|| "code, outside any function".into());
        return (kind, Mapped { tu: None, confidence: 0.0, how: "not inside a discovered function".into() });
    };
    let fb = &base.funcs[b];
    let off = x - fb.begin;
    let mut kind = if off == 0 {
        if fb.import.is_some() { "import thunk".to_string() } else { "function start".to_string() }
    } else {
        format!("inside sub_{:08X} +0x{off:X}", fb.begin)
    };
    if off != 0 && base.secondary.contains(&x) {
        kind = format!("secondary entry, {kind}");
    }
    let Some((t, method, conf)) = m.b2t[b] else {
        return (kind, Mapped { tu: None, confidence: 0.0, how: format!("sub_{:08X} unmatched", fb.begin) });
    };
    let ft = &tu.funcs[t];
    let mut conf = conf;
    let mut head = format!("sub_{:08X} -> sub_{:08X} ({} {:.2})", fb.begin, ft.begin, method.name(), conf);
    if m.moved.contains(&b) {
        head += "; MOVED out of address order (another copy kept, e.g. identical-code folding)";
        conf = conf.min(0.85);
    }
    if off == 0 {
        // Callers: base callers whose counterparts call the update function.
        let callers = |img: &Image, target: u32| -> Vec<usize> {
            img.funcs.iter().enumerate().filter(|(_, f)| f.calls.contains(&target)).map(|(i, _)| i).collect()
        };
        let (cb, ct) = (callers(base, fb.begin), callers(tu, ft.begin));
        let agree = cb.iter().filter(|&&i| m.b2t[i].is_some_and(|(j, _, _)| ct.contains(&j))).count();
        head += &format!("; direct callers {} base / {} update, {agree} paired", cb.len(), ct.len());
        if !cb.is_empty() && agree * 2 < cb.len() {
            conf *= 0.7;
        }
        return (kind, Mapped { tu: Some(ft.begin), confidence: conf, how: head });
    }
    let k = (off / 4) as usize;
    let (j, note) = if fb.hash == ft.hash {
        (Some(k), "same body")
    } else {
        (lcs_align(&fb.norm, &ft.norm)[k], "aligned")
    };
    let Some(j) = j else {
        return (kind, Mapped { tu: None, confidence: 0.0, how: format!("{head}; instruction +0x{off:X} has no aligned counterpart") });
    };
    let t_addr = ft.begin + j as u32 * 4;
    let mut c = conf;
    let mut how = format!("{head}; {note} +0x{off:X} -> +0x{:X}", j * 4);
    if fb.norm.get(k) != ft.norm.get(j) {
        c *= 0.5;
        how += "; normalized word differs";
    }
    // For return addresses and branch sites, the previous instruction too.
    if k > 0 && j > 0 {
        if fb.norm[k - 1] == ft.norm[j - 1] {
            how += "; previous word equal";
        } else {
            c *= 0.8;
            how += "; previous word differs";
        }
    }
    if note == "aligned" {
        c *= 0.95;
    }
    (kind, Mapped { tu: Some(t_addr), confidence: c, how })
}

pub fn function_map(base: &Image, tu: &Image, m: &Matching) -> HashMap<u32, u32> {
    m.b2t
        .iter()
        .enumerate()
        .filter_map(|(b, x)| x.map(|(t, _, _)| (base.funcs[b].begin, tu.funcs[t].begin)))
        .collect()
}

/// One hard-coded guest address found in the sources.
pub struct Literal {
    pub file: String,
    pub line: usize,
    pub form: &'static str,
    pub value: u32,
}

/// `0x82…`/`0x83…` hex literals (underscores allowed) and `sub_8…` names in `text`.
pub fn find_literals(file: &str, text: &str) -> Vec<Literal> {
    let mut out = Vec::new();
    let mut in_tests = false;
    for (n, line) in text.lines().enumerate() {
        // A `#[cfg(test)]` item runs to the next closing brace in column 0.
        if line.trim_start().starts_with("#[cfg(test)]") {
            in_tests = true;
        } else if in_tests && line.starts_with('}') {
            in_tests = false;
            continue;
        }
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let (form, start) = if b[i..].starts_with(b"0x") || b[i..].starts_with(b"0X") {
                ("hex", i + 2)
            } else if b[i..].starts_with(b"sub_") && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
                ("sub_name", i + 4)
            } else {
                i += 1;
                continue;
            };
            if form == "hex" && i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
                i += 2;
                continue;
            }
            let mut j = start;
            let mut digits = String::new();
            while j < b.len() && (b[j].is_ascii_hexdigit() || (form == "hex" && b[j] == b'_')) {
                if b[j] != b'_' {
                    digits.push(b[j] as char);
                }
                j += 1;
            }
            if digits.len() == 8
                && let Ok(v) = u32::from_str_radix(&digits, 16)
                && (0x8200_0000..0x8400_0000).contains(&v)
            {
                let form = match (in_tests, form) {
                    (true, "hex") => "hex (test code)",
                    (true, _) => "sub_name (test code)",
                    _ => form,
                };
                out.push(Literal { file: file.to_string(), line: n + 1, form, value: v });
            }
            i = j.max(i + 1);
        }
    }
    out
}
