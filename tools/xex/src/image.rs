//! Decrypting and decompressing the XEX payload into the in-memory PE image.
//!
//! Matches XenonRecomp's `Xex2LoadImage`. The per-file key is decrypted with the
//! retail key (devkit key as a fallback). The payload is then AES-128-CBC
//! decrypted with that key and a zero IV, and finally decompressed.

use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};

use crate::bytes::slice;
use crate::headers::{Compression, Encryption, FileFormat, Header, SecurityInfo};
use crate::{Error, Result};

pub const RETAIL_KEY: [u8; 16] = [
    0x20, 0xB1, 0x85, 0xA5, 0x9D, 0x28, 0xFD, 0xC3, 0x40, 0x58, 0x3F, 0xBB, 0x08, 0x96, 0xBF, 0x91,
];
pub const DEVKIT_KEY: [u8; 16] = [0; 16];

/// Sanity cap on the decompressed image (the 360 has 512 MiB of RAM in total).
const MAX_IMAGE_SIZE: usize = 512 << 20;

/// Which key decrypted the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    NotEncrypted,
    Retail,
    Devkit,
}

/// AES-128-CBC decryption with a zero IV, in place. A trailing partial block is left untouched.
pub(crate) fn aes128_cbc_decrypt(key: &[u8; 16], data: &mut [u8]) {
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut prev = [0u8; 16];
    for chunk in data.chunks_exact_mut(16) {
        let ciphertext: [u8; 16] = (&*chunk).try_into().unwrap();
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        for (b, p) in chunk.iter_mut().zip(prev) {
            *b ^= p;
        }
        prev = ciphertext;
    }
}

pub(crate) fn build_image(
    file: &[u8],
    header: &Header,
    security: &SecurityInfo,
    format: &FileFormat,
) -> Result<(Vec<u8>, KeyKind)> {
    let payload = file.get(header.header_size as usize..).ok_or(Error::Truncated {
        what: "image payload",
        offset: header.header_size as usize,
        len: 0,
    })?;
    match format.encryption {
        Encryption::None => Ok((decompress(payload, security, &format.compression)?, KeyKind::NotEncrypted)),
        Encryption::Normal => {
            for (kind, key) in [(KeyKind::Retail, RETAIL_KEY), (KeyKind::Devkit, DEVKIT_KEY)] {
                let mut session_key = security.encrypted_key;
                aes128_cbc_decrypt(&key, &mut session_key);
                let mut data = payload.to_vec();
                aes128_cbc_decrypt(&session_key, &mut data);
                let image = decompress(&data, security, &format.compression)?;
                if image.starts_with(b"MZ") {
                    return Ok((image, kind));
                }
            }
            Err(Error::DecryptionFailed)
        }
    }
}

