//! Source code tools: read, write, search, patch, rollback.

use super::{Args, Tool, ToolOutput, blocking, mutate, schema};
use crate::backup;
use crate::code::{self, PatchOptions, WriteOptions};
use crate::error::Result;
use crate::fsutil::display_relative;
use crate::mcp::AppState;
use serde_json::json;
use std::sync::Arc;

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "foxpro.read_code",
            description: "Read a text/source file with encoding detection (UTF-8, Windows-1252, Windows-874). Use start_line/end_line for large files; responses report total_lines and are capped at max_bytes.",
            input_schema: schema(
                json!({
                    "path": { "type": "string", "description": "File path relative to the workspace (or absolute inside it)" },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 },
                    "encoding": { "type": "string", "description": "Force an encoding, e.g. windows-874" },
                    "max_bytes": { "type": "integer", "description": "Cap on returned text (default 262144)" }
                }),
                &["path"],
            ),
        },
        Tool {
            name: "foxpro.write_code",
            description: "Write a whole file. Preserves the existing encoding (new .prg files use Windows-1252, or Windows-874 when the text contains Thai); refuses text the encoding cannot represent. Backs up the old file, writes atomically, returns a diff. dry_run previews only.",
            input_schema: schema(
                json!({
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean", "description": "Default true" },
                    "encoding": { "type": "string", "description": "Override the target encoding" }
                }),
                &["path", "content"],
            ),
        },
        Tool {
            name: "foxpro.search_code",
            description: "Search text files for a literal or regex (case-insensitive by default). Binary FoxPro files (dbf/scx/frx/...), .git and backup folders are skipped. Returns file, line, column.",
            input_schema: schema(
                json!({
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Directory to search (default: workspace root)" },
                    "regex": { "type": "boolean" },
                    "case_insensitive": { "type": "boolean", "description": "Default true" },
                    "file_type": { "type": "string", "description": "Extension filter, e.g. \"prg\" or \"prg,h\"" },
                    "max_results": { "type": "integer", "description": "Default 100" }
                }),
                &["pattern"],
            ),
        },
        Tool {
            name: "foxpro.apply_patch",
            description: "Replace an exact text fragment (no fuzzy matching). old_text must occur exactly once unless replace_all is true; LF text also matches CRLF files. Backs up, writes atomically, returns a diff.",
            input_schema: schema(
                json!({
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "replace_all": { "type": "boolean" },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean", "description": "Default true" }
                }),
                &["path", "old_text", "new_text"],
            ),
        },
        Tool {
            name: "foxpro.rollback",
            description: "Restore a file (and companions such as .sct/.frt) from .mcp-backup: the latest backup, or the one whose timestamp starts with `timestamp`. The replaced state is backed up too. Set list=true to only list backups.",
            input_schema: schema(
                json!({
                    "path": { "type": "string" },
                    "timestamp": { "type": "string", "description": "e.g. 20260928_101500" },
                    "list": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                }),
                &["path"],
            ),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "foxpro.read_code"
            | "foxpro.write_code"
            | "foxpro.search_code"
            | "foxpro.apply_patch"
            | "foxpro.rollback"
    )
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    let ws = state.sandbox.workspace().to_path_buf();
    match name {
        "foxpro.read_code" => {
            let path = state.sandbox.validate(&a.req_path(&["path", "file"])?)?;
            let start = a.usize("start_line")?;
            let end = a.usize("end_line")?;
            let encoding = a.str("encoding")?.map(str::to_string);
            let max = a.usize("max_bytes")?;
            blocking(move || {
                let fc = code::read_file(&path, start, end, encoding.as_deref(), max)?;
                Ok(json!({
                    "path": display_relative(&fc.path, &ws),
                    "encoding": fc.encoding,
                    "total_lines": fc.total_lines,
                    "start_line": fc.start_line,
                    "end_line": fc.end_line,
                    "truncated": fc.truncated,
                    "text": fc.text,
                })
                .into())
            })
            .await
        }
        "foxpro.write_code" => {
            let path = state.sandbox.resolve(&a.req_path(&["path", "file"])?)?;
            let content = a.req_str("content")?.to_string();
            let options = WriteOptions {
                dry_run: a.flag("dry_run", false)?,
                backup: a.flag("backup", true)?,
                encoding: a.str("encoding")?.map(str::to_string),
            };
            mutate(&state, move || {
                let r = code::write_file(&path, &content, Some(&ws), options)?;
                Ok(json!({
                    "path": display_relative(&path, &ws),
                    "dry_run": r.dry_run,
                    "created": r.created,
                    "encoding": r.encoding,
                    "bytes_written": r.bytes_written,
                    "backup_path": r.backup_path.map(|p| display_relative(&p, &ws)),
                    "diff": r.diff,
                })
                .into())
            })
            .await
        }
        "foxpro.search_code" => {
            let root = state
                .sandbox
                .validate(std::path::Path::new(a.str("path")?.unwrap_or(".")))?;
            if !root.is_dir() {
                return Err(crate::error::FoxProError::InvalidArgument(format!(
                    "{} is not a directory",
                    root.display()
                )));
            }
            let pattern = a.req_str("pattern")?.to_string();
            let regex = a.flag("regex", false)?;
            let ci = a.flag("case_insensitive", true)?;
            let file_type = a.str("file_type")?.map(str::to_string);
            let max = a.usize("max_results")?;
            blocking(move || {
                let r = code::search_files(&root, &pattern, regex, ci, file_type.as_deref(), max)?;
                Ok(json!({
                    "matches": r.matches.iter().map(|m| json!({
                        "file": display_relative(&m.file, &ws),
                        "line": m.line,
                        "column": m.column,
                        "text": m.text,
                    })).collect::<Vec<_>>(),
                    "count": r.matches.len(),
                    "truncated": r.truncated,
                    "files_scanned": r.files_scanned,
                    "files_skipped": r.files_skipped,
                })
                .into())
            })
            .await
        }
        "foxpro.apply_patch" => {
            let path = state.sandbox.validate(&a.req_path(&["path", "file"])?)?;
            let old = a.req_str("old_text")?.to_string();
            let new = a.req_str("new_text")?.to_string();
            let options = PatchOptions {
                dry_run: a.flag("dry_run", false)?,
                backup: a.flag("backup", true)?,
                replace_all: a.flag("replace_all", false)?,
            };
            mutate(&state, move || {
                let r = code::apply_patch(&path, &old, &new, Some(&ws), options)?;
                Ok(json!({
                    "path": display_relative(&path, &ws),
                    "dry_run": r.dry_run,
                    "replacements": r.replacements,
                    "line_endings_normalized": r.line_endings_normalized,
                    "backup_path": r.backup_path.map(|p| display_relative(&p, &ws)),
                    "diff": r.diff,
                })
                .into())
            })
            .await
        }
        "foxpro.rollback" => {
            let path = state.sandbox.resolve(&a.req_path(&["path", "file"])?)?;
            let timestamp = a.str("timestamp")?.map(str::to_string);
            let list = a.flag("list", false)?;
            let dry_run = a.flag("dry_run", false)?;
            mutate(&state, move || {
                let backups = backup::list(&ws, &path)?;
                if list || dry_run {
                    let chosen = match &timestamp {
                        Some(ts) => backups.iter().find(|b| b.timestamp.starts_with(ts.trim())),
                        None => backups.first(),
                    };
                    return Ok(json!({
                        "path": display_relative(&path, &ws),
                        "dry_run": dry_run,
                        "would_restore": if dry_run { chosen.map(|b| b.timestamp.clone()) } else { None },
                        "backups": backups.iter().map(|b| json!({
                            "timestamp": b.timestamp,
                            "backup": display_relative(&b.path, &ws),
                        })).collect::<Vec<_>>(),
                    })
                    .into());
                }
                let restored = backup::restore(&ws, &path, timestamp.as_deref())?;
                Ok(json!({
                    "restored": restored.iter().map(|r| json!({
                        "file": display_relative(&r.file, &ws),
                        "from": display_relative(&r.from, &ws),
                        "timestamp": r.timestamp,
                        "previous_state_backup": r.previous_state_backup.as_ref().map(|p| display_relative(p, &ws)),
                    })).collect::<Vec<_>>(),
                })
                .into())
            })
            .await
        }
        other => Err(super::unknown(other)),
    }
}
