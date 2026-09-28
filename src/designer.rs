//! Shared "Validate → Backup → Modify → Validate → Return diff" pipeline for
//! designer files (SCX/SCT, FRX/FRT) that are stored as VFP tables.

use crate::backup::{self, FileBackup};
use crate::code::make_diff;
use crate::error::Result;
use crate::fsutil;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub trait DesignerDocument {
    fn path(&self) -> &Path;
    /// Deterministic human-readable rendering used to produce diffs.
    fn render_text(&self) -> String;
    /// Whole-document validation. Returns non-fatal warnings.
    fn validate(&self) -> Result<Vec<String>>;
    /// Serialized files to write: `(path, bytes)`.
    fn serialize(&self) -> Result<Vec<(PathBuf, Vec<u8>)>>;
}

#[derive(Debug, Serialize)]
pub struct Mutation {
    pub success: bool,
    pub dry_run: bool,
    pub file: PathBuf,
    pub result: Value,
    pub diff: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<FileBackup>,
    pub written: Vec<PathBuf>,
}

/// Validate, diff and (unless `dry_run`) back up and atomically write `doc`.
/// `before` is the rendering prior to the change (empty for new files).
pub fn commit<D: DesignerDocument>(
    doc: &D,
    before: &str,
    workspace: &Path,
    result: Value,
    dry_run: bool,
    make_backup: bool,
) -> Result<Mutation> {
    let warnings = doc.validate()?;
    let diff = make_diff(before, &doc.render_text());
    // Serialize before touching the disk so encoding or size errors never
    // leave a half-written artifact.
    let files = doc.serialize()?;

    if dry_run {
        return Ok(Mutation {
            success: true,
            dry_run: true,
            file: doc.path().to_path_buf(),
            result,
            diff,
            warnings,
            backups: Vec::new(),
            written: Vec::new(),
        });
    }

    let backups = if make_backup {
        backup::create_group(workspace, doc.path())?
            .into_iter()
            .filter(|b| files.iter().any(|(p, _)| p == &b.file))
            .collect()
    } else {
        Vec::new()
    };
    let refs: Vec<(&Path, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_path(), b.as_slice()))
        .collect();
    fsutil::atomic_write_all(&refs)?;

    Ok(Mutation {
        success: true,
        dry_run: false,
        file: doc.path().to_path_buf(),
        result,
        diff,
        warnings,
        backups,
        written: files.into_iter().map(|(p, _)| p).collect(),
    })
}
