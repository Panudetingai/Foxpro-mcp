//! Database tools (Phase 5): read-only DBF access.

use super::{Args, Tool, ToolOutput, blocking, schema, unknown};
use crate::db::{self, QueryOptions};
use crate::error::{FoxProError, Result};
use crate::mcp::AppState;
use serde_json::json;
use std::sync::Arc;

pub fn tools() -> Vec<Tool> {
    let cursor = json!({
        "table": { "type": "string", "description": ".dbf file" },
        "fields": { "type": "array", "items": { "type": "string" }, "description": "Columns to return (default: all)" },
        "start": { "type": "integer", "description": "First RECNO to examine (GO), default 1; pass next_recno to continue" },
        "skip": { "type": "integer", "description": "Matching records to skip first (SKIP)" },
        "limit": { "type": "integer", "description": "Maximum rows (NEXT), default 50, max 1000" },
        "include_deleted": { "type": "boolean" },
        "max_memo_chars": { "type": "integer", "description": "Truncate memo text (default 2000)" },
        "encoding": { "type": "string", "description": "Override the code page, e.g. windows-874" }
    });
    let mut query_props = cursor.clone();
    query_props["where"] = json!({ "type": "string", "description": "Optional FoxPro filter, e.g. \"UPPER(name) = 'JOHN' AND balance > 0\"" });
    let mut find_props = cursor;
    find_props["expression"] = json!({ "type": "string", "description": "FoxPro filter expression: = == <> < > <= >= $, AND/OR/NOT, UPPER() ALLTRIM() LEFT() SUBSTR() EMPTY() BETWEEN() INLIST() LIKE() YEAR() DTOS() DELETED() RECNO() ..., dates as {^2024-01-31}" });

    vec![
        Tool {
            name: "foxpro.inspect_table",
            description: "List the fields (name, type, length, decimals) and record count of a DBF table.",
            input_schema: schema(
                json!({ "table": { "type": "string" }, "encoding": { "type": "string" } }),
                &["table"],
            ),
        },
        Tool {
            name: "foxpro.describe_table",
            description: "Detailed DBF structure: format version, code page, record/header length, memo file, nullable/autoinc fields, CDX index tags (name, key expression, filter) and integrity warnings.",
            input_schema: schema(
                json!({ "table": { "type": "string" }, "encoding": { "type": "string" } }),
                &["table"],
            ),
        },
        Tool {
            name: "foxpro.query_table",
            description: "Cursor-style read of a DBF (like GO start / SKIP / NEXT limit) with an optional filter. Returns rows with _recno, eof and next_recno for paging. Read-only; decodes Thai/ANSI code pages.",
            input_schema: schema(query_props, &["table"]),
        },
        Tool {
            name: "foxpro.find_records",
            description: "Locate records matching a FoxPro expression (like LOCATE FOR / CONTINUE). Returns matching rows with _recno and next_recno to continue.",
            input_schema: schema(find_props, &["table", "expression"]),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "foxpro.inspect_table"
            | "foxpro.describe_table"
            | "foxpro.query_table"
            | "foxpro.find_records"
    )
}

fn options(a: &Args, filter_key: &str) -> Result<QueryOptions> {
    let start = a.u64("start")?.unwrap_or(1);
    let start = u32::try_from(start)
        .map_err(|_| FoxProError::InvalidArgument("start is out of range".into()))?;
    let fields = a.strings("fields")?;
    Ok(QueryOptions {
        fields: (!fields.is_empty()).then_some(fields),
        filter: a
            .str(filter_key)?
            .or(a.str("filter")?)
            .filter(|f| !f.trim().is_empty())
            .map(str::to_string),
        start,
        skip: a.usize("skip")?.unwrap_or(0),
        limit: a.usize("limit")?.unwrap_or(db::DEFAULT_LIMIT),
        include_deleted: a.flag("include_deleted", false)?,
        max_memo_chars: a.usize("max_memo_chars")?.unwrap_or(2000).max(1),
    })
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    let path = state
        .sandbox
        .validate(&a.req_path(&["table", "file", "path"])?)?;
    let encoding = a.str("encoding")?.map(str::to_string);
    match name {
        "foxpro.inspect_table" => {
            blocking(move || Ok(db::inspect_table(&path, encoding.as_deref())?.into())).await
        }
        "foxpro.describe_table" => {
            blocking(move || Ok(db::describe_table(&path, encoding.as_deref())?.into())).await
        }
        "foxpro.query_table" => {
            let opts = options(&a, "where")?;
            blocking(move || Ok(db::query(&path, encoding.as_deref(), &opts)?.into())).await
        }
        "foxpro.find_records" => {
            let opts = options(&a, "expression")?;
            if opts.filter.is_none() {
                return Err(FoxProError::InvalidArgument(
                    "Missing required argument: expression".into(),
                ));
            }
            blocking(move || Ok(db::query(&path, encoding.as_deref(), &opts)?.into())).await
        }
        other => Err(unknown(other)),
    }
}
