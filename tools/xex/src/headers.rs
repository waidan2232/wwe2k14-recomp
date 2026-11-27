//! XEX2 file header, optional headers and security info.
//!
//! All XEX header fields are big-endian.

use std::fmt;

use crate::bytes::{be16, be32, latin1, slice};
use crate::{Error, Result};

/// Optional-header keys. The low byte encodes how the value is stored:
/// `0x00`/`0x01` = the value is inline, `0xFF` = the value is a file offset to
/// size-prefixed data, anything else = a file offset to `low_byte * 4` bytes.
pub mod key {
    pub const RESOURCE_INFO: u32 = 0x0000_02FF;
    pub const FILE_FORMAT_INFO: u32 = 0x0000_03FF;
    pub const DELTA_PATCH_DESCRIPTOR: u32 = 0x0000_05FF;
    pub const BOUNDING_PATH: u32 = 0x0000_80FF;
    pub const ORIGINAL_BASE_ADDRESS: u32 = 0x0001_0001;
    pub const ENTRY_POINT: u32 = 0x0001_0100;
    pub const IMAGE_BASE_ADDRESS: u32 = 0x0001_0201;
    pub const IMPORT_LIBRARIES: u32 = 0x0001_03FF;
    pub const CHECKSUM_TIMESTAMP: u32 = 0x0001_8002;
    pub const ORIGINAL_PE_NAME: u32 = 0x0001_83FF;
    pub const STATIC_LIBRARIES: u32 = 0x0002_00FF;
    pub const TLS_INFO: u32 = 0x0002_0104;
    pub const DEFAULT_STACK_SIZE: u32 = 0x0002_0200;
    pub const DEFAULT_FILESYSTEM_CACHE_SIZE: u32 = 0x0002_0301;
    pub const DEFAULT_HEAP_SIZE: u32 = 0x0002_0401;
    pub const SYSTEM_FLAGS: u32 = 0x0003_0000;
    pub const EXECUTION_INFO: u32 = 0x0004_0006;
    pub const TITLE_WORKSPACE_SIZE: u32 = 0x0004_0201;
    pub const GAME_RATINGS: u32 = 0x0004_0310;
    pub const LAN_KEY: u32 = 0x0004_0404;
    pub const XBOX360_LOGO: u32 = 0x0004_05FF;
    pub const MULTIDISC_MEDIA_IDS: u32 = 0x0004_06FF;
    pub const ALTERNATE_TITLE_IDS: u32 = 0x0004_07FF;
    pub const ADDITIONAL_TITLE_MEMORY: u32 = 0x0004_0801;
    pub const EXPORTS_BY_NAME: u32 = 0x00E1_0402;