fn decompress(payload: &[u8], security: &SecurityInfo, compression: &Compression) -> Result<Vec<u8>> {
    let image_size = security.image_size as usize;
    if image_size > MAX_IMAGE_SIZE {
        return Err(Error::Invalid(format!("image size 0x{image_size:X} is implausibly large")));
    }
    match compression {
        Compression::None => {
            let mut image = vec![0u8; image_size];
            let n = payload.len().min(image_size);
            image[..n].copy_from_slice(&payload[..n]);
            Ok(image)
        }
        Compression::Basic { blocks } => {
            let total = blocks
                .iter()
                .try_fold(0usize, |acc, b| acc.checked_add(b.data_size as usize)?.checked_add(b.zero_size as usize))
                .filter(|&t| t <= MAX_IMAGE_SIZE)
                .ok_or_else(|| Error::Invalid("basic-compression blocks expand to an implausible size".into()))?;
            let mut image = vec![0u8; total.max(image_size)];
            let (mut src, mut dst) = (0usize, 0usize);
            for b in blocks {
                let n = b.data_size as usize;
                image[dst..dst + n].copy_from_slice(slice(payload, src, n, "basic-compression block data")?);
                src += n;
                dst += n + b.zero_size as usize;
            }
            Ok(image)
        }
        Compression::Lzx { window_size, first_block_size, first_block_hash } => {
            // A chain of blocks: the first is described by the file format info, each
            // block starts with the next one's size and SHA-1, then holds u16-length
            // chunks. The chunks concatenate into one LZX stream.
            // (No sample on hand: WWE 2K14's default.xex uses basic compression.)
            let mut stream = Vec::new();
            let (mut size, mut hash, mut off) = (*first_block_size, *first_block_hash, 0usize);
            while size != 0 {
                let block = slice(payload, off, size as usize, "LZX block")?;
                if crate::sha1::sha1(block) != hash {
                    return Err(Error::Invalid(format!("LZX block at payload 0x{off:X}: SHA-1 mismatch")));
                }
                let mut p = 24;
                while p + 2 <= block.len() {
                    let n = u16::from_be_bytes([block[p], block[p + 1]]) as usize;
                    p += 2;
                    if n == 0 {
                        break;
                    }
                    stream.extend_from_slice(slice(block, p, n, "LZX chunk")?);
                    p += n;
                }
                off += size as usize;
                size = crate::bytes::be32(block, 0, "LZX block link")?;
                hash = slice(block, 4, 20, "LZX block link")?.try_into().unwrap();
            }
            Ok(crate::lzx::decompress(&stream, image_size, *window_size, &[])?.0)
        }
        Compression::Delta => Err(Error::Unsupported(
            "a delta-compressed XEX is a title-update patch and can't be loaded on its own".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headers::BasicBlock;
    use aes::cipher::BlockEncrypt;

    #[test]
    fn aes_fips197_vector() {
        // FIPS-197 appendix C.1. With a zero IV, single-block CBC decryption is plain AES.
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let mut block = [
            0x69, 0xC4, 0xE0, 0xD8, 0x6A, 0x7B, 0x04, 0x30, 0xD8, 0xCD, 0xB7, 0x80, 0x70, 0xB4, 0xC5, 0x5A,
        ];
        aes128_cbc_decrypt(&key, &mut block);
        let expected: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
        assert_eq!(block, expected);
    }

    #[test]
    fn cbc_chains_blocks() {
        let key = [7u8; 16];
        let plain: Vec<u8> = (0..48u8).collect();
        let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
        let mut data = plain.clone();
        let mut prev = [0u8; 16];
        for chunk in data.chunks_exact_mut(16) {
            for (b, p) in chunk.iter_mut().zip(prev) {
                *b ^= p;
            }
            cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
            prev = (&*chunk).try_into().unwrap();
        }
        aes128_cbc_decrypt(&key, &mut data);
        assert_eq!(data, plain);
    }

    fn security(image_size: u32) -> SecurityInfo {
        SecurityInfo {
            header_size: 0,
            image_size,
            image_flags: 0,
            load_address: 0x8200_0000,
            encrypted_key: [0; 16],
            image_digest: [0; 20],
            region: 0,
            allowed_media_types: 0,
            pages: Vec::new(),
        }
    }

    #[test]
    fn basic_decompression_inserts_zero_runs() {
        let blocks = vec![
            BasicBlock { data_size: 3, zero_size: 2 },
            BasicBlock { data_size: 2, zero_size: 1 },
        ];
        let image = decompress(&[1, 2, 3, 4, 5], &security(8), &Compression::Basic { blocks }).unwrap();
        assert_eq!(image, [1, 2, 3, 0, 0, 4, 5, 0]);
    }

    #[test]
    fn basic_decompression_rejects_short_payload() {
        let blocks = vec![BasicBlock { data_size: 10, zero_size: 0 }];
        assert!(matches!(
            decompress(&[1, 2, 3], &security(10), &Compression::Basic { blocks }),
            Err(Error::Truncated { .. })
        ));
    }
}
