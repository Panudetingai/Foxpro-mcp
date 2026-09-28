//! Visual FoxPro table access (DBF + FPT/DBT memo files).
//!
//! Two access modes are provided:
//! * [`TableReader`] streams records from arbitrarily large tables (used by
//!   the database tools).
//! * [`Table`] loads a whole table, including memo contents, into memory so it
//!   can be edited and written back (used for SCX/FRX files, which are small
//!   VFP tables).
//!
//! Supported field types: C, V, N, F, I, B, Y, D, T, L, M, G, W, Q, 0 (_NullFlags).

pub mod cdx;
pub mod expr;
pub mod memo;
pub mod value;

use crate::encoding;
use crate::error::{FoxProError, Result};
use chrono::{Datelike, Local};
use encoding_rs::Encoding;
use memo::{MemoKind, MemoReader, MemoWriter};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};

pub use value::Val;

const HEADER_SIZE: usize = 32;
const FIELD_DESCRIPTOR_SIZE: usize = 32;
const VFP_BACKLINK_SIZE: usize = 263;
/// Refuse to load tables larger than this fully into memory.
const MAX_IN_MEMORY_BYTES: u64 = 64 * 1024 * 1024;

pub const FLAG_HAS_CDX: u8 = 0x01;
pub const FLAG_HAS_MEMO: u8 = 0x02;
pub const FLAG_IS_DBC: u8 = 0x04;

pub const FIELD_SYSTEM: u8 = 0x01;
pub const FIELD_NULLABLE: u8 = 0x02;
pub const FIELD_BINARY: u8 = 0x04;
pub const FIELD_AUTOINC: u8 = 0x0C;

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub kind: char,
    /// Offset inside the record, counting the leading deletion flag byte.
    pub offset: usize,
    pub length: u8,
    pub decimals: u8,
    pub flags: u8,
    pub autoinc_next: u32,
    pub autoinc_step: u8,
}

impl FieldDef {
    pub fn new(name: &str, kind: char, length: u8, decimals: u8) -> Self {
        Self {
            name: name.to_ascii_uppercase(),
            kind,
            offset: 0,
            length,
            decimals,
            flags: 0,
            autoinc_next: 0,
            autoinc_step: 0,
        }
    }

    pub fn with_flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    pub fn is_memo(&self) -> bool {
        matches!(self.kind, 'M' | 'G' | 'W') && matches!(self.length, 4 | 10)
    }

    pub fn is_nullable(&self) -> bool {
        self.flags & FIELD_NULLABLE != 0
    }

    pub fn is_binary(&self) -> bool {
        self.flags & FIELD_BINARY != 0 || matches!(self.kind, 'G' | 'W' | 'Q')
    }

    pub fn is_system(&self) -> bool {
        self.flags & FIELD_SYSTEM != 0 || self.kind == '0'
    }

    pub fn type_name(&self) -> &'static str {
        match self.kind {
            'C' => "Character",
            'V' => "Varchar",
            'N' => "Numeric",
            'F' => "Float",
            'I' => "Integer",
            'B' => "Double",
            'Y' => "Currency",
            'D' => "Date",
            'T' => "DateTime",
            'L' => "Logical",
            'M' => "Memo",
            'G' => "General",
            'W' => "Blob",
            'Q' => "Varbinary",
            '0' => "NullFlags",
            _ => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub version: u8,
    pub last_update: (u16, u8, u8),
    pub record_count: u32,
    pub header_len: u16,
    pub record_len: u16,
    pub flags: u8,
    pub codepage: u8,
}

impl Header {
    pub fn version_name(&self) -> &'static str {
        match self.version {
            0x02 => "FoxBASE",
            0x03 => "FoxBASE+/dBase III PLUS (no memo)",
            0x30 => "Visual FoxPro",
            0x31 => "Visual FoxPro (autoincrement)",
            0x32 => "Visual FoxPro (varchar/varbinary)",
            0x43 => "dBase IV SQL table (no memo)",
            0x83 => "FoxBASE+/dBase III PLUS (with memo)",
            0x8B => "dBase IV (with memo)",
            0xF5 => "FoxPro 2.x (with memo)",
            0xFB => "FoxBASE",
            _ => "Unknown",
        }
    }

