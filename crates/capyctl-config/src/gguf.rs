//! ADR 0029 §9: a bounded GGUF header reader. It reads the magic, the
//! version and the metadata key/value pairs of the GGUF file a llama.cpp
//! launch renders, never tensor data, so sizing a deployment reads at most
//! [`MAX_METADATA_BYTES`] of a file that may hold hundreds of gigabytes.
//!
//! The format is llama.cpp 0.6.0's (`ggml/include/gguf.h`,
//! `ggml/src/gguf.cpp`): little-endian, `GGUF`, a `u32` version (2 and 3 are
//! read the same way; 1 is no longer supported and later ones are unknown),
//! a `u64` tensor count, a `u64` key count, then per key a string name, a
//! `u32` type and the value. A string is a `u64` length and its bytes; an
//! array is a `u32` element type, a `u64` count and the elements (arrays of
//! arrays are invalid). Values sizing does not need are skipped: only
//! numbers, booleans, `general.architecture` and short integer arrays (the
//! per-layer counts) are kept; every key's name is kept, since a key's
//! presence alone can say what a model is (`<arch>.ssm.*`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

/// The file's first four bytes.
pub const MAGIC: &[u8; 4] = b"GGUF";
/// The metadata read at most, after the fixed 24-byte preamble.
pub const MAX_METADATA_BYTES: u64 = 64 << 20;
/// The most keys a header may state.
pub const MAX_KEYS: u64 = 1 << 20;
/// The longest string (a key's name or a value).
pub const MAX_STRING_BYTES: u64 = 1 << 20;
/// The most elements of one array.
pub const MAX_ARRAY_ENTRIES: u64 = 1 << 20;
/// The longest integer array kept (a per-layer count; llama.cpp allows 512
/// layers). Longer ones are skipped.
const MAX_KEPT_ARRAY: u64 = 4096;
/// The one string value sizing reads.
pub const ARCHITECTURE_KEY: &str = "general.architecture";
/// The longest architecture name kept.
const MAX_ARCHITECTURE_BYTES: u64 = 256;

const TYPE_UINT8: u32 = 0;
const TYPE_INT8: u32 = 1;
const TYPE_UINT16: u32 = 2;
const TYPE_INT16: u32 = 3;
const TYPE_UINT32: u32 = 4;
const TYPE_INT32: u32 = 5;
const TYPE_FLOAT32: u32 = 6;
const TYPE_BOOL: u32 = 7;
const TYPE_STRING: u32 = 8;
const TYPE_ARRAY: u32 = 9;
const TYPE_UINT64: u32 = 10;
const TYPE_INT64: u32 = 11;
const TYPE_FLOAT64: u32 = 12;

/// Why a header was not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GgufError {
    #[error("the file cannot be read")]
    Io,
    #[error("the file is not a GGUF file (bad magic)")]
    BadMagic,
    #[error("GGUF version {0} is not read (2 and 3 are)")]
    Version(u32),
    #[error("the GGUF header ends before its metadata does")]
    Truncated,
    #[error("the GGUF header's {0} is past its bound")]
    Bound(&'static str),
    #[error("the GGUF header is malformed: {0}")]
    Malformed(&'static str),
}

/// One kept metadata value.
#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    /// An array of integers, each widened; negative elements are kept as
    /// `None` so a count read from them is refused rather than wrapped.
    Integers(Vec<Option<u64>>),
}

/// What a GGUF header states that sizing reads.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GgufHeader {
    pub version: u32,
    pub tensor_count: u64,
    /// Every key the header names, kept or not.
    pub keys: BTreeSet<String>,
    values: BTreeMap<String, GgufValue>,
}

impl GgufHeader {
    pub fn value(&self, key: &str) -> Option<&GgufValue> {
        self.values.get(key)
    }

    /// `general.architecture`.
    pub fn architecture(&self) -> Option<&str> {
        match self.values.get(ARCHITECTURE_KEY)? {
            GgufValue::String(name) => Some(name),
            _ => None,
        }
    }

