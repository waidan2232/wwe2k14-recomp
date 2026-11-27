//! Loads the executable's data into guest memory and fills in what the
//! console's loader provides: the import variables, the module entry
//! (`LDR_DATA_TABLE_ENTRY`), a copy of the XEX header, and page protections.
//!
//! The image is loaded as data. The game reads its own image (constants,
//! vtables, its XEX header); its code was recompiled ahead of time and the
//! image's code bytes are never executed or translated here.

use std::sync::OnceLock;

use xex::{PageKind, RecordKind, Xex};

use crate::mem;
use crate::object;
use crate::thread::ThreadSetup;

/// What the kernel and XAM need to know about the running title.
#[allow(dead_code)]
pub struct Module {
    pub path: std::path::PathBuf,
    pub base: u32,
    pub image_size: u32,
    pub entry: u32,
    pub default_stack: u32,
    pub title_id: u32,
    pub system_flags: u32,
    /// Guest copy of the XEX header (for `RtlImageXexHeaderField`).
    pub header: u32,
    /// Guest `LDR_DATA_TABLE_ENTRY` of the executable.
    pub ldr: u32,
    /// Guest `XEX_EXECUTION_ID` (inside the header copy), or 0.
    pub execution_id: u32,
    /// Fake module entries for the system modules (`XexGetModuleHandle`).
    pub xam_ldr: u32,
    pub kernel_ldr: u32,
    /// `KeTimeStampBundle`.
    pub timestamp_bundle: u32,
    /// `ExThreadObjectType`.
    pub thread_object_type: u32,
    pub tls: ThreadSetupInfo,
}

#[derive(Clone, Copy)]
pub struct ThreadSetupInfo {
    pub slots: u32,
    pub template: u32,
    pub raw_size: u32,
    pub data_size: u32,
}

static MODULE: OnceLock<Module> = OnceLock::new();

pub fn module() -> &'static Module {
    MODULE.get().expect("executable not loaded")
}

/// Same hash as `recomp` computes when it translates the image.
fn image_hash(image: &[u8]) -> u64 {
    image.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3))
}

fn guest_utf16(s: &str) -> u32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    let a = mem::kalloc(units.len() as u32 * 2 + 2, 4);
    for (i, u) in units.iter().enumerate() {
        mem::w16(a + 2 * i as u32, *u);
    }
    a
}

fn guest_unicode_string(at: u32, s: &str) {
    let buf = guest_utf16(s);
    let len = (s.encode_utf16().count() * 2) as u16;
    mem::w16(at, len);
    mem::w16(at + 2, len + 2);
    mem::w32(at + 4, buf);
}

/// Builds a module entry: `+0x18` DllBase, `+0x20` SizeOfImage, `+0x24`
/// FullDllName, `+0x2C` BaseDllName, `+0x34` Flags, `+0x3C` EntryPoint,
/// `+0x58` the XEX header (what the game reads through
/// `XexExecutableModuleHandle`).
fn ldr_entry(name: &str, base: u32, size: u32, entry: u32, header: u32) -> u32 {
    let l = mem::kalloc(0x80, 16);
    for off in [0, 8, 0x10] {
        object::init_list_head(l + off);
    }
    mem::w32(l + 0x18, base);
    mem::w32(l + 0x1C, base);
    mem::w32(l + 0x20, size);
    guest_unicode_string(l + 0x24, name);
    guest_unicode_string(l + 0x2C, name);
    mem::w32(l + 0x34, 0);
    mem::w32(l + 0x38, size);
    mem::w32(l + 0x3C, entry);
    mem::w16(l + 0x40, 1);
    mem::w32(l + 0x44, base);
    mem::w32(l + 0x58, header);
    l
}