    pub fn is_vfp(&self) -> bool {
        matches!(self.version, 0x30..=0x32)
    }
}

/// Parsed table structure (header + field descriptors).
#[derive(Debug, Clone)]
pub struct Structure {
    pub header: Header,
    pub fields: Vec<FieldDef>,
    pub backlink: Vec<u8>,
    /// Records actually present in the file (may be less than the header
    /// claims for truncated files).
    pub available_records: u32,
    pub warnings: Vec<String>,
    null_bits: Vec<Option<usize>>,
    varlen_bits: Vec<Option<usize>>,
    nullflags_field: Option<usize>,
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Structure {
    fn parse(path: &Path, head: &[u8], file_len: u64) -> Result<Self> {
        if head.len() < HEADER_SIZE + 1 {
            return Err(FoxProError::malformed(
                path,
                "file too small for a DBF header",
            ));
        }
        let header = Header {
            version: head[0],
            last_update: (1900 + head[1] as u16, head[2], head[3]),
            record_count: le_u32(&head[4..8]),
            header_len: le_u16(&head[8..10]),
            record_len: le_u16(&head[10..12]),
            flags: head[28],
            codepage: head[29],
        };
        let header_len = header.header_len as usize;
        if header_len < HEADER_SIZE + 1 || header_len > head.len() {
            return Err(FoxProError::malformed(
                path,
                format!("invalid header length {header_len}"),
            ));
        }
        if header.record_len == 0 {
            return Err(FoxProError::malformed(path, "record length is zero"));
        }

        let mut fields = Vec::new();
        let mut pos = HEADER_SIZE;
        let mut offset = 1usize;
        while pos + FIELD_DESCRIPTOR_SIZE <= header_len && head[pos] != 0x0D {
            let d = &head[pos..pos + FIELD_DESCRIPTOR_SIZE];
            let name_end = d[..11].iter().position(|&b| b == 0).unwrap_or(11);
            let name = String::from_utf8_lossy(&d[..name_end])
                .trim()
                .to_ascii_uppercase();
            let field = FieldDef {
                name,
                kind: d[11] as char,
                offset,
                length: d[16],
                decimals: d[17],
                flags: d[18],
                autoinc_next: le_u32(&d[19..23]),
                autoinc_step: d[23],
            };
            offset += field.length as usize;
            fields.push(field);
            pos += FIELD_DESCRIPTOR_SIZE;
        }
        if fields.is_empty() {
            return Err(FoxProError::malformed(path, "table has no fields"));
        }
        if offset != header.record_len as usize {
            return Err(FoxProError::malformed(
                path,
                format!(
                    "field lengths add up to {offset} bytes but the header declares {}",
                    header.record_len
                ),
            ));
        }

        let backlink = if header.is_vfp() {
            let start = (pos + 1).min(header_len);
            head[start..header_len].to_vec()
        } else {
            Vec::new()
        };

        let mut warnings = Vec::new();
        let data_bytes = file_len.saturating_sub(header_len as u64);
        let fit = (data_bytes / header.record_len as u64).min(u32::MAX as u64) as u32;
        let available_records = if fit < header.record_count {
            warnings.push(format!(
                "header declares {} records but the file only holds {fit}; the table may be truncated",
                header.record_count
            ));
            fit
        } else {
            header.record_count
        };

        // _NullFlags bit allocation: nullable fields first, then the
        // "not full" bit of variable-length fields, in field order.
        let mut null_bits = vec![None; fields.len()];
        let mut varlen_bits = vec![None; fields.len()];
        let nullflags_field = fields.iter().position(|f| f.kind == '0');
        if nullflags_field.is_some() {
            let mut bit = 0usize;
            for (i, f) in fields.iter().enumerate() {
                if f.kind == '0' {
                    continue;
                }
                if f.is_nullable() {
                    null_bits[i] = Some(bit);
                    bit += 1;
                }
                if matches!(f.kind, 'V' | 'Q') {
                    varlen_bits[i] = Some(bit);
                    bit += 1;
                }
            }
        }

        Ok(Self {
            header,
            fields,
            backlink,
            available_records,
            warnings,
            null_bits,
            varlen_bits,
            nullflags_field,
        })
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn has_memo_fields(&self) -> bool {
        self.fields.iter().any(|f| f.is_memo())
    }

    fn flag_bit(&self, record: &[u8], bit: Option<usize>) -> bool {
        let (Some(bit), Some(nf)) = (bit, self.nullflags_field) else {
            return false;
        };
        let f = &self.fields[nf];
        let byte = bit / 8;
        if byte >= f.length as usize {
            return false;
        }
        record
            .get(f.offset + byte)
            .is_some_and(|b| b & (1 << (bit % 8)) != 0)
    }

    pub fn is_null(&self, record: &[u8], field: usize) -> bool {
        self.flag_bit(record, self.null_bits[field])
    }

    /// For V/Q fields: true when the stored value is shorter than the field and
    /// its real length is kept in the last byte.
    fn is_short_varlen(&self, record: &[u8], field: usize) -> bool {
        self.flag_bit(record, self.varlen_bits[field])
    }
}

/// Locate the memo file of a table (`.fpt`, or `.dbt` for dBase tables).
pub fn memo_path_for(path: &Path) -> Option<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let memo_ext = match ext.as_str() {
        "scx" => "sct",
        "frx" => "frt",
        "vcx" => "vct",
        "lbx" => "lbt",
        "mnx" => "mnt",
        "pjx" => "pjt",
        "dbc" => "dct",
        _ => "fpt",
    };
    let fpt = crate::fsutil::companion(path, memo_ext);
    if fpt.exists() {
        return Some(fpt);
    }
    let dbt = crate::fsutil::companion(path, "dbt");
    if dbt.exists() {
        return Some(dbt);
    }
    None
}

fn read_structure(path: &Path, file: &mut File) -> Result<Structure> {
    let file_len = file.metadata()?.len();
    let mut first = [0u8; HEADER_SIZE];
    file.read_exact(&mut first)
        .map_err(|_| FoxProError::malformed(path, "file too small for a DBF header"))?;
    let header_len = le_u16(&first[8..10]) as usize;
    if header_len < HEADER_SIZE + 1 || header_len as u64 > file_len {
        return Err(FoxProError::malformed(
            path,
            format!("invalid header length {header_len}"),
        ));
    }
    let mut head = vec![0u8; header_len];
    head[..HEADER_SIZE].copy_from_slice(&first);
    file.read_exact(&mut head[HEADER_SIZE..])?;
    Structure::parse(path, &head, file_len)
}

fn resolve_encoding(structure: &Structure, preferred: Option<&str>) -> Result<&'static Encoding> {
    if let Some(label) = preferred {
        return encoding::lookup(label);
    }
    Ok(
        encoding::from_codepage_mark(structure.header.codepage)
            .unwrap_or(encoding_rs::WINDOWS_1252),
    )
}