    /// A non-negative integer scalar.
    pub fn uint(&self, key: &str) -> Option<u64> {
        match self.values.get(key)? {
            GgufValue::Uint(value) => Some(*value),
            GgufValue::Int(value) => u64::try_from(*value).ok(),
            _ => None,
        }
    }

    /// A per-layer count, as llama.cpp's `get_key_or_arr` reads it: one
    /// scalar for every layer, or an array of exactly `layers` entries.
    /// `None` when absent, negative or of another length.
    pub fn per_layer(&self, key: &str, layers: usize) -> Option<Vec<u64>> {
        match self.values.get(key)? {
            GgufValue::Integers(values) if values.len() == layers => {
                values.iter().copied().collect()
            }
            GgufValue::Integers(_) => None,
            _ => self.uint(key).map(|value| vec![value; layers]),
        }
    }

    /// Whether the header names `key` or any key below `prefix.`.
    pub fn names_under(&self, prefix: &str) -> bool {
        let dotted = format!("{prefix}.");
        self.keys
            .range(dotted.clone()..)
            .next()
            .is_some_and(|key| key.starts_with(&dotted))
    }
}

/// Read the header of the GGUF file at `path`, bounded.
pub fn read_header_file(path: &Path) -> Result<GgufHeader, GgufError> {
    let file = std::fs::File::open(path).map_err(|_| GgufError::Io)?;
    read_header(std::io::BufReader::new(file))
}

/// Read a GGUF header from `source`, bounded; stops after the last key.
pub fn read_header(source: impl Read) -> Result<GgufHeader, GgufError> {
    let mut reader = Bounded {
        inner: source,
        remaining: 24,
    };
    let mut magic = [0u8; 4];
    reader.fill(&mut magic)?;
    if &magic != MAGIC {
        return Err(GgufError::BadMagic);
    }
    let version = reader.u32()?;
    if !(2..=3).contains(&version) {
        return Err(GgufError::Version(version));
    }
    let tensor_count = reader.u64()?;
    let key_count = reader.u64()?;
    if key_count > MAX_KEYS {
        return Err(GgufError::Bound("key count"));
    }
    reader.remaining = MAX_METADATA_BYTES;
    let mut header = GgufHeader {
        version,
        tensor_count,
        ..GgufHeader::default()
    };
    for _ in 0..key_count {
        let key = reader.string(MAX_STRING_BYTES)?;
        if key.is_empty() {
            return Err(GgufError::Malformed("an empty key"));
        }
        if !header.keys.insert(key.clone()) {
            return Err(GgufError::Malformed("a duplicate key"));
        }
        let kind = reader.u32()?;
        let value = if kind == TYPE_ARRAY {
            let element = reader.u32()?;
            let count = reader.u64()?;
            if count > MAX_ARRAY_ENTRIES {
                return Err(GgufError::Bound("array length"));
            }
            reader.array(element, count)?
        } else if kind == TYPE_STRING {
            let keep = key == ARCHITECTURE_KEY;
            let length = reader.u64()?;
            if length > MAX_STRING_BYTES {
                return Err(GgufError::Bound("string length"));
            }
            if keep && length <= MAX_ARCHITECTURE_BYTES {
                let mut bytes = vec![0u8; length as usize];
                reader.fill(&mut bytes)?;
                String::from_utf8(bytes).ok().map(GgufValue::String)
            } else {
                reader.skip(length)?;
                None
            }
        } else {
            Some(reader.scalar(kind)?)
        };
        if let Some(value) = value {
            header.values.insert(key, value);
        }
    }
    Ok(header)
}

/// A reader that refuses to read past `remaining` bytes.
struct Bounded<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Bounded<R> {
    fn take(&mut self, length: u64) -> Result<(), GgufError> {
        self.remaining = self
            .remaining
            .checked_sub(length)
            .ok_or(GgufError::Bound("metadata size"))?;
        Ok(())
    }

