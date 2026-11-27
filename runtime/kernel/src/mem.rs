//! Guest memory: the 4 GiB address space, its layout, and the allocators behind
//! the kernel's memory API (`NtAllocateVirtualMemory`,
//! `MmAllocatePhysicalMemoryEx`, pool and kernel-object allocations).
//!
//! The whole space is reserved up front and pages are committed only when the
//! guest allocates them, as on the console: touching memory nobody allocated
//! is an access violation, which [`crate::diag`] reports with the guest
//! address. The null page and the hardware (MMIO) window are never committed,
//! so a stray pointer or a hardware register access stops loudly.
//!
//! Layout (guest addresses):
//!
//! | Range | Use |
//! |---|---|
//! | `0x00000000-0x0000FFFF` | never mapped (null guard) |
//! | `0x00010000-0x3FFFFFFF` | virtual allocations, 4 KB pages |
//! | `0x40000000-0x6FFFFFFF` | virtual allocations, 64 KB pages (`MEM_LARGE_PAGES`) |
//! | `0x70000000-0x7EFFFFFF` | guest thread stacks |
//! | `0x7F000000-0x7FFFFFFF` | never mapped (includes MMIO at `0x7FC00000`) |
//! | `0x80000000-0x81FFFFFF` | kernel-owned objects and pool (where the console kernel lives) |
//! | `0x82000000-0x8FFFFFFF` | the executable image |
//! | `0xA0000000-0xBFFFFFFF` | physical memory, 64 KB pages view |
//! | `0xC0000000-0xDFFFFFFF` | physical memory, 16 MB pages view |
//! | `0xE0000000-0xFFFFFFFF` | physical memory, 4 KB pages view |
//!
//! Physical memory is one 512 MiB space shared by the three views, so a
//! physical address identifies one allocation. The views are not aliased onto
//! each other: each allocation is reachable through the view it was allocated
//! in. That is enough while the game's GPU library is replaced natively
//! (Phase 4); if code ever reads one allocation through two views, that needs
//! OS-level view mapping.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::status::*;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
    fn VirtualProtect(addr: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn VirtualQuery(addr: *const c_void, info: *mut HostRegion, len: usize) -> usize;
}