/// Streaming reader for DBF tables of any size.
pub struct TableReader {
    path: PathBuf,
    file: BufReader<File>,
    position: u64,
    pub structure: Structure,
    pub encoding: &'static Encoding,
    memo: Option<MemoReader>,
    memo_error: Option<String>,
}

impl TableReader {
    pub fn open(path: &Path, encoding_override: Option<&str>) -> Result<Self> {
        let mut file = File::open(path)?;
        let structure = read_structure(path, &mut file)?;
        let encoding = resolve_encoding(&structure, encoding_override)?;

        let mut memo_error = None;
        let memo = if structure.has_memo_fields() {
            match memo_path_for(path) {
                Some(p) => match MemoReader::open(&p) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        memo_error = Some(e.to_string());
                        None
                    }
                },
                None => {
                    memo_error = Some("memo file (.fpt/.dbt) is missing".to_string());
                    None
                }
            }
        } else {
            None
        };

        let position = file.stream_position()?;
        Ok(Self {
            path: path.to_path_buf(),
            file: BufReader::with_capacity(64 * 1024, file),
            position,
            structure,
            encoding,
            memo,
            memo_error,
        })
    }

    pub fn memo_path(&self) -> Option<&Path> {
        self.memo.as_ref().map(|m| m.path())
    }

    pub fn memo_error(&self) -> Option<&str> {
        self.memo_error.as_deref()
    }

    pub fn record_count(&self) -> u32 {
        self.structure.available_records
    }

    /// Read the raw bytes of record `recno` (1-based).
    pub fn read_raw(&mut self, recno: u32, buf: &mut Vec<u8>) -> Result<()> {
        if recno == 0 || recno > self.record_count() {
            return Err(FoxProError::InvalidArgument(format!(
                "record {recno} is out of range 1..={}",
                self.record_count()
            )));
        }
        let len = self.structure.header.record_len as u64;
        let target = self.structure.header.header_len as u64 + (recno as u64 - 1) * len;
        if target != self.position {
            // seek_relative keeps the buffer for short forward jumps.
            let delta = target as i64 - self.position as i64;
            self.file.seek_relative(delta)?;
        }
        buf.resize(len as usize, 0);
        self.file.read_exact(buf)?;
        self.position = target + len;
        Ok(())
    }

    pub fn is_deleted(record: &[u8]) -> bool {
        record.first() == Some(&b'*')
    }

    /// Decode field `index` of a raw record.
    pub fn value(&mut self, record: &[u8], index: usize) -> Result<Val> {
        let field = &self.structure.fields[index];
        if self.structure.is_null(record, index) {
            return Ok(Val::Null);
        }
        let raw = &record[field.offset..field.offset + field.length as usize];
        if field.is_memo() {
            let block = memo::parse_pointer(raw);
            if block == 0 {
                return Ok(if field.is_binary() {
                    Val::Binary(Vec::new())
                } else {
                    Val::Str(String::new())
                });
            }
            let Some(memo) = self.memo.as_mut() else {
                return Err(FoxProError::malformed(
                    &self.path,
                    self.memo_error
                        .clone()
                        .unwrap_or_else(|| "memo file unavailable".into()),
                ));
            };
            let (_, data) = memo.read(block)?;
            return Ok(if field.is_binary() {
                Val::Binary(data)
            } else {
                let (text, _) = self.encoding.decode_without_bom_handling(&data);
                Val::Str(text.into_owned())
            });
        }
        let short = self.structure.is_short_varlen(record, index);
        value::decode_fixed(field, raw, self.encoding, short)
    }
}

