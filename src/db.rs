//! Read-only database tools on top of [`crate::dbf`]: table structure, index
//! tags and cursor-style record access (GO/SKIP/NEXT with a filter).

use crate::dbf::expr::{self, Context, Expr};
use crate::dbf::{TableReader, Val, cdx};
use crate::encoding;
use crate::error::{FoxProError, Result};
use crate::fsutil;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 1000;
const DEFAULT_MAX_MEMO_CHARS: usize = 2000;
/// A single call never scans for longer than this; the cursor is returned
/// so the agent can continue.
const MAX_SCAN_TIME: Duration = Duration::from_secs(20);

fn open(path: &Path, encoding: Option<&str>) -> Result<TableReader> {
    if !fsutil::has_extension(path, "dbf") && !fsutil::has_extension(path, "dbc") {
        return Err(FoxProError::InvalidArgument(format!(
            "{} is not a table (.dbf)",
            path.display()
        )));
    }
    TableReader::open(path, encoding)
}

fn field_json(r: &TableReader, include_system: bool) -> Vec<Value> {
    r.structure
        .fields
        .iter()
        .filter(|f| include_system || !f.is_system())
        .map(|f| {
            let mut v = json!({
                "name": f.name,
                "type": f.kind.to_string(),
                "type_name": f.type_name(),
                "length": f.length,
            });
            if f.decimals > 0 {
                v["decimals"] = json!(f.decimals);
            }
            if f.is_nullable() {
                v["nullable"] = json!(true);
            }
            if f.is_binary() && !matches!(f.kind, 'G' | 'W' | 'Q') {
                v["binary"] = json!(true);
            }
            if f.flags & crate::dbf::FIELD_AUTOINC == crate::dbf::FIELD_AUTOINC {
                v["autoinc"] = json!({ "next": f.autoinc_next, "step": f.autoinc_step });
            }
            v
        })
        .collect()
}

/// Field list (name, type, length).
pub fn inspect_table(path: &Path, encoding: Option<&str>) -> Result<Value> {
    let r = open(path, encoding)?;
    Ok(json!({
        "table": path.to_string_lossy(),
        "record_count": r.record_count(),
        "fields": field_json(&r, false),
    }))
}

/// Detailed structure: header, code page, memo file, index tags.
pub fn describe_table(path: &Path, encoding: Option<&str>) -> Result<Value> {
    let r = open(path, encoding)?;
    let h = &r.structure.header;
    let mut warnings = r.structure.warnings.clone();
    if let Some(e) = r.memo_error() {
        warnings.push(format!("memo file: {e}"));
    }

    let cdx_path = fsutil::companion(path, "cdx");
    let indexes = if cdx_path.is_file() {
        match cdx::read_tags(&cdx_path) {
            Ok(tags) => json!({ "file": cdx_path.to_string_lossy(), "tags": tags }),
            Err(e) => {
                warnings.push(format!("could not read index tags: {e}"));
                json!({ "file": cdx_path.to_string_lossy(), "tags": null })
            }
        }
    } else if h.flags & crate::dbf::FLAG_HAS_CDX != 0 {
        warnings.push(
            "header says the table has a structural index but the .cdx file is missing".into(),
        );
        Value::Null
    } else {
        Value::Null
    };

    Ok(json!({
        "table": path.to_string_lossy(),
        "format": { "version": format!("0x{:02X}", h.version), "name": h.version_name() },
        "last_update": format!("{:04}-{:02}-{:02}", h.last_update.0, h.last_update.1, h.last_update.2),
        "record_count": r.record_count(),
        "header_length": h.header_len,
        "record_length": h.record_len,
        "code_page": { "mark": format!("0x{:02X}", h.codepage), "name": encoding::codepage_name(h.codepage) },
        "decoding_as": r.encoding.name().to_lowercase(),
        "is_database_container": h.flags & crate::dbf::FLAG_IS_DBC != 0,
        "memo_file": r.memo_path().map(|p| p.to_string_lossy().into_owned()),
        "fields": field_json(&r, true),
        "indexes": indexes,
        "warnings": warnings,
    }))
}

