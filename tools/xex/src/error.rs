use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The file doesn't start with `XEX2`.
    BadMagic([u8; 4]),
    /// A structure extends past the end of the data it lives in.
    Truncated { what: &'static str, offset: usize, len: usize },
    /// Structurally invalid data.
    Invalid(String),
    /// A valid XEX feature this loader doesn't handle yet.
    Unsupported(String),
    /// Neither the retail nor the devkit key produced a PE image.
    DecryptionFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadMagic(m) => write!(f, "not an XEX2 file (magic {m:02X?})"),
            Error::Truncated { what, offset, len } => {
                write!(f, "truncated data: {what} needs {len} bytes at offset 0x{offset:X}")
            }
            Error::Invalid(s) => write!(f, "invalid XEX: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported XEX feature: {s}"),
            Error::DecryptionFailed => {
                f.write_str("decryption failed: neither the retail nor the devkit key produced a PE image")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