// ---------------------------------------------------------------------------
// In-memory tables (SCX/FRX editing)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Fixed(Vec<u8>),
    Memo { kind: MemoKind, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub deleted: bool,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub header: Header,
    pub fields: Vec<FieldDef>,
    pub backlink: Vec<u8>,
    pub rows: Vec<Row>,
    pub memo_block_size: u16,
    pub encoding: &'static Encoding,
}

impl Table {
    /// Create an empty VFP table with a memo file.
    pub fn new_vfp(fields: Vec<FieldDef>, enc: &'static Encoding, memo_block_size: u16) -> Self {
        let mut fields = fields;
        let mut offset = 1usize;
        for f in &mut fields {
            f.offset = offset;
            offset += f.length as usize;
        }
        let has_memo = fields.iter().any(|f| f.is_memo());
        Self {
            header: Header {
                version: 0x30,
                last_update: today(),
                record_count: 0,
                header_len: (HEADER_SIZE
                    + fields.len() * FIELD_DESCRIPTOR_SIZE
                    + 1
                    + VFP_BACKLINK_SIZE) as u16,
                record_len: offset as u16,
                flags: if has_memo { FLAG_HAS_MEMO } else { 0 },
                codepage: encoding::to_codepage_mark(enc),
            },
            fields,
            backlink: vec![0; VFP_BACKLINK_SIZE],
            rows: Vec::new(),
            memo_block_size,
            encoding: enc,
        }
    }