    fn fill(&mut self, buffer: &mut [u8]) -> Result<(), GgufError> {
        self.take(buffer.len() as u64)?;
        self.inner.read_exact(buffer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                GgufError::Truncated
            } else {
                GgufError::Io
            }
        })
    }

    fn skip(&mut self, length: u64) -> Result<(), GgufError> {
        self.take(length)?;
        let copied = std::io::copy(&mut (&mut self.inner).take(length), &mut std::io::sink())
            .map_err(|_| GgufError::Io)?;
        if copied == length {
            Ok(())
        } else {
            Err(GgufError::Truncated)
        }
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], GgufError> {
        let mut buffer = [0u8; N];
        self.fill(&mut buffer)?;
        Ok(buffer)
    }

    fn u32(&mut self) -> Result<u32, GgufError> {
        self.bytes().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, GgufError> {
        self.bytes().map(u64::from_le_bytes)
    }

    fn string(&mut self, bound: u64) -> Result<String, GgufError> {
        let length = self.u64()?;
        if length > bound {
            return Err(GgufError::Bound("string length"));
        }
        let mut bytes = vec![0u8; length as usize];
        self.fill(&mut bytes)?;
        String::from_utf8(bytes).map_err(|_| GgufError::Malformed("a key that is not UTF-8"))
    }

    /// One scalar of type `kind`.
    fn scalar(&mut self, kind: u32) -> Result<GgufValue, GgufError> {
        Ok(match kind {
            TYPE_UINT8 => GgufValue::Uint(u64::from(self.bytes::<1>()?[0])),
            TYPE_INT8 => GgufValue::Int(i64::from(i8::from_le_bytes(self.bytes()?))),
            TYPE_UINT16 => GgufValue::Uint(u64::from(u16::from_le_bytes(self.bytes()?))),
            TYPE_INT16 => GgufValue::Int(i64::from(i16::from_le_bytes(self.bytes()?))),
            TYPE_UINT32 => GgufValue::Uint(u64::from(self.u32()?)),
            TYPE_INT32 => GgufValue::Int(i64::from(i32::from_le_bytes(self.bytes()?))),
            TYPE_FLOAT32 => GgufValue::Float(f64::from(f32::from_le_bytes(self.bytes()?))),
            TYPE_BOOL => GgufValue::Bool(self.bytes::<1>()?[0] != 0),
            TYPE_UINT64 => GgufValue::Uint(self.u64()?),
            TYPE_INT64 => GgufValue::Int(i64::from_le_bytes(self.bytes()?)),
            TYPE_FLOAT64 => GgufValue::Float(f64::from_le_bytes(self.bytes()?)),
            _ => return Err(GgufError::Malformed("an unknown value type")),
        })
    }

    /// An array of `count` elements of type `element`: integers kept when
    /// short, everything else skipped.
    fn array(&mut self, element: u32, count: u64) -> Result<Option<GgufValue>, GgufError> {
        let width = match element {
            TYPE_UINT8 | TYPE_INT8 | TYPE_BOOL => 1,
            TYPE_UINT16 | TYPE_INT16 => 2,
            TYPE_UINT32 | TYPE_INT32 | TYPE_FLOAT32 => 4,
            TYPE_UINT64 | TYPE_INT64 | TYPE_FLOAT64 => 8,
            TYPE_STRING => {
                for _ in 0..count {
                    let length = self.u64()?;
                    if length > MAX_STRING_BYTES {
                        return Err(GgufError::Bound("string length"));
                    }
                    self.skip(length)?;
                }
                return Ok(None);
            }
            TYPE_ARRAY => return Err(GgufError::Malformed("an array of arrays")),
            _ => return Err(GgufError::Malformed("an unknown array type")),
        };
        let integer = !matches!(element, TYPE_FLOAT32 | TYPE_FLOAT64 | TYPE_BOOL);
        if !integer || count > MAX_KEPT_ARRAY {
            self.skip(count * width)?;
            return Ok(None);
        }
        let mut values = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let value = match self.scalar(element)? {
                GgufValue::Uint(value) => Some(value),
                GgufValue::Int(value) => u64::try_from(value).ok(),
                _ => None,
            };
            values.push(value);
        }
        Ok(Some(GgufValue::Integers(values)))
    }
}
