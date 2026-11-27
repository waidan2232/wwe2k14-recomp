//! Applying an XEX delta patch (`.xexp`, title updates) to its base executable.
//!
//! A `.xexp` is an XEX2 file whose file format info says compression 3 ("delta")
//! and which carries a delta patch descriptor (optional header 0x05FF):
//!
//! ```text
//! +0x00 size                         +0x30 size of target headers
//! +0x04 target version               +0x34 header copy: source offset
//! +0x08 source (base) version        +0x38 header copy: size
//! +0x0C SHA-1 digest of the base     +0x3C header copy: target offset
//! +0x20 base's encrypted image key   +0x40 image copy: source offset
//!                                    +0x44 image copy: size
//!                                    +0x48 image copy: target offset
//! +0x4C header delta patches (a patch list, see below), up to `size`
//! ```
//!
//! Producing the new executable:
//! 1. Headers: start from the base file's raw header bytes, apply the optional
//!    block copy, then apply the header patch list. The result is the title
//!    update's complete XEX header (its own execution info, security info,
//!    import table, ...).
//! 2. Image: start from the base's decrypted, decompressed image, resized to the
//!    new image size, apply the optional block copy, then walk the patch payload.
//!    The payload is laid out like LZX ("normal") compressed data: a chain of
//!    blocks, the first described in the file format info (size + SHA-1) and each
//!    block starting with the size + SHA-1 of the next. After that 24-byte link
//!    a block holds chunks, each a big-endian u16 length followed by a patch list.
//!
//! A patch list is a run of records, ending at the end of its buffer or at an
//! all-zero record:
//!
//! ```text
//! u32 old offset, u32 new offset, u16 uncompressed length, u16 compressed length
//! compressed length 0: zero-fill new[..len]
//! compressed length 1: copy old[..len] to new[..len]
//! otherwise:           `compressed length` bytes of LZX follow; decode them with
//!                      the window pre-loaded with old[..len] and write new[..len]
//! ```
//!
//! Offsets are relative to the buffer being patched (header bytes or image).
//! The LZX window size is the one in the patch's file format info.
//!
//! Keys: the target session key is the patched security info's image key
//! decrypted with the retail (or devkit) key. The descriptor's key at +0x20 is the
//! base session key encrypted with the target session key, so it identifies the
//! base. The `.xexp`'s own image key, decrypted with the target session key, is the
//! AES-128-CBC key of the patch payload.

use crate::bytes::{be16, be32, slice};
use crate::headers::{Encryption, Header, SecurityInfo, Version, key};
use crate::image::{DEVKIT_KEY, RETAIL_KEY, aes128_cbc_decrypt};
use crate::sha1::sha1;
use crate::{Error, Result, Xex, lzx};

/// The parsed delta patch descriptor.
#[derive(Debug, Clone)]
pub struct PatchDescriptor {
    pub size: u32,
    pub target_version: Version,
    pub source_version: Version,
    pub source_digest: [u8; 20],
    pub source_image_key: [u8; 16],
    pub size_of_target_headers: u32,
    pub headers_source_offset: u32,
    pub headers_source_size: u32,
    pub headers_target_offset: u32,
    pub image_source_offset: u32,
    pub image_source_size: u32,
    pub image_target_offset: u32,
    /// The header patch list (descriptor bytes from +0x4C).
    pub header_patches: Vec<u8>,
}

impl PatchDescriptor {
    pub fn parse(d: &[u8]) -> Result<PatchDescriptor> {
        let w = "delta patch descriptor";
        Ok(PatchDescriptor {
            size: be32(d, 0, w)?,
            target_version: Version::from_u32(be32(d, 4, w)?),
            source_version: Version::from_u32(be32(d, 8, w)?),
            source_digest: slice(d, 0x0C, 20, w)?.try_into().unwrap(),
            source_image_key: slice(d, 0x20, 16, w)?.try_into().unwrap(),
            size_of_target_headers: be32(d, 0x30, w)?,
            headers_source_offset: be32(d, 0x34, w)?,
            headers_source_size: be32(d, 0x38, w)?,
            headers_target_offset: be32(d, 0x3C, w)?,
            image_source_offset: be32(d, 0x40, w)?,
            image_source_size: be32(d, 0x44, w)?,
            image_target_offset: be32(d, 0x48, w)?,
            header_patches: d.get(0x4C..).unwrap_or_default().to_vec(),
        })
    }
}