    /// Load a whole table (and its memo file) into memory.
    pub fn load(path: &Path, encoding_override: Option<&str>) -> Result<Self> {
        let size = fs::metadata(path)?.len();
        if size > MAX_IN_MEMORY_BYTES {
            return Err(FoxProError::Unsupported(format!(
                "{} is too large to edit in memory ({size} bytes)",
                path.display()
            )));
        }
        let bytes = fs::read(path)?;
        let structure = Structure::parse(path, &bytes, bytes.len() as u64)?;
        if let Some(w) = structure.warnings.first() {
            return Err(FoxProError::malformed(path, w.clone()));
        }
        let enc = resolve_encoding(&structure, encoding_override)?;

        let mut memo = None;
        let mut block_size = 64u16;
        if structure.has_memo_fields() {
            let memo_path = memo_path_for(path).ok_or_else(|| {
                FoxProError::malformed(path, "memo file is missing (expected .sct/.frt/.fpt)")
            })?;
            let reader = MemoReader::open(&memo_path)?;
            block_size = reader.block_size().clamp(1, u16::MAX as u32) as u16;
            memo = Some(reader);
        }

        let header_len = structure.header.header_len as usize;
        let record_len = structure.header.record_len as usize;
        let mut rows = Vec::with_capacity(structure.available_records as usize);
        for i in 0..structure.available_records as usize {
            let start = header_len + i * record_len;
            let raw = &bytes[start..start + record_len];
            let mut cells = Vec::with_capacity(structure.fields.len());
            for field in &structure.fields {
                let slice = &raw[field.offset..field.offset + field.length as usize];
                if field.is_memo() {
                    let block = memo::parse_pointer(slice);
                    let (kind, data) = match (block, memo.as_mut()) {
                        (0, _) => (MemoKind::Text, Vec::new()),
                        (b, Some(m)) => m.read(b)?,
                        (_, None) => {
                            return Err(FoxProError::malformed(path, "memo file unavailable"));
                        }
                    };
                    cells.push(Cell::Memo { kind, data });
                } else {
                    cells.push(Cell::Fixed(slice.to_vec()));
                }
            }
            rows.push(Row {
                deleted: raw[0] == b'*',
                cells,
            });
        }

        Ok(Self {
            header: structure.header,
            fields: structure.fields,
            backlink: structure.backlink,
            rows,
            memo_block_size: block_size,
            encoding: enc,
        })
    }