/// `MEMORY_BASIC_INFORMATION` (64-bit).
#[repr(C)]
#[derive(Default)]
struct HostRegion {
    base: usize,
    allocation_base: usize,
    allocation_protect: u32,
    partition_id: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

/// True if guest bytes `[addr, addr + len)` are committed and readable. Takes
/// no locks, so fault handlers can use it.
pub fn host_readable(addr: u32, len: u32) -> bool {
    let b = base();
    if b.is_null() || (addr as u64 + len as u64) > SPAN as u64 {
        return false;
    }
    let mut at = addr as usize;
    let end = addr as usize + len as usize;
    while at < end {
        let mut r = HostRegion::default();
        let n = unsafe { VirtualQuery(b.add(at).cast(), &mut r, std::mem::size_of::<HostRegion>()) };
        let readable = r.protect & (HOST_PAGE_READONLY | HOST_PAGE_READWRITE) != 0;
        if n == 0 || r.state != HOST_MEM_COMMIT || !readable {
            return false;
        }
        at = r.base + r.region_size - b as usize;
    }
    true
}

const HOST_MEM_COMMIT: u32 = 0x1000;
const HOST_MEM_RESERVE: u32 = 0x2000;
const HOST_MEM_DECOMMIT: u32 = 0x4000;
const HOST_PAGE_NOACCESS: u32 = 0x01;
const HOST_PAGE_READONLY: u32 = 0x02;
const HOST_PAGE_READWRITE: u32 = 0x04;

/// Guest `MEM_*` allocation flags.
pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_DECOMMIT: u32 = 0x4000;
pub const MEM_RELEASE: u32 = 0x8000;
pub const MEM_FREE: u32 = 0x1_0000;
pub const MEM_PRIVATE: u32 = 0x2_0000;
pub const MEM_RESET: u32 = 0x8_0000;
pub const MEM_TOP_DOWN: u32 = 0x10_0000;
pub const MEM_IMAGE: u32 = 0x100_0000;
pub const MEM_LARGE_PAGES: u32 = 0x2000_0000;
pub const MEM_16MB_PAGES: u32 = 0x8000_0000;

/// Guest `PAGE_*` protection flags.
pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// 4 GiB plus slack for a 16-byte access at the very top.
const SPAN: usize = (1usize << 32) + 0x1_0000;

pub const NULL_GUARD_END: u32 = 0x0001_0000;
pub const HEAP_4K: (u32, u32) = (0x0001_0000, 0x4000_0000);
pub const HEAP_64K: (u32, u32) = (0x4000_0000, 0x7000_0000);
pub const STACKS: (u32, u32) = (0x7000_0000, 0x7F00_0000);
pub const MMIO: (u32, u32) = (0x7FC0_0000, 0x8000_0000);
pub const KERNEL: (u32, u32) = (0x8000_0000, 0x8200_0000);
pub const IMAGE: (u32, u32) = (0x8200_0000, 0x9000_0000);
/// Physical memory size (512 MiB).
pub const PHYSICAL_SIZE: u32 = 0x2000_0000;

static BASE: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

/// Host pointer to guest address 0.
#[inline(always)]
pub fn base() -> *mut u8 {
    BASE.load(Ordering::Relaxed)
}

/// True if `host` points into the reserved guest address space.
pub fn guest_address_of(host: usize) -> Option<u32> {
    let b = base() as usize;
    (b != 0 && host >= b && host < b + SPAN).then(|| (host - b) as u32)
}

/// Reserves the address space and commits the kernel region.
pub fn init() {
    assert!(base().is_null(), "guest memory initialised twice");
    let p = unsafe { VirtualAlloc(core::ptr::null_mut(), SPAN, HOST_MEM_RESERVE, HOST_PAGE_NOACCESS) };
    assert!(!p.is_null(), "could not reserve the 4 GiB guest address space: {}", std::io::Error::last_os_error());
    BASE.store(p.cast(), Ordering::Relaxed);
    host_commit(KERNEL.0, KERNEL.1 - KERNEL.0, PAGE_READWRITE);
}

fn host_protect_of(guest: u32) -> u32 {
    match guest & 0xFF {
        0 | PAGE_NOACCESS => HOST_PAGE_NOACCESS,
        PAGE_READONLY | PAGE_EXECUTE_READ | 0x10 => HOST_PAGE_READONLY,
        _ => HOST_PAGE_READWRITE,
    }
}

/// Commits guest pages `[addr, addr+len)` (page-aligned) with a guest protection.
pub fn host_commit(addr: u32, len: u32, protect: u32) {
    let p = unsafe { VirtualAlloc(base().add(addr as usize).cast(), len as usize, HOST_MEM_COMMIT, host_protect_of(protect)) };
    assert!(
        !p.is_null(),
        "FATAL: host refused to commit guest memory 0x{addr:08X}+0x{len:X}: {}",
        std::io::Error::last_os_error()
    );
}

pub fn host_decommit(addr: u32, len: u32) {
    // Freed memory is nobody's to watch any more.
    WATCHES.lock().unwrap().retain(|&(s, n, _, _)| !(addr < s + n && s < addr.saturating_add(len)));
    unsafe { VirtualFree(base().add(addr as usize).cast(), len as usize, HOST_MEM_DECOMMIT) };
}

pub fn host_protect(addr: u32, len: u32, protect: u32) {
    let mut old = 0;
    let ok = unsafe { VirtualProtect(base().add(addr as usize).cast(), len as usize, host_protect_of(protect), &mut old) };
    assert!(ok != 0, "FATAL: could not protect guest memory 0x{addr:08X}+0x{len:X}: {}", std::io::Error::last_os_error());
}

// ---- Access watches ---------------------------------------------------------------

/// Called on the thread that first reads a watched range, with the range,
/// before the read goes ahead: it puts in guest memory what the reader
/// should find there.
pub type WatchReader = fn(u32, u32);

/// Guest ranges being watched: page-aligned start, length, what it is, and
/// who supplies its contents on the first read (none: a diagnostic watch).
static WATCHES: Mutex<Vec<(u32, u32, String, Option<WatchReader>)>> = Mutex::new(Vec::new());

/// Makes the pages holding `[addr, addr+len)` inaccessible until something
/// touches them. The first access, a read or a write, is reported with the
/// guest thread, its registers and both stacks (`diag`'s fault handler); the
/// pages are then made accessible again and the access goes ahead. For
/// finding the code that reads or writes a piece of guest memory. Native code
/// that would read a watched range asks [`watched`] first.
pub fn watch(addr: u32, len: u32, what: &str) {
    watch_with(addr, len, what, None);
}

/// A watch whose first read is served by `reader`: memory whose contents
/// exist somewhere else (an image the renderer holds) until the game looks.
/// The reader runs on the reading thread, with the pages already accessible;
/// a first access that is a write just ends the watch.
pub fn watch_with(addr: u32, len: u32, what: &str, reader: Option<WatchReader>) {
    let start = addr & !0xFFF;
    let size = ((addr + len + 0xFFF) & !0xFFF) - start;
    let mut w = WATCHES.lock().unwrap();
    if w.iter().any(|&(s, n, _, _)| start < s + n && s < start + size) {
        return;
    }
    let mut old = 0;
    if unsafe { VirtualProtect(base().add(start as usize).cast(), size as usize, HOST_PAGE_NOACCESS, &mut old) } != 0 {
        w.push((start, size, what.to_string(), reader));
    }
}

/// Is `addr` in a watched range?
pub fn watched(addr: u32) -> bool {
    WATCHES.lock().unwrap().iter().any(|&(s, n, _, _)| (s..s + n).contains(&addr))
}

/// Ends the watch that covers `addr`, if there is one: its pages are
/// accessible again. Returns its range and name.
pub(crate) fn end_watch(addr: u32) -> Option<(u32, u32, String, Option<WatchReader>)> {
    let mut w = WATCHES.lock().unwrap();
    let i = w.iter().position(|&(s, n, _, _)| (s..s + n).contains(&addr))?;
    let (start, size, what, reader) = w.remove(i);
    let mut old = 0;
    unsafe { VirtualProtect(base().add(start as usize).cast(), size as usize, HOST_PAGE_READWRITE, &mut old) };
    Some((start, size, what, reader))
}

// ---- Access helpers for native code ----------------------------------------------

#[inline]
pub fn r8(a: u32) -> u8 {
    unsafe { ppc_rt::ld8(base(), a) }
}
#[inline]
pub fn r16(a: u32) -> u16 {
    unsafe { ppc_rt::ld16(base(), a) }
}
#[inline]
pub fn r32(a: u32) -> u32 {
    unsafe { ppc_rt::ld32(base(), a) }
}
#[inline]
pub fn r64(a: u32) -> u64 {
    unsafe { ppc_rt::ld64(base(), a) }
}
#[inline]
pub fn w8(a: u32, v: u8) {
    unsafe { ppc_rt::st8(base(), a, v) }
}
#[inline]
pub fn w16(a: u32, v: u16) {
    unsafe { ppc_rt::st16(base(), a, v) }
}
#[inline]
pub fn w32(a: u32, v: u32) {
    unsafe { ppc_rt::st32(base(), a, v) }
}
#[inline]
pub fn w64(a: u32, v: u64) {
    unsafe { ppc_rt::st64(base(), a, v) }
}

pub fn read(a: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0; len];
    unsafe { core::ptr::copy_nonoverlapping(base().add(a as usize), v.as_mut_ptr(), len) };
    v
}