/// What applying a patch did, for reporting.
#[derive(Debug, Clone, Default)]
pub struct PatchStats {
    pub header_records: usize,
    pub image_blocks: usize,
    pub image_records: usize,
    pub zero_records: usize,
    pub copy_records: usize,
    pub lzx_records: usize,
    pub lzx_input_bytes: usize,
    pub lzx_output_bytes: usize,
    /// Compressed bytes the decoder did not need (0 when every stream ends exactly).
    pub lzx_unused_bytes: usize,
    /// Page hash chain links of the result that match, and the total.
    pub pages_verified: (usize, usize),
}

/// A parsed `.xexp`. Its payload is decrypted while applying, because the key may
/// only be known once the headers are patched.
pub struct DeltaPatch {
    pub header: Header,
    pub security: SecurityInfo,
    pub descriptor: PatchDescriptor,
    pub window_size: u32,
    first_block: (u32, [u8; 20]),
    encryption: Encryption,
    /// The payload as stored (still encrypted if `encryption` says so).
    payload: Vec<u8>,
}

impl DeltaPatch {
    pub fn parse(file: &[u8]) -> Result<DeltaPatch> {
        let header = Header::parse(file)?;
        let security = SecurityInfo::parse(file, header.security_offset)?;
        let format = header.file_format()?;
        if format.compression != crate::Compression::Delta {
            return Err(Error::Invalid(format!("not a delta patch (compression {:?})", format.compression)));
        }
        let descriptor = PatchDescriptor::parse(
            header
                .data(key::DELTA_PATCH_DESCRIPTOR)
                .ok_or_else(|| Error::Invalid("delta patch has no delta patch descriptor (0x05FF)".into()))?,
        )?;
        let ff = header.data(key::FILE_FORMAT_INFO).unwrap();
        let w = "delta file format info";
        let window_size = be32(ff, 8, w)?;
        let first_block = (be32(ff, 0x0C, w)?, slice(ff, 0x10, 20, w)?.try_into().unwrap());
        let raw = slice(file, header.header_size as usize, file.len() - header.header_size as usize, "patch payload")?;

        let payload = raw.to_vec();
        let encryption = format.encryption;
        Ok(DeltaPatch { header, security, descriptor, window_size, first_block, encryption, payload })
    }

    /// Decrypts the payload with `session` (AES-128-CBC, zero IV) and checks the
    /// first block's SHA-1.
    fn decrypt_payload(&self, session: &[u8; 16]) -> Result<Vec<u8>> {
        let mut data = self.payload.clone();
        if self.encryption == Encryption::Normal {
            aes128_cbc_decrypt(session, &mut data);
        }
        let n = (self.first_block.0 as usize).min(data.len());
        if sha1(&data[..n]) != self.first_block.1 {
            return Err(Error::Invalid("patch payload: first block SHA-1 mismatch after decryption".into()));
        }
        Ok(data)
    }