#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub fields: Option<Vec<String>>,
    pub filter: Option<String>,
    /// First record to examine (1-based RECNO), like GO n.
    pub start: u32,
    /// Matching records to skip before collecting, like SKIP n.
    pub skip: usize,
    /// Maximum rows returned, like NEXT n.
    pub limit: usize,
    pub include_deleted: bool,
    pub max_memo_chars: usize,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            fields: None,
            filter: None,
            start: 1,
            skip: 0,
            limit: DEFAULT_LIMIT,
            include_deleted: false,
            max_memo_chars: DEFAULT_MAX_MEMO_CHARS,
        }
    }
}

struct RecordCtx<'a> {
    reader: &'a mut TableReader,
    raw: &'a [u8],
    recno: u32,
}

impl Context for RecordCtx<'_> {
    fn field(&mut self, name: &str) -> Result<Val> {
        let idx = self.reader.structure.field_index(name).ok_or_else(|| {
            FoxProError::InvalidArgument(format!("expression: unknown field {name}"))
        })?;
        self.reader.value(self.raw, idx)
    }

    fn recno(&self) -> u32 {
        self.recno
    }

    fn deleted(&self) -> bool {
        TableReader::is_deleted(self.raw)
    }
}

fn clip(v: Val, max_chars: usize) -> Value {
    match v {
        Val::Str(s) if s.chars().count() > max_chars => {
            let cut: String = s.chars().take(max_chars).collect();
            json!({ "text": cut, "truncated": true, "total_chars": s.chars().count() })
        }
        other => other.to_json(true),
    }
}