pub fn write(a: u32, bytes: &[u8]) {
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), base().add(a as usize), bytes.len()) };
}

pub fn fill(a: u32, len: usize, value: u8) {
    unsafe { core::ptr::write_bytes(base().add(a as usize), value, len) };
}

/// Host slice over guest memory. The caller must know the range is committed.
pub unsafe fn slice_mut<'a>(a: u32, len: usize) -> &'a mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(base().add(a as usize), len) }
}

/// NUL-terminated byte string (at most `max` bytes).
pub fn cstr(a: u32, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut p = a;
    while out.len() < max {
        let b = r8(p);
        if b == 0 {
            break;
        }
        out.push(b);
        p = p.wrapping_add(1);
    }
    out
}

/// NUL-terminated big-endian UTF-16 string.
pub fn wstr(a: u32, max: usize) -> Vec<u16> {
    let mut out = Vec::new();
    let mut p = a;
    while out.len() < max {
        let c = r16(p);
        if c == 0 {
            break;
        }
        out.push(c);
        p = p.wrapping_add(2);
    }
    out
}

/// Latin-1 decoding of guest bytes (the console's ANSI code page for paths).
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// `ANSI_STRING { u16 Length; u16 MaximumLength; char *Buffer; }`
pub fn ansi_string(a: u32) -> Option<String> {
    if a == 0 {
        return None;
    }
    let len = r16(a) as usize;
    let buf = r32(a + 4);
    if buf == 0 {
        return Some(String::new());
    }
    Some(latin1(&read(buf, len)))
}

