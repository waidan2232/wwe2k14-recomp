//! Phase 0 exit-gate checks against the real WWE 2K14 executable.
//! Runs only when `WWE14_XEX` points at the extracted `default.xex`.

use xex::{Compression, Encryption, KeyKind, Xex};

fn load() -> Option<Xex> {
    let Ok(path) = std::env::var("WWE14_XEX") else {
        eprintln!("WWE14_XEX not set; skipping");
        return None;
    };
    let bytes = std::fs::read(&path).expect("read default.xex");
    Some(Xex::parse(&bytes).expect("parse default.xex"))
}

#[test]
fn header_and_image() {
    let Some(x) = load() else { return };
    let exec = x.header.execution_info().unwrap().expect("execution info");
    assert_eq!(exec.title_id, 0x5454_08B2);
    assert_eq!(x.header.original_pe_name().as_deref(), Some("WWE14_DVD_MASTER.exe"));
    assert_eq!(x.file_format.encryption, Encryption::Normal);
    assert!(matches!(x.file_format.compression, Compression::Basic { .. }));
    assert_eq!(x.key, KeyKind::Retail);
    assert_eq!(x.base_address, 0x8200_0000);
    assert_eq!(x.entry_point(), Some(0x8226_2638));
    assert_eq!(x.security.image_size, 0x1A6_0000);
    assert!(x.image.starts_with(b"MZ"));
}

#[test]
fn sections_and_pdata() {
    let Some(x) = load() else { return };
    let names: Vec<&str> = x.sections.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            ".rdata", ".pdata", "BINKCONS", ".text", "BINK", "RADCODE", "BINKBSS", ".data", ".tls", ".XBMOVIE",
            "BINKDATA", "RADDATA", ".idata", ".XBLD", ".reloc"
        ]
    );
    let text = x.section(".text").unwrap();
    assert_eq!((text.address, text.end()), (0x8225_0000, 0x8359_8034));
    assert_eq!(x.code_sections().count(), 3);

    let pdata = x.pdata().unwrap();
    assert_eq!(pdata.len(), 58_508);
    assert_eq!(pdata.iter().filter(|f| f.has_exception_handler).count(), 56);
    let largest = pdata.iter().max_by_key(|f| f.length).unwrap();
    assert_eq!((largest.begin, largest.length), (0x834B_0D18, 0xBC0C));
}

#[test]
fn imports_resolve() {
    let Some(x) = load() else { return };
    let krnl = x.import_library("xboxkrnl.exe").expect("xboxkrnl.exe imports");
    assert_eq!(krnl.functions().count(), 166);
    assert_eq!(krnl.variables().len(), 12);
    assert_eq!(krnl.records.len(), 344);
    let xam = x.import_library("xam.xex").expect("xam.xex imports");
    assert_eq!(xam.functions().count(), 104);
    assert_eq!(xam.variables().len(), 0);
    assert_eq!(xam.records.len(), 208);
    assert!(x.imports.iter().flat_map(|l| &l.records).all(|r| r.name.is_some()), "unnamed import ordinal");
    for name in ["VdSwap", "XAudioSubmitRenderDriverFrame", "XMACreateContext", "ExCreateThread"] {
        assert!(krnl.functions().any(|r| r.name == Some(name)), "missing {name}");
    }
    assert!(xam.functions().any(|r| r.name == Some("XamInputGetState")));
}

/// Title Update 1. Runs when `WWE14_XEX` and `WWE14_TU1_XEXP` (the extracted
/// `default.xexp`) are both set.
#[test]
fn title_update_1_applies() {
    let (Ok(base), Ok(patch)) = (std::env::var("WWE14_XEX"), std::env::var("WWE14_TU1_XEXP")) else {
        eprintln!("WWE14_XEX / WWE14_TU1_XEXP not set; skipping");
        return;
    };
    let (out, stats) =
        xex::apply_to_file(&std::fs::read(base).unwrap(), &std::fs::read(patch).unwrap()).expect("apply TU1");
    assert_eq!(stats.pages_verified, (422, 422));
    assert_eq!(stats.lzx_unused_bytes, 0);
    let x = Xex::parse(&out).expect("patched xex loads");
    assert_eq!(x.header.execution_info().unwrap().unwrap().version.to_string(), "0.0.1.2");
    assert_eq!(x.file_format.compression, Compression::None);
    assert_eq!(x.entry_point(), Some(0x8226_26B8));
    assert_eq!(x.pdata().unwrap().len(), 58_524);
}