    pub fn field_index(&self, name: &str) -> Result<usize> {
        self.fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| FoxProError::Validation(format!("table has no field {name}")))
    }

    /// A row with every field blank (spaces / zero pointers), not deleted.
    pub fn blank_row(&self) -> Row {
        let cells = self
            .fields
            .iter()
            .map(|f| {
                if f.is_memo() {
                    Cell::Memo {
                        kind: MemoKind::Text,
                        data: Vec::new(),
                    }
                } else {
                    Cell::Fixed(value::blank(f))
                }
            })
            .collect();
        Row {
            deleted: false,
            cells,
        }
    }

    // -- typed accessors ------------------------------------------------

    pub fn get_text(&self, row: &Row, name: &str) -> String {
        let Ok(i) = self.field_index(name) else {
            return String::new();
        };
        match &row.cells[i] {
            Cell::Fixed(b) => {
                let (t, _) = self.encoding.decode_without_bom_handling(b);
                t.trim_end().to_string()
            }
            Cell::Memo { data, .. } => {
                let (t, _) = self.encoding.decode_without_bom_handling(data);
                t.into_owned()
            }
        }
    }

    pub fn get_num(&self, row: &Row, name: &str) -> Option<f64> {
        let i = self.field_index(name).ok()?;
        match &row.cells[i] {
            Cell::Fixed(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
            Cell::Memo { .. } => None,
        }
    }

    pub fn get_bool(&self, row: &Row, name: &str) -> bool {
        self.field_index(name)
            .ok()
            .and_then(|i| match &row.cells[i] {
                Cell::Fixed(b) => b.first().copied(),
                Cell::Memo { .. } => None,
            })
            .is_some_and(|c| matches!(c, b'T' | b't' | b'Y' | b'y'))
    }

    /// Build the cell for a character or memo field (validated, not stored).
    pub fn text_cell(&self, name: &str, text: &str) -> Result<(usize, Cell)> {
        let i = self.field_index(name)?;
        let field = &self.fields[i];
        let bytes = encoding::encode_with(text, self.encoding)
            .map_err(|e| FoxProError::Encoding(format!("field {}: {e}", field.name)))?;
        if field.is_memo() {
            return Ok((
                i,
                Cell::Memo {
                    kind: MemoKind::Text,
                    data: bytes,
                },
            ));
        }
        if !matches!(field.kind, 'C' | 'V') {
            return Err(FoxProError::Validation(format!(
                "field {} is not a character field",
                field.name
            )));
        }
        let len = field.length as usize;
        if bytes.len() > len {
            return Err(FoxProError::Validation(format!(
                "value {text:?} is longer than field {} ({len} bytes)",
                field.name
            )));
        }
        let mut cell = bytes;
        cell.resize(len, b' ');
        Ok((i, Cell::Fixed(cell)))
    }

    pub fn num_cell(&self, name: &str, value: f64) -> Result<(usize, Cell)> {
        let i = self.field_index(name)?;
        Ok((
            i,
            Cell::Fixed(value::encode_numeric(&self.fields[i], value)?),
        ))
    }

    pub fn bool_cell(&self, name: &str, value: bool) -> Result<(usize, Cell)> {
        let i = self.field_index(name)?;
        if self.fields[i].kind != 'L' {
            return Err(FoxProError::Validation(format!(
                "field {name} is not a logical field"
            )));
        }
        Ok((i, Cell::Fixed(vec![if value { b'T' } else { b'F' }])))
    }

    pub fn memo_cell(&self, name: &str, data: Vec<u8>) -> Result<(usize, Cell)> {
        let i = self.field_index(name)?;
        if !self.fields[i].is_memo() {
            return Err(FoxProError::Validation(format!(
                "field {name} is not a memo field"
            )));
        }
        let kind = if self.fields[i].is_binary() {
            MemoKind::Binary
        } else {
            MemoKind::Text
        };
        Ok((i, Cell::Memo { kind, data }))
    }

    pub fn set_text(&self, row: &mut Row, name: &str, text: &str) -> Result<()> {
        let (i, cell) = self.text_cell(name, text)?;
        row.cells[i] = cell;
        Ok(())
    }

    pub fn set_num(&self, row: &mut Row, name: &str, value: f64) -> Result<()> {
        let (i, cell) = self.num_cell(name, value)?;
        row.cells[i] = cell;
        Ok(())
    }

    pub fn set_bool(&self, row: &mut Row, name: &str, value: bool) -> Result<()> {
        let (i, cell) = self.bool_cell(name, value)?;
        row.cells[i] = cell;
        Ok(())
    }

    // Row-index variants for editing rows already in the table.

    pub fn set_text_at(&mut self, row: usize, name: &str, text: &str) -> Result<()> {
        let (i, cell) = self.text_cell(name, text)?;
        self.rows[row].cells[i] = cell;
        Ok(())
    }

    pub fn set_num_at(&mut self, row: usize, name: &str, value: f64) -> Result<()> {
        let (i, cell) = self.num_cell(name, value)?;
        self.rows[row].cells[i] = cell;
        Ok(())
    }

    pub fn set_bool_at(&mut self, row: usize, name: &str, value: bool) -> Result<()> {
        let (i, cell) = self.bool_cell(name, value)?;
        self.rows[row].cells[i] = cell;
        Ok(())
    }

    pub fn set_memo_at(&mut self, row: usize, name: &str, data: Vec<u8>) -> Result<()> {
        let (i, cell) = self.memo_cell(name, data)?;
        self.rows[row].cells[i] = cell;
        Ok(())
    }

    /// Serialize the table and its memo file. Returns `(dbf, memo)` bytes;
    /// `memo` is `None` for tables without memo fields.
    pub fn to_bytes(&self) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        self.validate()?;
        let has_memo = self.fields.iter().any(|f| f.is_memo());
        let mut memo = has_memo.then(|| MemoWriter::new(self.memo_block_size));

        let header_len = HEADER_SIZE
            + self.fields.len() * FIELD_DESCRIPTOR_SIZE
            + 1
            + if self.header.is_vfp() {
                VFP_BACKLINK_SIZE
            } else {
                0
            };
        let record_len: usize = 1 + self.fields.iter().map(|f| f.length as usize).sum::<usize>();
        if header_len > u16::MAX as usize || record_len > u16::MAX as usize {
            return Err(FoxProError::Validation(
                "table structure is too large".into(),
            ));
        }

        let mut out = Vec::with_capacity(header_len + record_len * self.rows.len() + 1);
        let (y, m, d) = today();
        out.push(self.header.version);
        out.push((y.saturating_sub(1900)).min(255) as u8);
        out.push(m);
        out.push(d);
        out.extend_from_slice(&(self.rows.len() as u32).to_le_bytes());
        out.extend_from_slice(&(header_len as u16).to_le_bytes());
        out.extend_from_slice(&(record_len as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        out.push(self.header.flags | if has_memo { FLAG_HAS_MEMO } else { 0 });
        out.push(self.header.codepage);
        out.extend_from_slice(&[0u8; 2]);

        let mut offset = 1u32;
        for f in &self.fields {
            let mut d = [0u8; FIELD_DESCRIPTOR_SIZE];
            let name = f.name.as_bytes();
            d[..name.len().min(10)].copy_from_slice(&name[..name.len().min(10)]);
            d[11] = f.kind as u8;
            d[12..16].copy_from_slice(&offset.to_le_bytes());
            d[16] = f.length;
            d[17] = f.decimals;
            d[18] = f.flags;
            d[19..23].copy_from_slice(&f.autoinc_next.to_le_bytes());
            d[23] = f.autoinc_step;
            out.extend_from_slice(&d);
            offset += f.length as u32;
        }
        out.push(0x0D);
        if self.header.is_vfp() {
            let mut backlink = self.backlink.clone();
            backlink.resize(VFP_BACKLINK_SIZE, 0);
            out.extend_from_slice(&backlink);
        }

        for row in &self.rows {
            out.push(if row.deleted { b'*' } else { b' ' });
            for (f, cell) in self.fields.iter().zip(&row.cells) {
                match cell {
                    Cell::Fixed(bytes) => out.extend_from_slice(bytes),
                    Cell::Memo { kind, data } => {
                        let block = match memo.as_mut() {
                            Some(m) if !data.is_empty() => m.append(*kind, data)?,
                            _ => 0,
                        };
                        out.extend_from_slice(&memo::format_pointer(block, f.length));
                    }
                }
            }
        }
        out.push(0x1A);

        Ok((out, memo.map(MemoWriter::finish)))
    }

    /// Structural checks performed before anything is written.
    pub fn validate(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for f in &self.fields {
            if f.name.is_empty() || f.name.len() > 10 || !f.name.is_ascii() {
                return Err(FoxProError::Validation(format!(
                    "invalid field name {:?}",
                    f.name
                )));
            }
            if !names.insert(f.name.clone()) {
                return Err(FoxProError::Validation(format!(
                    "duplicate field name {}",
                    f.name
                )));
            }
        }
        for (r, row) in self.rows.iter().enumerate() {
            if row.cells.len() != self.fields.len() {
                return Err(FoxProError::Validation(format!(
                    "record {} has {} values but the table has {} fields",
                    r + 1,
                    row.cells.len(),
                    self.fields.len()
                )));
            }
            for (f, cell) in self.fields.iter().zip(&row.cells) {
                match cell {
                    Cell::Fixed(b) if f.is_memo() || b.len() != f.length as usize => {
                        return Err(FoxProError::Validation(format!(
                            "record {} field {} has a malformed value",
                            r + 1,
                            f.name
                        )));
                    }
                    Cell::Memo { .. } if !f.is_memo() => {
                        return Err(FoxProError::Validation(format!(
                            "record {} field {} is not a memo field",
                            r + 1,
                            f.name
                        )));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

fn today() -> (u16, u8, u8) {
    let now = Local::now();
    (now.year() as u16, now.month() as u8, now.day() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_table() -> Table {
        let fields = vec![
            FieldDef::new("NAME", 'C', 10, 0),
            FieldDef::new("QTY", 'N', 6, 2),
            FieldDef::new("OK", 'L', 1, 0),
            FieldDef::new("NOTES", 'M', 4, 0),
        ];
        let mut t = Table::new_vfp(fields, encoding_rs::WINDOWS_874, 64);
        let mut row = t.blank_row();
        t.set_text(&mut row, "NAME", "สมชาย").unwrap();
        t.set_num(&mut row, "QTY", 12.5).unwrap();
        t.set_bool(&mut row, "OK", true).unwrap();
        t.set_text(&mut row, "NOTES", &"memo text ".repeat(20))
            .unwrap();
        t.rows.push(row);
        let mut row2 = t.blank_row();
        t.set_text(&mut row2, "NAME", "second").unwrap();
        row2.deleted = true;
        t.rows.push(row2);
        t
    }

    #[test]
    fn roundtrip_in_memory_table() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.dbf");
        let t = sample_table();
        let (dbf, memo) = t.to_bytes().unwrap();
        fs::write(&path, dbf).unwrap();
        fs::write(dir.path().join("t.fpt"), memo.unwrap()).unwrap();

        let loaded = Table::load(&path, None).unwrap();
        assert_eq!(loaded.rows.len(), 2);
        assert_eq!(loaded.encoding, encoding_rs::WINDOWS_874);
        let row = &loaded.rows[0];
        assert_eq!(loaded.get_text(row, "NAME"), "สมชาย");
        assert_eq!(loaded.get_num(row, "QTY"), Some(12.5));
        assert!(loaded.get_bool(row, "OK"));
        assert_eq!(loaded.get_text(row, "NOTES"), "memo text ".repeat(20));
        assert!(loaded.rows[1].deleted);
        assert_eq!(loaded.rows, t.rows);
    }

    #[test]
    fn streaming_reader_decodes_values() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.dbf");
        let (dbf, memo) = sample_table().to_bytes().unwrap();
        fs::write(&path, dbf).unwrap();
        fs::write(dir.path().join("t.fpt"), memo.unwrap()).unwrap();

        let mut reader = TableReader::open(&path, None).unwrap();
        assert_eq!(reader.record_count(), 2);
        let mut buf = Vec::new();
        reader.read_raw(1, &mut buf).unwrap();
        assert_eq!(
            reader.value(&buf, 0).unwrap().to_json(true),
            serde_json::json!("สมชาย")
        );
        assert_eq!(reader.value(&buf, 1).unwrap(), Val::Num(12.5));
        assert_eq!(reader.value(&buf, 2).unwrap(), Val::Bool(true));
        reader.read_raw(2, &mut buf).unwrap();
        assert!(TableReader::is_deleted(&buf));
        assert!(reader.read_raw(3, &mut buf).is_err());
    }

    #[test]
    fn rejects_values_that_do_not_fit() {
        let t = sample_table();
        let mut row = t.blank_row();
        assert!(
            t.set_text(&mut row, "NAME", "this is far too long")
                .is_err()
        );
        assert!(t.set_num(&mut row, "QTY", 1234567.0).is_err());
        // VFP drops decimals before overflowing: N(6,2) holds 123456.
        assert!(t.set_num(&mut row, "QTY", 123456.0).is_ok());
    }

    #[test]
    fn truncated_file_is_detected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.dbf");
        let (mut dbf, memo) = sample_table().to_bytes().unwrap();
        dbf.truncate(dbf.len() - 20);
        fs::write(&path, dbf).unwrap();
        fs::write(dir.path().join("t.fpt"), memo.unwrap()).unwrap();

        let reader = TableReader::open(&path, None).unwrap();
        assert_eq!(reader.record_count(), 1);
        assert!(!reader.structure.warnings.is_empty());
        assert!(Table::load(&path, None).is_err());
    }
}