/// `UNICODE_STRING { u16 Length (bytes); u16 MaximumLength; wchar_t *Buffer; }`
pub fn unicode_string(a: u32) -> Option<String> {
    if a == 0 {
        return None;
    }
    let len = r16(a) as usize / 2;
    let buf = r32(a + 4);
    let units: Vec<u16> = (0..len).map(|i| r16(buf + 2 * i as u32)).collect();
    Some(String::from_utf16_lossy(&units))
}

// ---- Arena: first-fit allocator over a fixed range ---------------------------------

/// First-fit allocator with coalescing, for kernel-owned guest memory.
struct Arena {
    free: BTreeMap<u32, u32>,
    used: HashMap<u32, u32>,
}

impl Arena {
    fn new(start: u32, end: u32) -> Arena {
        Arena { free: BTreeMap::from([(start, end - start)]), used: HashMap::new() }
    }

    fn alloc(&mut self, size: u32, align: u32) -> Option<u32> {
        let size = size.max(1).next_multiple_of(16);
        let (start, len, at) = self.free.iter().find_map(|(&s, &l)| {
            let at = s.next_multiple_of(align);
            (at >= s && (at - s) as u64 + size as u64 <= l as u64).then_some((s, l, at))
        })?;
        self.free.remove(&start);
        if at > start {
            self.free.insert(start, at - start);
        }
        let end = start + len;
        if at + size < end {
            self.free.insert(at + size, end - (at + size));
        }
        self.used.insert(at, size);
        Some(at)
    }

    fn free(&mut self, addr: u32) -> Option<u32> {
        let size = self.used.remove(&addr)?;
        let (mut start, mut len) = (addr, size);
        if let Some((&ps, &pl)) = self.free.range(..start).next_back()
            && ps + pl == start
        {
            self.free.remove(&ps);
            start = ps;
            len += pl;
        }
        if let Some(&nl) = self.free.get(&(start + len)) {
            self.free.remove(&(start + len));
            len += nl;
        }
        self.free.insert(start, len);
        Some(size)
    }
}

// ---- Virtual heaps (NtAllocateVirtualMemory) --------------------------------------------

struct Reservation {
    size: u32,
    alloc_protect: u32,
    /// Guest protection of each page; 0 = reserved but not committed.
    pages: Vec<u32>,
    image: bool,
}

struct VirtualHeap {
    start: u32,
    end: u32,
    page: u32,
    map: BTreeMap<u32, Reservation>,
}

impl VirtualHeap {
    fn new((start, end): (u32, u32), page: u32) -> VirtualHeap {
        VirtualHeap { start, end, page, map: BTreeMap::new() }
    }

    fn containing(&self, addr: u32) -> Option<(u32, &Reservation)> {
        let (&s, r) = self.map.range(..=addr).next_back()?;
        (addr < s + r.size).then_some((s, r))
    }

    fn overlaps(&self, addr: u32, size: u32) -> bool {
        if let Some((&s, r)) = self.map.range(..addr + size).next_back() {
            return s + r.size > addr;
        }
        false
    }

    fn find_free(&self, size: u32, align: u32, top_down: bool) -> Option<u32> {
        let mut gaps = Vec::new();
        let mut cursor = self.start;
        for (&s, r) in &self.map {
            if s > cursor {
                gaps.push((cursor, s));
            }
            cursor = cursor.max(s + r.size);
        }
        if cursor < self.end {
            gaps.push((cursor, self.end));
        }
        let fit = |&(a, b): &(u32, u32)| -> Option<u32> {
            if top_down {
                let at = (b.checked_sub(size)?) / align * align;
                (at >= a).then_some(at)
            } else {
                let at = a.next_multiple_of(align);
                (at as u64 + size as u64 <= b as u64).then_some(at)
            }
        };
        if top_down { gaps.iter().rev().find_map(fit) } else { gaps.iter().find_map(fit) }
    }

    fn reserve_at(&mut self, addr: u32, size: u32, protect: u32, image: bool) {
        let pages = vec![0; (size / self.page) as usize];
        self.map.insert(addr, Reservation { size, alloc_protect: protect, pages, image });
    }

    /// Commits `[addr, addr+size)` inside one reservation.
    fn commit(&mut self, addr: u32, size: u32, protect: u32) -> Result<(), u32> {
        let page = self.page;
        let (s, r) = self.containing(addr).ok_or(STATUS_CONFLICTING_ADDRESSES)?;
        if addr + size > s + r.size {
            return Err(STATUS_CONFLICTING_ADDRESSES);
        }
        host_commit(addr, size, protect);
        let r = self.map.get_mut(&s).unwrap();
        for p in (addr - s) / page..(addr - s + size) / page {
            r.pages[p as usize] = protect;
        }
        Ok(())
    }

