//! Read tag definitions from a Visual FoxPro compound index (.cdx).
//!
//! A CDX file starts with a "tag directory": a compact B-tree whose keys are
//! tag names and whose record numbers are the file offsets of each tag's own
//! header. Only the directory and tag headers are read; index keys are not.

use crate::error::{FoxProError, Result};
use serde::Serialize;
use std::fs;
use std::path::Path;

const NODE_SIZE: usize = 512;
const HEADER_SIZE: usize = 1024;
const MAX_NODES_VISITED: usize = 10_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdxTag {
    pub name: String,
    pub expression: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    pub unique: bool,
    pub descending: bool,
    pub key_length: u16,
}

struct TagHeader {
    root: u32,
    key_length: u16,
    options: u8,
    descending: bool,
    expression: String,
    filter: Option<String>,
}

fn u16_le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn u32_le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn u32_be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn read_tag_header(data: &[u8], offset: usize) -> Option<TagHeader> {
    let h = data.get(offset..offset + HEADER_SIZE)?;
    let options = h[14];
    let key_pool_len = u16_le(h, 510)? as usize;
    let pool = &h[512..];
    let expression = cstr(&pool[..key_pool_len.min(pool.len())]);
    let filter = if options & 0x08 != 0 {
        let after = pool.iter().position(|&b| b == 0).map(|p| p + 1)?;
        let f = cstr(pool.get(after..)?);
        (!f.is_empty()).then_some(f)
    } else {
        None
    };
    Some(TagHeader {
        root: u32_le(h, 0)?,
        key_length: u16_le(h, 12)?,
        options,
        descending: u16_le(h, 502)? != 0,
        expression,
        filter,
    })
}

/// Collect `(key, record_number)` pairs from every leaf of a compact index.
fn leaf_entries(data: &[u8], root: u32, key_len: usize) -> Option<Vec<(Vec<u8>, u32)>> {
    // Walk down the leftmost path to the first leaf.
    let mut node_off = root as usize;
    let mut visited = 0;
    loop {
        visited += 1;
        if visited > MAX_NODES_VISITED {
            return None;
        }
        let node = data.get(node_off..node_off + NODE_SIZE)?;
        let attrs = u16_le(node, 0)?;
        if attrs & 0x02 != 0 {
            break;
        }
        // Interior node: key + record number (BE) + child pointer (BE).
        let child = u32_be(node, 12 + key_len + 4)?;
        node_off = child as usize;
    }

    let mut out = Vec::new();
    loop {
        visited += 1;
        if visited > MAX_NODES_VISITED {
            return None;
        }
        let node = data.get(node_off..node_off + NODE_SIZE)?;
        let count = u16_le(node, 2)? as usize;
        let right = u32_le(node, 8)?;
        let rec_mask = u32_le(node, 14)?;
        let dup_mask = node[18] as u32;
        let trail_mask = node[19] as u32;
        let rec_bits = node[20] as u32;
        let dup_bits = node[21] as u32;
        let entry_bytes = node[23] as usize;
        if entry_bytes == 0 || entry_bytes > 8 || 24 + count * entry_bytes > NODE_SIZE {
            return None;
        }

        let mut key_pos = NODE_SIZE;
        let mut prev: Vec<u8> = Vec::new();
        for i in 0..count {
            let start = 24 + i * entry_bytes;
            let mut raw = [0u8; 8];
            raw[..entry_bytes].copy_from_slice(&node[start..start + entry_bytes]);
            let info = u64::from_le_bytes(raw);
            let recno = (info as u32) & rec_mask;
            let dup = ((info >> rec_bits) as u32 & dup_mask) as usize;
            let trail = ((info >> (rec_bits + dup_bits)) as u32 & trail_mask) as usize;
            let new_len = key_len.checked_sub(dup + trail)?;
            key_pos = key_pos.checked_sub(new_len)?;
            if key_pos < 24 + count * entry_bytes {
                return None;
            }
            let mut key = prev.get(..dup)?.to_vec();
            key.extend_from_slice(&node[key_pos..key_pos + new_len]);
            key.resize(key_len, b' ');
            prev = key.clone();
            out.push((key, recno));
        }

        if right == u32::MAX || right == 0 {
            break;
        }
        node_off = right as usize;
    }
    Some(out)
}

/// Read tag definitions of a CDX file.
pub fn read_tags(path: &Path) -> Result<Vec<CdxTag>> {
    let data = fs::read(path)?;
    let malformed = || FoxProError::malformed(path, "unrecognized compound index structure");

    let directory = read_tag_header(&data, 0).ok_or_else(malformed)?;
    if directory.key_length == 0 || directory.key_length > 240 {
        return Err(malformed());
    }
    let entries =
        leaf_entries(&data, directory.root, directory.key_length as usize).ok_or_else(malformed)?;

    let mut tags = Vec::with_capacity(entries.len());
    for (key, offset) in entries {
        let name = cstr(&key);
        let header = read_tag_header(&data, offset as usize).ok_or_else(malformed)?;
        tags.push(CdxTag {
            name,
            expression: header.expression,
            filter: header.filter,
            unique: header.options & 0x01 != 0,
            descending: header.descending,
            key_length: header.key_length,
        });
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a minimal CDX with one tag, following the VFP layout.
    fn build_cdx(tag: &str, expr: &str) -> Vec<u8> {
        let mut data = vec![0u8; 1024 + 512 + 1024];
        // Directory header: root node at 1024, key length 10, compound+compact.
        data[0..4].copy_from_slice(&1024u32.to_le_bytes());
        data[12..14].copy_from_slice(&10u16.to_le_bytes());
        data[14] = 0xE0;
        // Directory leaf node at 1024.
        let node = 1024;
        data[node..node + 2].copy_from_slice(&3u16.to_le_bytes());
        data[node + 2..node + 4].copy_from_slice(&1u16.to_le_bytes());
        data[node + 4..node + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        data[node + 8..node + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        data[node + 14..node + 18].copy_from_slice(&0xFFFFu32.to_le_bytes());
        data[node + 18] = 0x0F;
        data[node + 19] = 0x0F;
        data[node + 20] = 16;
        data[node + 21] = 4;
        data[node + 22] = 4;
        data[node + 23] = 3;
        let trail = 10 - tag.len();
        let info: u32 = 1536 | ((trail as u32) << 20);
        // Record number field holds the tag header offset (1536); widen mask.
        data[node + 14..node + 18].copy_from_slice(&0xFFFFu32.to_le_bytes());
        data[node + 24..node + 27].copy_from_slice(&info.to_le_bytes()[..3]);
        let key_start = node + 512 - tag.len();
        data[key_start..node + 512].copy_from_slice(tag.as_bytes());
        // Tag header at 1536.
        let t = 1536;
        data[t + 12..t + 14].copy_from_slice(&10u16.to_le_bytes());
        data[t + 14] = 0x60;
        data[t + 510..t + 512].copy_from_slice(&((expr.len() + 1) as u16).to_le_bytes());
        data[t + 512..t + 512 + expr.len()].copy_from_slice(expr.as_bytes());
        data
    }

    #[test]
    fn reads_tag_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.cdx");
        fs::write(&path, build_cdx("CUSTID", "UPPER(cust_id)")).unwrap();
        let tags = read_tags(&path).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "CUSTID");
        assert_eq!(tags[0].expression, "UPPER(cust_id)");
    }

    #[test]
    fn garbage_is_reported_not_panicking() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.cdx");
        fs::write(&path, vec![0xFFu8; 700]).unwrap();
        assert!(read_tags(&path).is_err());
    }
}