    pub fn name(key: u32) -> Option<&'static str> {
        Some(match key {
            RESOURCE_INFO => "resource info",
            FILE_FORMAT_INFO => "file format info",
            DELTA_PATCH_DESCRIPTOR => "delta patch descriptor",
            BOUNDING_PATH => "bounding path",
            ORIGINAL_BASE_ADDRESS => "original base address",
            ENTRY_POINT => "entry point",
            IMAGE_BASE_ADDRESS => "image base address",
            IMPORT_LIBRARIES => "import libraries",
            CHECKSUM_TIMESTAMP => "checksum/timestamp",
            ORIGINAL_PE_NAME => "original PE name",
            STATIC_LIBRARIES => "static libraries",
            TLS_INFO => "TLS info",
            DEFAULT_STACK_SIZE => "default stack size",
            DEFAULT_FILESYSTEM_CACHE_SIZE => "default filesystem cache size",
            DEFAULT_HEAP_SIZE => "default heap size",
            SYSTEM_FLAGS => "system flags",
            EXECUTION_INFO => "execution info",
            TITLE_WORKSPACE_SIZE => "title workspace size",
            GAME_RATINGS => "game ratings",
            LAN_KEY => "LAN key",
            XBOX360_LOGO => "Xbox 360 logo",
            MULTIDISC_MEDIA_IDS => "multidisc media IDs",
            ALTERNATE_TITLE_IDS => "alternate title IDs",
            ADDITIONAL_TITLE_MEMORY => "additional title memory",
            EXPORTS_BY_NAME => "exports by name",
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct OptionalHeader {
    pub key: u32,
    /// The inline value, or the file offset of `data` for out-of-line headers.
    pub value: u32,
    /// The out-of-line payload (empty for inline headers). For size-prefixed
    /// headers (key low byte `0xFF`) this includes the leading size word.
    pub data: Vec<u8>,
}

impl OptionalHeader {
    pub fn is_inline(&self) -> bool {
        matches!(self.key & 0xFF, 0x00 | 0x01)
    }
}

/// `major.minor.build.qfe` packed as 4/4/16/8 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub build: u16,
    pub qfe: u8,
}

impl Version {
    pub fn from_u32(v: u32) -> Self {
        Version {
            major: (v >> 28) as u8,
            minor: ((v >> 24) & 0xF) as u8,
            build: ((v >> 8) & 0xFFFF) as u16,
            qfe: (v & 0xFF) as u8,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.major, self.minor, self.build, self.qfe)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionInfo {
    pub media_id: u32,
    pub version: Version,
    pub base_version: Version,
    pub title_id: u32,
    pub platform: u8,
    pub executable_type: u8,
    pub disc_number: u8,
    pub disc_count: u8,
    pub savegame_id: u32,
}

/// An XDK library statically linked into the executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticLibrary {
    pub name: String,
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    /// Final version word, kept raw. It packs the QFE number with approval bits.
    pub qfe_and_flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub name: String,
    pub address: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsInfo {
    pub slot_count: u32,
    pub raw_data_address: u32,
    pub data_size: u32,
    pub raw_data_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encryption {
    None,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicBlock {
    pub data_size: u32,
    pub zero_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compression {
    None,
    /// Runs of stored data, each followed by a run of zeros.
    Basic { blocks: Vec<BasicBlock> },
    /// LZX ("normal" in XDK terms).
    Lzx { window_size: u32, first_block_size: u32, first_block_hash: [u8; 20] },
    /// Delta patch against a base executable (title updates).
    Delta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFormat {
    pub encryption: Encryption,
    pub compression: Compression,
}

#[derive(Debug, Clone)]
pub struct Header {
    pub module_flags: u32,
    /// File offset of the image payload. This is also the header length.
    pub header_size: u32,
    pub security_offset: u32,
    pub optional: Vec<OptionalHeader>,
}

impl Header {
    pub fn parse(file: &[u8]) -> Result<Header> {
        let magic = slice(file, 0, 4, "XEX magic")?;
        if magic != b"XEX2" {
            return Err(Error::BadMagic(magic.try_into().unwrap()));
        }
        let module_flags = be32(file, 0x04, "module flags")?;
        let header_size = be32(file, 0x08, "header size")?;
        let security_offset = be32(file, 0x10, "security info offset")?;
        let count = be32(file, 0x14, "optional header count")? as usize;
        if count > 0x400 {
            return Err(Error::Invalid(format!("implausible optional header count {count}")));
        }
        let mut optional = Vec::with_capacity(count);
        for i in 0..count {
            let entry = 0x18 + i * 8;
            let key = be32(file, entry, "optional header key")?;
            let value = be32(file, entry + 4, "optional header value")?;
            let data = match key & 0xFF {
                0x00 | 0x01 => Vec::new(),
                0xFF => {
                    let len = be32(file, value as usize, "optional header size")? as usize;
                    slice(file, value as usize, len, "optional header data")?.to_vec()
                }
                dwords => slice(file, value as usize, dwords as usize * 4, "optional header data")?.to_vec(),
            };
            optional.push(OptionalHeader { key, value, data });
        }
        Ok(Header { module_flags, header_size, security_offset, optional })
    }

    pub fn get(&self, key: u32) -> Option<&OptionalHeader> {
        self.optional.iter().find(|h| h.key == key)
    }

    pub fn inline(&self, key: u32) -> Option<u32> {
        self.get(key).filter(|h| h.is_inline()).map(|h| h.value)
    }

    pub fn data(&self, key: u32) -> Option<&[u8]> {
        self.get(key).filter(|h| !h.is_inline()).map(|h| h.data.as_slice())
    }

    pub fn entry_point(&self) -> Option<u32> {
        self.inline(key::ENTRY_POINT)
    }

    pub fn image_base(&self) -> Option<u32> {
        self.inline(key::IMAGE_BASE_ADDRESS)
    }

    pub fn default_stack_size(&self) -> Option<u32> {
        self.inline(key::DEFAULT_STACK_SIZE)
    }

    pub fn default_heap_size(&self) -> Option<u32> {
        self.inline(key::DEFAULT_HEAP_SIZE)
    }

    pub fn title_workspace_size(&self) -> Option<u32> {
        self.inline(key::TITLE_WORKSPACE_SIZE)
    }

    pub fn system_flags(&self) -> Option<u32> {
        self.inline(key::SYSTEM_FLAGS)
    }

    pub fn original_pe_name(&self) -> Option<String> {
        self.data(key::ORIGINAL_PE_NAME).map(|d| latin1(d.get(4..).unwrap_or_default()))
    }

    pub fn execution_info(&self) -> Result<Option<ExecutionInfo>> {
        let Some(d) = self.data(key::EXECUTION_INFO) else { return Ok(None) };
        let what = "execution info";
        Ok(Some(ExecutionInfo {
            media_id: be32(d, 0, what)?,
            version: Version::from_u32(be32(d, 4, what)?),
            base_version: Version::from_u32(be32(d, 8, what)?),
            title_id: be32(d, 12, what)?,
            platform: slice(d, 16, 1, what)?[0],
            executable_type: slice(d, 17, 1, what)?[0],
            disc_number: slice(d, 18, 1, what)?[0],
            disc_count: slice(d, 19, 1, what)?[0],
            savegame_id: be32(d, 20, what)?,
        }))
    }

    pub fn static_libraries(&self) -> Result<Vec<StaticLibrary>> {
        let Some(d) = self.data(key::STATIC_LIBRARIES) else { return Ok(Vec::new()) };
        let what = "static library";
        (4..d.len())
            .step_by(16)
            .take_while(|&o| o + 16 <= d.len())
            .map(|o| {
                Ok(StaticLibrary {
                    name: latin1(slice(d, o, 8, what)?),
                    major: be16(d, o + 8, what)?,
                    minor: be16(d, o + 10, what)?,
                    build: be16(d, o + 12, what)?,
                    qfe_and_flags: be16(d, o + 14, what)?,
                })
            })
            .collect()
    }

    pub fn resources(&self) -> Result<Vec<Resource>> {
        let Some(d) = self.data(key::RESOURCE_INFO) else { return Ok(Vec::new()) };
        let what = "resource entry";
        (4..d.len())
            .step_by(16)
            .take_while(|&o| o + 16 <= d.len())
            .map(|o| {
                Ok(Resource {
                    name: latin1(slice(d, o, 8, what)?),
                    address: be32(d, o + 8, what)?,
                    size: be32(d, o + 12, what)?,
                })
            })
            .collect()
    }

    pub fn tls_info(&self) -> Result<Option<TlsInfo>> {
        let Some(d) = self.data(key::TLS_INFO) else { return Ok(None) };
        let what = "TLS info";
        Ok(Some(TlsInfo {
            slot_count: be32(d, 0, what)?,
            raw_data_address: be32(d, 4, what)?,
            data_size: be32(d, 8, what)?,
            raw_data_size: be32(d, 12, what)?,
        }))
    }

    pub fn file_format(&self) -> Result<FileFormat> {
        let d = self
            .data(key::FILE_FORMAT_INFO)
            .ok_or_else(|| Error::Invalid("missing file format info header".into()))?;
        let what = "file format info";
        let info_size = be32(d, 0, what)? as usize;
        let encryption = match be16(d, 4, what)? {
            0 => Encryption::None,
            1 => Encryption::Normal,
            n => return Err(Error::Unsupported(format!("encryption type {n}"))),
        };
        let compression = match be16(d, 6, what)? {
            0 => Compression::None,
            1 => {
                let count = info_size.saturating_sub(8) / 8;
                let blocks = (0..count)
                    .map(|i| {
                        Ok(BasicBlock {
                            data_size: be32(d, 8 + i * 8, "basic compression block")?,
                            zero_size: be32(d, 12 + i * 8, "basic compression block")?,
                        })
                    })
                    .collect::<Result<_>>()?;
                Compression::Basic { blocks }
            }
            2 => Compression::Lzx {
                window_size: be32(d, 8, what)?,
                first_block_size: be32(d, 0x0C, what)?,
                first_block_hash: slice(d, 0x10, 20, what)?.try_into().unwrap(),
            },
            3 => Compression::Delta,
            n => return Err(Error::Unsupported(format!("compression type {n}"))),
        };
        Ok(FileFormat { encryption, compression })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Code,
    Data,
    ReadOnly,
    Other(u8),
}

impl fmt::Display for PageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageKind::Code => f.write_str("code"),
            PageKind::Data => f.write_str("data"),
            PageKind::ReadOnly => f.write_str("read-only"),
            PageKind::Other(n) => write!(f, "kind {n}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageDescriptor {
    pub kind: PageKind,
    pub page_count: u32,
    /// SHA-1 from the descriptor. It hashes these pages followed by the next
    /// descriptor (see `Xex::verify_pages`).
    pub digest: [u8; 20],
}

/// A run of consecutive pages with the same kind, as guest addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRegion {
    pub kind: PageKind,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone)]
pub struct SecurityInfo {
    pub header_size: u32,
    pub image_size: u32,
    pub image_flags: u32,
    pub load_address: u32,
    /// Per-file AES key, itself encrypted with the retail (or devkit) key.
    pub encrypted_key: [u8; 16],
    /// SHA-1 heading the page hash chain: it covers the first descriptor's pages
    /// followed by that descriptor.
    pub image_digest: [u8; 20],
    pub region: u32,
    pub allowed_media_types: u32,
    pub pages: Vec<PageDescriptor>,
}

impl SecurityInfo {
    const PAGE_SIZE_4KB_FLAG: u32 = 0x1000_0000;

    pub fn parse(file: &[u8], offset: u32) -> Result<SecurityInfo> {
        let s = offset as usize;
        let what = "security info";
        let page_count = be32(file, s + 0x180, what)? as usize;
        if page_count > 0x10_0000 {
            return Err(Error::Invalid(format!("implausible page descriptor count {page_count}")));
        }
        let pages = (0..page_count)
            .map(|i| {
                let info = be32(file, s + 0x184 + i * 24, "page descriptor")?;
                Ok(PageDescriptor {
                    kind: match info & 0xF {
                        1 => PageKind::Code,
                        2 => PageKind::Data,
                        3 => PageKind::ReadOnly,
                        n => PageKind::Other(n as u8),
                    },
                    page_count: info >> 4,
                    digest: slice(file, s + 0x188 + i * 24, 20, "page descriptor")?.try_into().unwrap(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(SecurityInfo {
            header_size: be32(file, s, what)?,
            image_size: be32(file, s + 0x004, what)?,
            image_flags: be32(file, s + 0x10C, what)?,
            load_address: be32(file, s + 0x110, what)?,
            encrypted_key: slice(file, s + 0x150, 16, what)?.try_into().unwrap(),
            image_digest: slice(file, s + 0x114, 20, what)?.try_into().unwrap(),
            region: be32(file, s + 0x178, what)?,
            allowed_media_types: be32(file, s + 0x17C, what)?,
            pages,
        })
    }

    pub fn page_size(&self) -> u32 {
        if self.image_flags & Self::PAGE_SIZE_4KB_FLAG != 0 { 0x1000 } else { 0x1_0000 }
    }

    /// Consecutive same-kind pages merged into address ranges.
    pub fn regions(&self) -> Vec<PageRegion> {
        let mut out: Vec<PageRegion> = Vec::new();
        let mut addr = self.load_address;
        for p in &self.pages {
            let end = addr.wrapping_add(p.page_count.wrapping_mul(self.page_size()));
            match out.last_mut() {
                Some(last) if last.kind == p.kind => last.end = end,
                _ => out.push(PageRegion { kind: p.kind, start: addr, end }),
            }
            addr = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_unpacks() {
        let v = Version::from_u32(0x2052_FE00 | 0x0000_0003);
        assert_eq!((v.major, v.minor, v.build, v.qfe), (2, 0, 0x52FE, 3));
        // xboxkrnl.exe import version in WWE 2K14's default.xex
        assert_eq!(Version::from_u32(0x2053_0200).to_string(), "2.0.21250.0");
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(matches!(Header::parse(b"XEX1\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"), Err(Error::BadMagic(_))));
        assert!(matches!(Header::parse(b"XE"), Err(Error::Truncated { .. })));
    }
}