    fn decommit(&mut self, addr: u32, size: u32) -> Result<(), u32> {
        let page = self.page;
        let (s, r) = self.containing(addr).ok_or(STATUS_MEMORY_NOT_ALLOCATED)?;
        let size = if size == 0 { s + r.size - addr } else { size };
        if addr + size > s + r.size {
            return Err(STATUS_UNABLE_TO_FREE_VM);
        }
        host_decommit(addr, size);
        let r = self.map.get_mut(&s).unwrap();
        for p in (addr - s) / page..(addr - s + size) / page {
            r.pages[p as usize] = 0;
        }
        Ok(())
    }

    fn release(&mut self, addr: u32) -> Result<u32, u32> {
        let r = self.map.get(&addr).ok_or(STATUS_MEMORY_NOT_ALLOCATED)?;
        if r.image {
            return Err(STATUS_UNABLE_TO_FREE_VM);
        }
        let size = r.size;
        host_decommit(addr, size);
        self.map.remove(&addr);
        Ok(size)
    }
}

// ---- Physical memory (MmAllocatePhysicalMemoryEx) ------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// `0xA0000000`, 64 KB pages.
    A0,
    /// `0xC0000000`, 16 MB pages.
    C0,
    /// `0xE0000000`, 4 KB pages. Physical address = offset + 0x1000.
    E0,
}

impl View {
    fn base(self) -> u32 {
        match self {
            View::A0 => 0xA000_0000,
            View::C0 => 0xC000_0000,
            View::E0 => 0xE000_0000,
        }
    }
    fn page(self) -> u32 {
        match self {
            View::A0 => 0x1_0000,
            View::C0 => 0x100_0000,
            View::E0 => 0x1000,
        }
    }
    fn of(addr: u32) -> Option<View> {
        match addr >> 29 {
            5 => Some(View::A0),
            6 => Some(View::C0),
            7 => Some(View::E0),
            _ => None,
        }
    }
    /// Guest virtual address of physical address `phys` in this view.
    fn virt(self, phys: u32) -> u32 {
        match self {
            View::E0 => self.base() + phys - 0x1000,
            _ => self.base() + phys,
        }
    }
    fn phys(self, virt: u32) -> u32 {
        match self {
            View::E0 => virt - self.base() + 0x1000,
            _ => virt - self.base(),
        }
    }
}

struct PhysAlloc {
    size: u32,
    protect: u32,
}

// ---- The memory manager ---------------------------------------------------------------

struct Memory {
    heap4k: VirtualHeap,
    heap64k: VirtualHeap,
    image: VirtualHeap,
    physical: BTreeMap<u32, PhysAlloc>,
    kernel: Arena,
    stacks: Arena,
}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Memory) -> R) -> R {
    let mut g = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(|| Memory {
        heap4k: VirtualHeap::new(HEAP_4K, 0x1000),
        heap64k: VirtualHeap::new(HEAP_64K, 0x1_0000),
        image: VirtualHeap::new(IMAGE, 0x1000),
        physical: BTreeMap::new(),
        kernel: Arena::new(KERNEL.0 + 0x1_0000, KERNEL.1),
        stacks: Arena::new(STACKS.0, STACKS.1),
    });
    f(m)
}

fn heap_of(m: &mut Memory, addr: u32) -> Option<&mut VirtualHeap> {
    if (HEAP_4K.0..HEAP_4K.1).contains(&addr) {
        Some(&mut m.heap4k)
    } else if (HEAP_64K.0..HEAP_64K.1).contains(&addr) {
        Some(&mut m.heap64k)
    } else if (IMAGE.0..IMAGE.1).contains(&addr) {
        Some(&mut m.image)
    } else {
        None
    }
}

/// Registers the loaded image as committed memory (`MEM_IMAGE`).
pub fn map_image(addr: u32, size: u32) {
    host_commit(addr, size, PAGE_READWRITE);
    with(|m| {
        m.image.reserve_at(addr, size, PAGE_READWRITE, true);
        let r = m.image.map.get_mut(&addr).unwrap();
        r.pages.iter_mut().for_each(|p| *p = PAGE_READWRITE);
    });
}

/// Sets the protection of image pages after loading.
pub fn protect_image(addr: u32, size: u32, protect: u32) {
    host_protect(addr, size, protect);
    with(|m| {
        let (s, _) = m.image.containing(addr).map(|(s, r)| (s, r.size)).expect("image range");
        let r = m.image.map.get_mut(&s).unwrap();
        for p in (addr - s) / 0x1000..(addr - s + size) / 0x1000 {
            r.pages[p as usize] = protect;
        }
    });
}