    /// Applies the patch to `base_file` (the raw base `.xex`) and its loaded form.
    /// Returns the new header bytes and the new image.
    pub fn apply(&self, base_file: &[u8], base: &Xex) -> Result<(Vec<u8>, Vec<u8>, PatchStats)> {
        let d = &self.descriptor;
        let mut stats = PatchStats::default();

        if let Some(e) = base.header.execution_info()? {
            if e.version != d.source_version {
                return Err(Error::Invalid(format!(
                    "patch expects base version {}, base is {}",
                    d.source_version, e.version
                )));
            }
        }

        // 1. Headers.
        let base_header = slice(base_file, 0, base.header.header_size as usize, "base header")?;
        let target_len = d.size_of_target_headers as usize;
        let mut hdr = base_header.to_vec();
        let need = target_len.max((d.headers_target_offset + d.headers_source_size) as usize);
        if hdr.len() < need {
            hdr.resize(need, 0);
        }
        if d.headers_source_offset != 0 {
            let (s, t, n) =
                (d.headers_source_offset as usize, d.headers_target_offset as usize, d.headers_source_size as usize);
            slice(&hdr, s, n, "header copy source")?;
            hdr.copy_within(s..s + n, t);
        }
        if target_len != 0 && target_len < hdr.len() {
            hdr[target_len..].fill(0);
        }
        stats.header_records = apply_patch_list(&d.header_patches, self.window_size, &mut hdr, &mut stats, "header")?;
        if target_len != 0 {
            hdr.truncate(target_len);
        }
        let new_header = Header::parse(&hdr)
            .map_err(|e| Error::Invalid(format!("patched header does not parse: {e}")))?;
        let new_security = SecurityInfo::parse(&hdr, new_header.security_offset)?;
        if new_security.image_size != self.security.image_size {
            return Err(Error::Invalid(format!(
                "patched header image size 0x{:X} != patch image size 0x{:X}",
                new_security.image_size, self.security.image_size
            )));
        }

        // 2. Image. Keys: the target's session key comes from its (patched) security
        // info, decrypted with the retail (or devkit) key. The descriptor's key is the
        // base's session key encrypted with the target session key, which identifies
        // the base. The patch's own image key is encrypted with the target session key
        // too; decrypted, it is the key for the patch payload.
        let dec = |key: &[u8; 16], v: [u8; 16]| {
            let mut v = v;
            aes128_cbc_decrypt(key, &mut v);
            v
        };
        let base_session = match base.key {
            crate::KeyKind::Devkit => dec(&DEVKIT_KEY, base.security.encrypted_key),
            crate::KeyKind::Retail => dec(&RETAIL_KEY, base.security.encrypted_key),
            crate::KeyKind::NotEncrypted => {
                // A decrypted base has lost the key that identifies it: skip that check.
                [0u8; 16]
            }
        };
        let mut payload = None;
        for master in [RETAIL_KEY, DEVKIT_KEY] {
            let target_session = dec(&master, new_security.encrypted_key);
            let base_ok = base.key == crate::KeyKind::NotEncrypted
                || dec(&target_session, d.source_image_key) == base_session;
            if !base_ok {
                continue;
            }
            if let Ok(p) = self.decrypt_payload(&dec(&target_session, self.security.encrypted_key)) {
                payload = Some(p);
                break;
            }
        }
        let payload = payload.ok_or_else(|| {
            Error::Invalid(
                "the patch does not match this base executable (its key check fails), or its payload does not decrypt"
                    .into(),
            )
        })?;
        let mut image = base.image.clone();
        image.truncate(base.security.image_size as usize);
        image.resize(new_security.image_size as usize, 0);
        if d.image_source_offset != 0 {
            let (s, t, n) = (d.image_source_offset as usize, d.image_target_offset as usize, d.image_source_size as usize);
            slice(&image, s, n, "image copy source")?;
            slice(&image, t, n, "image copy target")?;
            image.copy_within(s..s + n, t);
        }

        let (mut size, mut hash) = self.first_block;
        let mut off = 0usize;
        while size != 0 {
            let block = slice(&payload, off, size as usize, "delta block")?;
            if sha1(block) != hash {
                return Err(Error::Invalid(format!("delta block {} at payload 0x{off:X}: SHA-1 mismatch", stats.image_blocks)));
            }
            let next_size = be32(block, 0, "delta block link")?;
            let next_hash: [u8; 20] = slice(block, 4, 20, "delta block link")?.try_into().unwrap();
            stats.image_records += apply_patch_list(&block[24..], self.window_size, &mut image, &mut stats, "image")?;
            stats.image_blocks += 1;
            off += size as usize;
            size = next_size;
            hash = next_hash;
        }
        Ok((hdr, image, stats))
    }
}