/// Loads the executable. Stops if it isn't the one the code was recompiled from.
pub fn load(path: &std::path::Path, expected_hash: u64, command_line: &str) -> &'static Module {
    let bytes = std::fs::read(path).unwrap_or_else(|e| crate::diag::fatal(&format!("{}: {e}", path.display())));
    let xex = Xex::parse(&bytes).unwrap_or_else(|e| crate::diag::fatal(&format!("{}: {e}", path.display())));
    let hash = image_hash(&xex.image);
    if hash != expected_hash {
        crate::diag::fatal(&format!(
            "{} is not the executable this build was recompiled from (image hash {hash:016X}, expected {expected_hash:016X})",
            path.display()
        ));
    }

    // Image.
    let base = xex.base_address;
    let image_size = (xex.image.len() as u32).next_multiple_of(0x1_0000);
    mem::map_image(base, image_size);
    mem::write(base, &xex.image);

    // XEX header copy and the module entry.
    let header_len = xex.header.header_size.min(bytes.len() as u32);
    let header = mem::kalloc(header_len, 16);
    mem::write(header, &bytes[..header_len as usize]);
    let entry = xex.entry_point().unwrap_or_else(|| crate::diag::fatal("executable has no entry point"));
    let ldr = ldr_entry("default.xex", base, image_size, entry, header);
    let execution_id = xex
        .header
        .get(xex::key::EXECUTION_INFO)
        .filter(|h| !h.is_inline())
        .map_or(0, |h| header + h.value);
    let title_id = xex.header.execution_info().ok().flatten().map_or(0, |e| e.title_id);
    let xam_ldr = ldr_entry("xam.xex", 0x8100_0000, 0, 0, 0);
    let kernel_ldr = ldr_entry("xboxkrnl.exe", 0x8008_0000, 0, 0, 0);

    // Import variables.
    let thread_object_type = mem::kalloc(0x40, 16);
    mem::w32(thread_object_type, u32::from_be_bytes(*b"Thre"));
    let timestamp_bundle = mem::kalloc(0x18, 16);
    let exe_handle_cell = mem::kalloc(4, 4);
    mem::w32(exe_handle_cell, ldr);
    for lib in &xex.imports {
        for r in &lib.records {
            if r.kind != RecordKind::Slot || lib.functions().any(|f| f.ordinal == r.ordinal) {
                continue;
            }
            let name = r.name.unwrap_or("?");
            let var = match (lib.name.to_ascii_lowercase().as_str(), name) {
                ("xboxkrnl.exe", "ExThreadObjectType") => thread_object_type,
                ("xboxkrnl.exe", "KeTimeStampBundle") => timestamp_bundle,
                ("xboxkrnl.exe", "XexExecutableModuleHandle") => exe_handle_cell,
                // Pointers to debugger state: none attached.
                ("xboxkrnl.exe", "KeDebugMonitorData" | "KeCertMonitorData") => mem::kalloc(4, 4),
                ("xboxkrnl.exe", "XboxHardwareInfo") => {
                    // { u32 Flags; u8 NumberOfProcessors; u8 PCIBridgeRevisionID; u8 Reserved[6]; ... }
                    // Flags: 0x20 = hard disk present. Bit 0x10 (debug hardware) stays clear: retail.
                    let a = mem::kalloc(0x10, 4);
                    mem::w32(a, 0x20);
                    mem::w8(a + 4, 6);
                    a
                }
                ("xboxkrnl.exe", "XboxKrnlVersion") => {
                    // { u16 Major, Minor, Build, Qfe } of the final console kernel.
                    let a = mem::kalloc(8, 4);
                    mem::w16(a, 2);
                    mem::w16(a + 2, 0);
                    mem::w16(a + 4, 17559);
                    mem::w16(a + 6, 0);
                    a
                }
                ("xboxkrnl.exe", "ExLoadedCommandLine") => {
                    // The variable is the command-line string itself.
                    let a = mem::kalloc(0x400, 4);
                    let s = command_line.as_bytes();
                    mem::write(a, &s[..s.len().min(0x3FF)]);
                    a
                }
                // D3D's global device pointers: storage the game's D3D fills in.
                ("xboxkrnl.exe", "VdGlobalDevice" | "VdGlobalXamDevice") => mem::kalloc(4, 4),
                ("xboxkrnl.exe", "VdGpuClockInMHz") => {
                    let a = mem::kalloc(4, 4);
                    mem::w32(a, 500);
                    a
                }
                ("xboxkrnl.exe", "VdHSIOCalibrationLock") => {
                    let a = mem::kalloc(0x1C, 16);
                    crate::rtl::init_critical_section(a, 0);
                    a
                }
                _ => crate::diag::fatal(&format!(
                    "imported variable {}!{name} (ordinal 0x{:04X}) has no native definition",
                    lib.name, r.ordinal
                )),
            };
            mem::w32(r.address, var);
        }
    }

    // Page protections, now that the loader's writes are done.
    for region in xex.security.regions() {
        let protect = match region.kind {
            PageKind::Data => mem::PAGE_READWRITE,
            PageKind::Code | PageKind::ReadOnly => mem::PAGE_READONLY,
            PageKind::Other(_) => mem::PAGE_READWRITE,
        };
        let end = region.end.min(base + image_size);
        if region.start < end {
            mem::protect_image(region.start, end - region.start, protect);
        }
    }

    let tls = xex.header.tls_info().ok().flatten();
    let info = Module {
        path: path.to_path_buf(),
        base,
        image_size,
        entry,
        default_stack: xex.header.default_stack_size().unwrap_or(0x4_0000),
        title_id,
        system_flags: xex.header.system_flags().unwrap_or(0),
        header,
        ldr,
        execution_id,
        xam_ldr,
        kernel_ldr,
        timestamp_bundle,
        thread_object_type,
        tls: ThreadSetupInfo {
            slots: tls.map_or(64, |t| t.slot_count),
            template: tls.map_or(0, |t| t.raw_data_address),
            raw_size: tls.map_or(0, |t| t.raw_data_size),
            data_size: tls.map_or(0, |t| t.data_size),
        },
    };
    crate::thread::configure(ThreadSetup {
        tls_slots: info.tls.slots,
        tls_template: info.tls.template,
        tls_raw_size: info.tls.raw_size,
        tls_data_size: info.tls.data_size,
        default_stack: info.default_stack,
    });
    MODULE.set(info).ok().expect("executable loaded twice");
    module()
}

/// `RtlImageXexHeaderField`: the value of an inline field, or the address of
/// the field's data in the header copy; 0 if absent.
pub fn header_field(header: u32, key: u32) -> u32 {
    if header == 0 || mem::r32(header) != u32::from_be_bytes(*b"XEX2") {
        return 0;
    }
    let count = mem::r32(header + 0x14);
    for i in 0..count.min(0x400) {
        let e = header + 0x18 + i * 8;
        if mem::r32(e) == key {
            let v = mem::r32(e + 4);
            return if key & 0xFF <= 1 { v } else { header + v };
        }
    }
    0
}