/// `NtAllocateVirtualMemory`. Returns `(base, size)`.
pub fn allocate_virtual(base_hint: u32, size: u32, alloc_type: u32, protect: u32) -> Result<(u32, u32), u32> {
    if size == 0 || alloc_type & (MEM_COMMIT | MEM_RESERVE | MEM_RESET) == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    if alloc_type & MEM_RESET != 0 {
        // Contents may be discarded; keeping them is a valid implementation.
        return Ok((base_hint, size));
    }
    let page = if alloc_type & MEM_LARGE_PAGES != 0 { 0x1_0000 } else { 0x1000 };
    with(|m| {
        if base_hint != 0 {
            let heap = heap_of(m, base_hint).ok_or(STATUS_INVALID_PARAMETER)?;
            if heap.page != page && heap.start != IMAGE.0 {
                return Err(STATUS_ACCESS_DENIED);
            }
            let page = heap.page;
            let start = base_hint / page * page;
            let end = (base_hint as u64 + size as u64).next_multiple_of(page as u64);
            if end > heap.end as u64 {
                return Err(STATUS_INVALID_PARAMETER);
            }
            let len = (end - start as u64) as u32;
            if alloc_type & MEM_RESERVE != 0 {
                if heap.overlaps(start, len) {
                    return Err(STATUS_CONFLICTING_ADDRESSES);
                }
                heap.reserve_at(start, len, protect, false);
            }
            if alloc_type & MEM_COMMIT != 0 {
                heap.commit(start, len, protect)?;
            }
            Ok((start, len))
        } else {
            let heap = if page == 0x1000 { &mut m.heap4k } else { &mut m.heap64k };
            let len = size.next_multiple_of(page);
            let at = heap.find_free(len, page, alloc_type & MEM_TOP_DOWN != 0).ok_or(STATUS_NO_MEMORY)?;
            heap.reserve_at(at, len, protect, false);
            if alloc_type & MEM_COMMIT != 0 {
                heap.commit(at, len, protect)?;
            }
            Ok((at, len))
        }
    })
}

/// `NtFreeVirtualMemory`. Returns `(base, size)` freed.
pub fn free_virtual(addr: u32, size: u32, free_type: u32) -> Result<(u32, u32), u32> {
    with(|m| {
        let heap = heap_of(m, addr).ok_or(STATUS_MEMORY_NOT_ALLOCATED)?;
        let page = heap.page;
        if free_type & MEM_RELEASE != 0 {
            let start = addr / page * page;
            if size != 0 {
                return Err(STATUS_INVALID_PARAMETER);
            }
            let n = heap.release(start)?;
            Ok((start, n))
        } else if free_type & MEM_DECOMMIT != 0 {
            let start = addr / page * page;
            let len = if size == 0 { 0 } else { ((addr as u64 + size as u64).next_multiple_of(page as u64) - start as u64) as u32 };
            heap.decommit(start, len)?;
            Ok((start, len))
        } else {
            Err(STATUS_INVALID_PARAMETER)
        }
    })
}

/// `MEMORY_BASIC_INFORMATION` for an address.
pub struct RegionInfo {
    pub base: u32,
    pub allocation_base: u32,
    pub allocation_protect: u32,
    pub size: u32,
    pub state: u32,
    pub protect: u32,
    pub kind: u32,
}

/// `NtQueryVirtualMemory`.
pub fn query(addr: u32) -> Option<RegionInfo> {
    with(|m| {
        if let Some(v) = View::of(addr) {
            let phys = v.phys(addr);
            let (&s, a) = m.physical.range(..=phys).next_back().filter(|(s, a)| phys < *s + a.size)?;
            return Some(RegionInfo {
                base: addr / v.page() * v.page(),
                allocation_base: v.virt(s),
                allocation_protect: a.protect,
                size: v.virt(s) + a.size - addr / v.page() * v.page(),
                state: MEM_COMMIT,
                protect: a.protect,
                kind: MEM_PRIVATE,
            });
        }
        let heap = heap_of(m, addr)?;
        let page = heap.page;
        let Some((s, r)) = heap.containing(addr) else {
            // Free: report the gap up to the next reservation.
            let page_base = addr / page * page;
            let next = heap.map.range(addr..).next().map(|(&s, _)| s).unwrap_or(heap.end);
            return Some(RegionInfo {
                base: page_base,
                allocation_base: 0,
                allocation_protect: 0,
                size: next - page_base,
                state: MEM_FREE,
                protect: PAGE_NOACCESS,
                kind: 0,
            });
        };
        let first = ((addr - s) / page) as usize;
        let p0 = r.pages[first];
        let run = r.pages[first..].iter().take_while(|&&p| p == p0).count() as u32;
        Some(RegionInfo {
            base: s + first as u32 * page,
            allocation_base: s,
            allocation_protect: r.alloc_protect,
            size: run * page,
            state: if p0 == 0 { MEM_RESERVE } else { MEM_COMMIT },
            protect: p0,
            kind: if r.image { MEM_IMAGE } else { MEM_PRIVATE },
        })
    })
}