/// Applies one patch list to `buf`. Returns the number of records applied.
fn apply_patch_list(list: &[u8], window: u32, buf: &mut [u8], stats: &mut PatchStats, what: &str) -> Result<usize> {
    let mut p = 0usize;
    let mut records = 0;
    while p + 12 <= list.len() {
        let w = "delta patch record";
        let old = be32(list, p, w)? as usize;
        let new = be32(list, p + 4, w)? as usize;
        let len = be16(list, p + 8, w)? as usize;
        let clen = be16(list, p + 10, w)? as usize;
        if old == 0 && new == 0 && len == 0 && clen == 0 {
            break;
        }
        let ctx = || format!("{what} patch record {records} (old 0x{old:X} new 0x{new:X} len 0x{len:X})");
        let dst_ok = new.checked_add(len).is_some_and(|e| e <= buf.len());
        if !dst_ok {
            return Err(Error::Invalid(format!("{}: destination is outside the buffer (0x{:X})", ctx(), buf.len())));
        }
        match clen {
            0 => {
                buf[new..new + len].fill(0);
                stats.zero_records += 1;
                p += 12;
            }
            1 => {
                if old + len > buf.len() {
                    return Err(Error::Invalid(format!("{}: copy source is outside the buffer", ctx())));
                }
                buf.copy_within(old..old + len, new);
                stats.copy_records += 1;
                p += 12;
            }
            _ => {
                let data = slice(list, p + 12, clen, "delta LZX data")?;
                let reference = buf
                    .get(old..old + len)
                    .ok_or_else(|| Error::Invalid(format!("{}: reference is outside the buffer", ctx())))?
                    .to_vec();
                let (out, used) = lzx::decompress(data, len, window, &reference)
                    .map_err(|e| Error::Invalid(format!("{}: {e}", ctx())))?;
                if used > clen {
                    return Err(Error::Invalid(format!("{}: LZX read {used} of {clen} bytes", ctx())));
                }
                stats.lzx_unused_bytes += clen - used;
                buf[new..new + len].copy_from_slice(&out);
                stats.lzx_records += 1;
                stats.lzx_input_bytes += clen;
                stats.lzx_output_bytes += len;
                p += 12 + clen;
            }
        }
        records += 1;
    }
    Ok(records)
}

/// Builds a plain XEX (no encryption, no compression) from header bytes and an image.
/// The file format info is rewritten in place; every other header is kept.
pub fn write_plain_xex(header_bytes: &[u8], image: &[u8]) -> Result<Vec<u8>> {
    let header = Header::parse(header_bytes)?;
    let ff = header
        .get(key::FILE_FORMAT_INFO)
        .ok_or_else(|| Error::Invalid("missing file format info header".into()))?;
    let mut out = header_bytes.to_vec();
    out.resize(header.header_size as usize, 0);
    let at = ff.value as usize;
    let len = ff.data.len();
    if len < 8 || at + len > out.len() {
        return Err(Error::Invalid("file format info is malformed".into()));
    }
    out[at + 4..at + 6].copy_from_slice(&0u16.to_be_bytes()); // encryption: none
    out[at + 6..at + 8].copy_from_slice(&0u16.to_be_bytes()); // compression: none
    out[at + 8..at + len].fill(0);
    out.extend_from_slice(image);
    Ok(out)
}

/// Applies `patch_file` (a `.xexp`) to `base_file` (a `.xex`) and returns a plain `.xex`.
pub fn apply_to_file(base_file: &[u8], patch_file: &[u8]) -> Result<(Vec<u8>, PatchStats)> {
    let base = Xex::parse(base_file)?;
    let patch = DeltaPatch::parse(patch_file)?;
    let (hdr, image, mut stats) = patch.apply(base_file, &base)?;
    let out = write_plain_xex(&hdr, &image)?;
    // The target headers carry the title update's own signed page hashes: a correct
    // result reproduces every one of them.
    stats.pages_verified = Xex::parse(&out)?.verify_pages();
    if stats.pages_verified.0 != stats.pages_verified.1 {
        return Err(Error::Invalid(format!(
            "patched image fails its page hash check ({} of {} links match)",
            stats.pages_verified.0, stats.pages_verified.1
        )));
    }
    Ok((out, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(old: u32, new: u32, len: u16, clen: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&old.to_be_bytes());
        v.extend_from_slice(&new.to_be_bytes());
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(&clen.to_be_bytes());
        v
    }

    #[test]
    fn patch_list_zero_copy_and_lzx() {
        let mut buf = b"0123456789abcdef".to_vec();
        let mut list = record(0, 12, 4, 0); // zero "cdef"
        list.extend(record(0, 8, 4, 1)); // copy "0123" over "89ab"
        // LZX: one uncompressed block "XY" written at offset 2 (reference = "23")
        let mut w = lzx::tests::Writer::new();
        w.put(0, 1);
        w.put(3, 3);
        w.put(2, 24);
        w.flush();
        for _ in 0..3 {
            w.bytes(&1u32.to_le_bytes());
        }
        w.bytes(b"XY");
        let data = w.finish();
        list.extend(record(2, 2, 2, data.len() as u16));
        list.extend(&data);
        list.extend(record(0, 0, 0, 0));
        list.extend(record(0, 0, 4, 0)); // after the terminator: ignored
        let mut stats = PatchStats::default();
        let n = apply_patch_list(&list, 0x8000, &mut buf, &mut stats, "test").unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf, b"01XY45670123\0\0\0\0");
    }
}
