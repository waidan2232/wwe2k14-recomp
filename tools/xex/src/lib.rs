//! XEX2 (Xbox 360 executable) loader. It parses headers, decrypts and decompresses
//! the image, reads PE sections and `.pdata`, and resolves imports.
//!
//! The logic follows XenonRecomp's `XenonUtils/xex.cpp` (MIT). See THIRD_PARTY_NOTICES.md.
//! This is a build-time tool: it turns the executable into data for the recompiler.
//! Nothing here executes guest code.

mod bytes;
mod error;
pub mod exports;
pub mod headers;
mod image;
pub mod lzx;
pub mod patch;
mod sha1;
pub mod imports;
pub mod pe;

pub use error::{Error, Result};
pub use headers::{
    Compression, Encryption, ExecutionInfo, FileFormat, Header, PageKind, PageRegion, Resource, SecurityInfo,
    StaticLibrary, TlsInfo, Version, key,
};
pub use image::{DEVKIT_KEY, KeyKind, RETAIL_KEY};
pub use patch::{DeltaPatch, PatchDescriptor, PatchStats, apply_to_file, write_plain_xex};
pub use imports::{ImportLibrary, ImportRecord, RecordKind};
pub use pe::{RuntimeFunction, Section};

/// A fully loaded executable.
#[derive(Debug, Clone)]
pub struct Xex {
    pub header: Header,
    pub security: SecurityInfo,
    pub file_format: FileFormat,
    pub key: KeyKind,
    /// Guest address the image is loaded at.
    pub base_address: u32,
    /// The decrypted, decompressed PE image. Index = guest address - `base_address`.
    pub image: Vec<u8>,
    pub sections: Vec<Section>,
    pub imports: Vec<ImportLibrary>,
}

impl Xex {
    pub fn parse(file: &[u8]) -> Result<Xex> {
        let header = Header::parse(file)?;
        let security = SecurityInfo::parse(file, header.security_offset)?;
        let file_format = header.file_format()?;
        let (image, key) = image::build_image(file, &header, &security, &file_format)?;
        let base_address = header.image_base().unwrap_or(security.load_address);
        let sections = pe::parse_sections(&image, base_address)?;
        let imports = match header.data(key::IMPORT_LIBRARIES) {
            Some(data) => imports::parse(data, &image, base_address)?,
            None => Vec::new(),
        };
        Ok(Xex { header, security, file_format, key, base_address, image, sections, imports })
    }

    pub fn entry_point(&self) -> Option<u32> {
        self.header.entry_point()
    }

    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    pub fn code_sections(&self) -> impl Iterator<Item = &Section> {
        self.sections.iter().filter(|s| s.is_code())
    }

    /// `.pdata` function records (empty if the image has no `.pdata`).
    pub fn pdata(&self) -> Result<Vec<RuntimeFunction>> {
        self.section(".pdata").map_or(Ok(Vec::new()), |s| pe::parse_pdata(&self.image, s))
    }

    /// `len` image bytes starting at guest address `addr`.
    pub fn bytes_at(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let start = addr.checked_sub(self.base_address)? as usize;
        self.image.get(start..start.checked_add(len)?)
    }

    /// Checks the image against the signed page hash chain in the security info.
    /// Each link is SHA-1(pages of descriptor i ++ descriptor i as stored), held by
    /// descriptor i-1, or by the security info's image digest for i = 0.
    /// Returns (links that match, total links).
    pub fn verify_pages(&self) -> (usize, usize) {
        let page_size = self.security.page_size() as usize;
        let pages = &self.security.pages;
        let mut ok = 0;
        let mut off = 0usize;
        for (i, p) in pages.iter().enumerate() {
            let n = p.page_count as usize * page_size;
            let mut data = self.image.get(off..off + n).unwrap_or_default().to_vec();
            let kind = match p.kind {
                PageKind::Code => 1,
                PageKind::Data => 2,
                PageKind::ReadOnly => 3,
                PageKind::Other(k) => k as u32,
            };
            data.extend_from_slice(&((p.page_count << 4) | kind).to_be_bytes());
            data.extend_from_slice(&p.digest);
            let expected = if i == 0 { self.security.image_digest } else { pages[i - 1].digest };
            ok += (sha1::sha1(&data) == expected) as usize;
            off += n;
        }
        (ok, pages.len())
    }

    pub fn import_library(&self, name: &str) -> Option<&ImportLibrary> {
        self.imports.iter().find(|l| l.name.eq_ignore_ascii_case(name))
    }
}