/// `MmQueryAllocationSize`.
pub fn allocation_size(addr: u32) -> u32 {
    with(|m| {
        if let Some(v) = View::of(addr) {
            let phys = v.phys(addr);
            return m.physical.range(..=phys).next_back().filter(|(s, a)| phys < *s + a.size).map_or(0, |(_, a)| a.size);
        }
        heap_of(m, addr).and_then(|h| h.containing(addr).map(|(_, r)| r.size)).unwrap_or(0)
    })
}

/// `MmAllocatePhysicalMemoryEx`: returns the virtual address, or 0.
pub fn allocate_physical(size: u32, protect: u32, min: u32, max: u32, align: u32) -> u32 {
    let view = if protect & MEM_LARGE_PAGES != 0 {
        View::A0
    } else if protect & MEM_16MB_PAGES != 0 {
        View::C0
    } else {
        View::E0
    };
    let page = view.page();
    let size = size.max(1).next_multiple_of(page);
    let align = align.max(page).next_power_of_two();
    // The range and alignment apply to the address the caller gets back, its
    // low 29 bits (the view offset), which in the E0 view is the physical
    // address minus 0x1000. The game's allocator (`sub_8225F650`) frees an
    // allocation whose `address & 0x1FFFFFFF` is below the minimum it asked
    // for, and asks for whole windows of exactly their size, so a placement
    // by physical address fails there.
    let shift: u64 = if view == View::E0 { 0x1000 } else { 0 };
    let lo = min.next_multiple_of(align) as u64;
    let hi = (max as u64 + 1).min(PHYSICAL_SIZE as u64 - shift);
    with(|m| {
        // Top-down first fit over the gaps between allocations, in view offsets.
        let mut gaps = Vec::new();
        let mut cursor = 0u64;
        for (&s, a) in &m.physical {
            if s as u64 > cursor {
                gaps.push((cursor, s as u64));
            }
            cursor = cursor.max(s as u64 + a.size as u64);
        }
        gaps.push((cursor, PHYSICAL_SIZE as u64));
        for &(a, b) in gaps.iter().rev() {
            let a = a.saturating_sub(shift).max(lo);
            let b = b.saturating_sub(shift).min(hi);
            if b < size as u64 {
                continue;
            }
            let at = (b - size as u64) / align as u64 * align as u64;
            if at >= a {
                let phys = (at + shift) as u32;
                m.physical.insert(phys, PhysAlloc { size, protect });
                let virt = view.virt(phys);
                host_commit(virt, size, protect & 0xFF);
                return virt;
            }
        }
        0
    })
}

/// `MmFreePhysicalMemory`.
pub fn free_physical(addr: u32) -> bool {
    let Some(v) = View::of(addr) else { return false };
    let phys = v.phys(addr);
    with(|m| match m.physical.remove(&phys) {
        Some(a) => {
            host_decommit(addr, a.size);
            true
        }
        None => false,
    })
}

/// Bytes of physical memory currently allocated (for diagnostics).
pub fn physical_in_use() -> u64 {
    with(|m| m.physical.values().map(|a| a.size as u64).sum())
}

/// `MmGetPhysicalAddress`. Only physical-view addresses have one.
pub fn physical_address(addr: u32) -> Option<u32> {
    View::of(addr).map(|v| v.phys(addr))
}

/// Protection of the page holding `addr`, or 0 if it isn't committed.
pub fn protect_of(addr: u32) -> u32 {
    query(addr).filter(|r| r.state == MEM_COMMIT).map_or(0, |r| r.protect)
}

// ---- Kernel-owned memory ------------------------------------------------------------------

