//! FoxPro memo files (FPT, and SCT/FRT/VCT which share the format) plus
//! read-only support for dBase DBT memos.
//!
//! FPT layout: a 512-byte header (next free block and block size, both
//! big-endian), followed by blocks. Each memo starts at a block boundary with
//! an 8-byte header: type (0 = binary/picture, 1 = text) and length, both
//! big-endian.

use crate::error::{FoxProError, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const FPT_HEADER_SIZE: usize = 512;
/// Guard against corrupt length fields causing huge allocations.
const MAX_MEMO_BYTES: u32 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoKind {
    Binary,
    Text,
    Other(u32),
}

impl MemoKind {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => MemoKind::Binary,
            1 => MemoKind::Text,
            v => MemoKind::Other(v),
        }
    }

    fn to_u32(self) -> u32 {
        match self {
            MemoKind::Binary => 0,
            MemoKind::Text => 1,
            MemoKind::Other(v) => v,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Fpt,
    Dbt3,
    Dbt4,
}

pub struct MemoReader {
    path: PathBuf,
    file: File,
    len: u64,
    block_size: u32,
    format: Format,
}

impl MemoReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut header = [0u8; FPT_HEADER_SIZE];
        let n = file.read(&mut header)?;
        if n < 8 {
            return Err(FoxProError::malformed(
                path,
                "memo file header is truncated",
            ));
        }

        let is_dbt = crate::fsutil::has_extension(path, "dbt");
        let (format, block_size) = if is_dbt {
            // dBase IV stores the block size little-endian at offset 20;
            // dBase III always uses 512-byte blocks.
            let bs = u16::from_le_bytes([header[20], header[21]]) as u32;
            if header[16] == 0 || bs == 0 {
                (Format::Dbt3, 512)
            } else {
                (Format::Dbt4, bs)
            }
        } else {
            let bs = u16::from_be_bytes([header[6], header[7]]) as u32;
            (Format::Fpt, if bs == 0 { 64 } else { bs })
        };

        Ok(Self {
            path: path.to_path_buf(),
            file,
            len,
            block_size,
            format,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    fn malformed(&self, message: String) -> FoxProError {
        FoxProError::malformed(&self.path, message)
    }

    /// Read the memo stored at `block`.
    pub fn read(&mut self, block: u32) -> Result<(MemoKind, Vec<u8>)> {
        let offset = block as u64 * self.block_size as u64;
        if offset >= self.len {
            return Err(self.malformed(format!(
                "memo block {block} points past the end of the file"
            )));
        }
        self.file.seek(SeekFrom::Start(offset))?;

        match self.format {
            Format::Fpt => {
                let mut head = [0u8; 8];
                self.file.read_exact(&mut head).map_err(|_| {
                    self.malformed(format!("memo block {block} header is truncated"))
                })?;
                let kind =
                    MemoKind::from_u32(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
                let size = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
                if size > MAX_MEMO_BYTES || offset + 8 + size as u64 > self.len {
                    return Err(self.malformed(format!(
                        "memo block {block} declares {size} bytes, beyond the end of the file"
                    )));
                }
                let mut data = vec![0u8; size as usize];
                self.file.read_exact(&mut data)?;
                Ok((kind, data))
            }
            Format::Dbt4 => {
                let mut head = [0u8; 8];
                self.file.read_exact(&mut head)?;
                let size =
                    u32::from_le_bytes([head[4], head[5], head[6], head[7]]).saturating_sub(8);
                if size > MAX_MEMO_BYTES || offset + 8 + size as u64 > self.len {
                    return Err(self.malformed(format!("memo block {block} is truncated")));
                }
                let mut data = vec![0u8; size as usize];
                self.file.read_exact(&mut data)?;
                Ok((MemoKind::Text, data))
            }
            Format::Dbt3 => {
                // Terminated by 0x1A 0x1A.
                let mut data = Vec::new();
                let mut chunk = [0u8; 512];
                loop {
                    let n = self.file.read(&mut chunk)?;
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&chunk[..n]);
                    if let Some(end) = data.windows(2).position(|w| w == [0x1A, 0x1A]) {
                        data.truncate(end);
                        break;
                    }
                    if data.len() as u32 > MAX_MEMO_BYTES {
                        return Err(self.malformed(format!("memo block {block} is unterminated")));
                    }
                }
                Ok((MemoKind::Text, data))
            }
        }
    }
}

/// Decode a memo pointer stored in a record (4-byte binary for VFP, 10-char
/// ASCII for older formats).
pub fn parse_pointer(raw: &[u8]) -> u32 {
    if raw.len() == 4 {
        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
    } else {
        std::str::from_utf8(raw)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

pub fn format_pointer(block: u32, length: u8) -> Vec<u8> {
    if length == 4 {
        block.to_le_bytes().to_vec()
    } else if block == 0 {
        vec![b' '; length as usize]
    } else {
        format!("{block:>width$}", width = length as usize).into_bytes()
    }
}

/// Builds a fresh FPT file (the memo file is compacted on every save).
pub struct MemoWriter {
    buf: Vec<u8>,
    block_size: usize,
}

impl MemoWriter {
    pub fn new(block_size: u16) -> Self {
        let block_size = block_size.max(1) as usize;
        let first = FPT_HEADER_SIZE.div_ceil(block_size) * block_size;
        Self {
            buf: vec![0u8; first],
            block_size,
        }
    }

    pub fn append(&mut self, kind: MemoKind, data: &[u8]) -> Result<u32> {
        let block = self.buf.len() / self.block_size;
        let block = u32::try_from(block)
            .map_err(|_| FoxProError::Validation("memo file would exceed 4G blocks".into()))?;
        let len = u32::try_from(data.len())
            .map_err(|_| FoxProError::Validation("memo value is too large".into()))?;
        self.buf.extend_from_slice(&kind.to_u32().to_be_bytes());
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(data);
        let padded = self.buf.len().div_ceil(self.block_size) * self.block_size;
        self.buf.resize(padded, 0);
        Ok(block)
    }

    pub fn finish(mut self) -> Vec<u8> {
        let next_free = (self.buf.len() / self.block_size) as u32;
        self.buf[0..4].copy_from_slice(&next_free.to_be_bytes());
        self.buf[6..8].copy_from_slice(&(self.block_size as u16).to_be_bytes());
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_then_read_memos() {
        let mut w = MemoWriter::new(64);
        let a = w.append(MemoKind::Text, b"hello").unwrap();
        let b = w.append(MemoKind::Binary, &[1u8; 100]).unwrap();
        assert_eq!(a, 8);
        assert_eq!(b, 9);
        let bytes = w.finish();

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.fpt");
        std::fs::write(&path, bytes).unwrap();
        let mut r = MemoReader::open(&path).unwrap();
        assert_eq!(r.block_size(), 64);
        assert_eq!(r.read(a).unwrap(), (MemoKind::Text, b"hello".to_vec()));
        assert_eq!(r.read(b).unwrap().1.len(), 100);
        assert!(r.read(1000).is_err());
    }

    #[test]
    fn pointer_formats() {
        assert_eq!(parse_pointer(&format_pointer(42, 4)), 42);
        assert_eq!(parse_pointer(&format_pointer(42, 10)), 42);
        assert_eq!(parse_pointer(b"          "), 0);
    }
}
