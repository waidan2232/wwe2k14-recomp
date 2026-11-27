//! Bounds-checked readers. XEX files are untrusted input, so every read goes through these.

use crate::{Error, Result};

pub(crate) fn slice<'a>(b: &'a [u8], offset: usize, len: usize, what: &'static str) -> Result<&'a [u8]> {
    offset
        .checked_add(len)
        .and_then(|end| b.get(offset..end))
        .ok_or(Error::Truncated { what, offset, len })
}

pub(crate) fn be16(b: &[u8], offset: usize, what: &'static str) -> Result<u16> {
    Ok(u16::from_be_bytes(slice(b, offset, 2, what)?.try_into().unwrap()))
}

pub(crate) fn be32(b: &[u8], offset: usize, what: &'static str) -> Result<u32> {
    Ok(u32::from_be_bytes(slice(b, offset, 4, what)?.try_into().unwrap()))
}

pub(crate) fn le16(b: &[u8], offset: usize, what: &'static str) -> Result<u16> {
    Ok(u16::from_le_bytes(slice(b, offset, 2, what)?.try_into().unwrap()))
}

pub(crate) fn le32(b: &[u8], offset: usize, what: &'static str) -> Result<u32> {
    Ok(u32::from_le_bytes(slice(b, offset, 4, what)?.try_into().unwrap()))
}

/// NUL-terminated single-byte string. Latin-1 decoding keeps every byte value.
pub(crate) fn latin1(b: &[u8]) -> String {
    b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect()
}