/// Allocates zeroed kernel memory (objects the console kernel would own:
/// thread blocks, PCRs, pool allocations, XAM buffers).
pub fn kalloc(size: u32, align: u32) -> u32 {
    let a = with(|m| m.kernel.alloc(size, align.max(16)))
        .unwrap_or_else(|| panic!("FATAL: kernel memory region exhausted allocating 0x{size:X} bytes"));
    fill(a, size as usize, 0);
    a
}

pub fn kfree(addr: u32) -> bool {
    with(|m| m.kernel.free(addr)).is_some()
}

pub fn is_kernel(addr: u32) -> bool {
    (KERNEL.0..KERNEL.1).contains(&addr)
}

/// Allocates and commits a guest thread stack; returns its low address.
pub fn alloc_stack(size: u32) -> u32 {
    let size = size.next_multiple_of(0x1_0000);
    let a = with(|m| m.stacks.alloc(size + 0x1_0000, 0x1_0000))
        .unwrap_or_else(|| panic!("FATAL: stack region exhausted allocating 0x{size:X} bytes"));
    // The lowest 64 KB stays uncommitted: a guard against overflow.
    host_commit(a + 0x1_0000, size, PAGE_READWRITE);
    a + 0x1_0000
}

pub fn free_stack(low: u32) {
    let a = low - 0x1_0000;
    if let Some(size) = with(|m| m.stacks.free(a)) {
        host_decommit(low, size - 0x1_0000);
    }
}

/// Human description of a guest address, for fault diagnostics. Never blocks.
pub fn describe(addr: u32) -> String {
    if addr < NULL_GUARD_END {
        return "the null page (a null or near-null pointer)".into();
    }
    if (MMIO.0..MMIO.1).contains(&addr) {
        let what = match addr >> 16 {
            0x7FC8..=0x7FCF => "Xenos GPU registers",
            0x7FEA => "the XMA / audio block",
            _ => "hardware registers",
        };
        return format!("MMIO: {what}. Recompiled code must never touch hardware; the function doing this must be replaced natively");
    }
    if (0x7F00_0000..MMIO.0).contains(&addr) {
        return "the unmapped region below MMIO".into();
    }
    let Ok(g) = MEMORY.try_lock() else { return "guest memory (allocator busy)".into() };
    let Some(m) = g.as_ref() else { return "guest memory (not initialised)".into() };
    let region = if (STACKS.0..STACKS.1).contains(&addr) {
        "the stack region (a stack guard page, or no stack)"
    } else if is_kernel(addr) {
        "kernel memory"
    } else if let Some(v) = View::of(addr) {
        let phys = v.phys(addr);
        if m.physical.range(..=phys).next_back().is_some_and(|(s, a)| phys < s + a.size) {
            "physical memory (allocated)"
        } else {
            "physical memory that isn't allocated"
        }
    } else {
        let h = if (HEAP_4K.0..HEAP_4K.1).contains(&addr) {
            Some(&m.heap4k)
        } else if (HEAP_64K.0..HEAP_64K.1).contains(&addr) {
            Some(&m.heap64k)
        } else if (IMAGE.0..IMAGE.1).contains(&addr) {
            Some(&m.image)
        } else {
            None
        };
        match h.map(|h| h.containing(addr).map(|(s, r)| r.pages[((addr - s) / h.page) as usize])) {
            Some(Some(0)) => "reserved but not committed virtual memory",
            Some(Some(p)) if p & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 => "read-only memory",
            Some(Some(_)) => "committed memory",
            Some(None) => "unallocated virtual memory",
            None => "an unused part of the address space",
        }
    };
    region.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address range of `MmAllocatePhysicalMemoryEx` constrains the
    /// returned address's view offset: a 4 KB-page request for a whole window
    /// gets the window's first address back (`sub_8225F650` checks this).
    #[test]
    fn physical_ranges_apply_to_the_view_offset() {
        init();
        let (min, max) = (0x1AF5_8000, 0x1B5D_8000);
        let e0 = allocate_physical(max - min, PAGE_READWRITE, min, max - 1, 4);
        assert_eq!(e0, 0xE000_0000 + min);
        assert_eq!(physical_address(e0), Some(min + 0x1000));
        assert_eq!(allocate_physical(0x1000, PAGE_READWRITE, min, max - 1, 4), 0, "the window is full");
        assert!(free_physical(e0));
        // 64 KB pages: the same rule, with no offset.
        let a0 = allocate_physical(0x60000, PAGE_READWRITE | MEM_LARGE_PAGES, 0x1AF6_0000, 0x1AFB_FFFF, 4);
        assert_eq!(a0, 0xBAF6_0000);
        assert!(free_physical(a0));
    }
}