/// Cursor-style read: start at `start`, skip `skip` matches, return up to
/// `limit` matching rows and the RECNO to continue from.
pub fn query(path: &Path, encoding: Option<&str>, opts: &QueryOptions) -> Result<Value> {
    let mut reader = open(path, encoding)?;
    let total = reader.record_count();
    let limit = opts.limit.clamp(1, MAX_LIMIT);

    let filter: Option<Expr> = opts.filter.as_deref().map(expr::parse).transpose()?;
    if let Some(f) = &filter {
        let mut used = Vec::new();
        f.fields(&mut used);
        if let Some(unknown) = used
            .iter()
            .find(|n| reader.structure.field_index(n).is_none())
        {
            return Err(FoxProError::InvalidArgument(format!(
                "expression refers to unknown field {unknown}"
            )));
        }
    }

    let columns: Vec<usize> = match &opts.fields {
        Some(names) if !names.is_empty() => names
            .iter()
            .map(|n| {
                reader
                    .structure
                    .field_index(n)
                    .ok_or_else(|| FoxProError::InvalidArgument(format!("unknown field {n}")))
            })
            .collect::<Result<_>>()?,
        _ => (0..reader.structure.fields.len())
            .filter(|&i| !reader.structure.fields[i].is_system())
            .collect(),
    };
    let names: Vec<String> = columns
        .iter()
        .map(|&i| reader.structure.fields[i].name.clone())
        .collect();

    let started = Instant::now();
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    let mut scanned = 0u64;
    let mut next: Option<u32> = None;
    let mut stopped_early = false;
    let mut raw = Vec::with_capacity(reader.structure.header.record_len as usize);
    let mut recno = opts.start.max(1);

    while recno <= total {
        if scanned > 0 && scanned.is_multiple_of(4096) && started.elapsed() > MAX_SCAN_TIME {
            stopped_early = true;
            next = Some(recno);
            break;
        }
        reader.read_raw(recno, &mut raw)?;
        scanned += 1;
        let deleted = TableReader::is_deleted(&raw);
        let current = recno;
        recno += 1;
        if deleted && !opts.include_deleted {
            continue;
        }
        if let Some(f) = &filter {
            let mut ctx = RecordCtx {
                reader: &mut reader,
                raw: &raw,
                recno: current,
            };
            let ok = f
                .matches(&mut ctx)
                .map_err(|e| FoxProError::InvalidArgument(format!("record {current}: {e}")))?;
            if !ok {
                continue;
            }
        }
        if skipped < opts.skip {
            skipped += 1;
            continue;
        }

        let mut row = Map::new();
        row.insert("_recno".into(), json!(current));
        if deleted {
            row.insert("_deleted".into(), json!(true));
        }
        for (&i, name) in columns.iter().zip(&names) {
            let v = match reader.value(&raw, i) {
                Ok(v) => clip(v, opts.max_memo_chars),
                Err(e) => json!({ "error": e.to_string() }),
            };
            row.insert(name.clone(), v);
        }
        rows.push(Value::Object(row));
        if rows.len() >= limit {
            if recno <= total {
                next = Some(recno);
            }
            break;
        }
    }

    let eof = next.is_none();
    let mut result = json!({
        "table": path.to_string_lossy(),
        "total_records": total,
        "returned": rows.len(),
        "scanned": scanned,
        "eof": eof,
        "next_recno": next,
        "rows": rows,
    });
    if stopped_early {
        result["note"] = json!(format!(
            "scan paused after {} s; call again with start = next_recno to continue",
            MAX_SCAN_TIME.as_secs()
        ));
    }
    if let Some(e) = reader.memo_error() {
        result["warnings"] = json!([format!("memo file: {e}")]);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::{FieldDef, Table};
    use tempfile::TempDir;

    fn make_table(dir: &TempDir, rows: usize) -> std::path::PathBuf {
        let fields = vec![
            FieldDef::new("CUST_ID", 'C', 8, 0),
            FieldDef::new("NAME", 'C', 30, 0),
            FieldDef::new("BALANCE", 'N', 10, 2),
            FieldDef::new("NOTES", 'M', 4, 0),
        ];
        let mut t = Table::new_vfp(fields, encoding_rs::WINDOWS_874, 64);
        for i in 0..rows {
            let mut r = t.blank_row();
            t.set_text(&mut r, "CUST_ID", &format!("C{i:04}")).unwrap();
            t.set_text(
                &mut r,
                "NAME",
                if i % 2 == 0 {
                    "สมชาย"
                } else {
                    "John"
                },
            )
            .unwrap();
            t.set_num(&mut r, "BALANCE", i as f64 * 10.0).unwrap();
            t.set_text(&mut r, "NOTES", &"x".repeat(i * 100)).unwrap();
            r.deleted = i == 3;
            t.rows.push(r);
        }
        let path = dir.path().join("customer.dbf");
        let (dbf, memo) = t.to_bytes().unwrap();
        std::fs::write(&path, dbf).unwrap();
        std::fs::write(dir.path().join("customer.fpt"), memo.unwrap()).unwrap();
        path
    }

    #[test]
    fn inspect_and_describe() {
        let dir = TempDir::new().unwrap();
        let path = make_table(&dir, 3);
        let v = inspect_table(&path, None).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 4);
        assert_eq!(v["fields"][2]["decimals"], json!(2));
        let d = describe_table(&path, None).unwrap();
        assert_eq!(d["decoding_as"], json!("windows-874"));
        assert_eq!(d["record_count"], json!(3));
    }

    #[test]
    fn query_with_cursor_and_filter() {
        let dir = TempDir::new().unwrap();
        let path = make_table(&dir, 10);

        let page1 = query(
            &path,
            None,
            &QueryOptions {
                limit: 4,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page1["returned"], json!(4));
        // Record 4 (index 3) is deleted and skipped.
        assert_eq!(page1["rows"][3]["_recno"], json!(5));
        let next = page1["next_recno"].as_u64().unwrap() as u32;

        let page2 = query(
            &path,
            None,
            &QueryOptions {
                start: next,
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page2["eof"], json!(true));
        assert_eq!(page2["returned"], json!(5));

        let found = query(
            &path,
            None,
            &QueryOptions {
                filter: Some("name = 'สมชาย' AND balance >= 40".into()),
                fields: Some(vec!["cust_id".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let ids: Vec<&str> = found["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["CUST_ID"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["C0004", "C0006", "C0008"]);

        let memo = query(
            &path,
            None,
            &QueryOptions {
                start: 10,
                max_memo_chars: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(memo["rows"][0]["NOTES"]["truncated"], json!(true));

        assert!(
            query(
                &path,
                None,
                &QueryOptions {
                    filter: Some("nope = 1".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
