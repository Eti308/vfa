use std::fmt;

/// All errors produced by the codec. Decoding of untrusted data never panics;
/// it returns one of these instead.
#[derive(Debug)]
pub enum Error {
    /// The bitstream ended before the expected data was read.
    UnexpectedEof,
    /// Structurally invalid data (bad field value, impossible sample, ...).
    Corrupt(&'static str),
    /// A CRC did not match.
    ChecksumMismatch {
        what: &'static str,
        expected: u32,
        actual: u32,
    },
    /// Valid but unsupported input (e.g. a 24-bit integer WAV).
    Unsupported(String),
    /// Invalid argument from the caller.
    InvalidInput(String),
    Io(std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of data"),
            Error::Corrupt(s) => write!(f, "corrupt data: {s}"),
            Error::ChecksumMismatch { what, expected, actual } => {
                write!(f, "checksum mismatch in {what}: expected {expected:08x}, got {actual:08x}")
            }
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::InvalidInput(s) => write!(f, "invalid input: {s}"),
            Error::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
