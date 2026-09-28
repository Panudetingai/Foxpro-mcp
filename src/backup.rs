//! Timestamped backups under `<workspace>/.mcp-backup/`, mirroring the
//! workspace layout: `src/customer.prg` → `.mcp-backup/src/customer.prg.<ts>.bak`.
//!
//! Multi-file FoxPro artifacts (SCX+SCT, FRX+FRT, ...) are backed up with a
//! shared timestamp so they can be restored together.

use crate::error::{FoxProError, Result};
use crate::fsutil;
use chrono::Local;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const BACKUP_DIR: &str = ".mcp-backup";
/// Number of backups kept per file; older ones are pruned.
const KEEP_PER_FILE: usize = 30;

#[derive(Debug, Clone, Serialize)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileBackup {
    pub file: PathBuf,
    /// `None` when the file did not exist before the change (rollback = delete).
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Restored {
    pub file: PathBuf,
    pub from: PathBuf,
    pub timestamp: String,
    /// Backup of the state that was replaced, so a rollback can be undone.
    pub previous_state_backup: Option<PathBuf>,
}

pub fn timestamp() -> String {
    Local::now().format("%Y%m%d_%H%M%S_%3f").to_string()
}

/// Companion files that belong to a FoxPro artifact and must be backed up
/// and restored together with it.
pub fn companions(original: &Path) -> Vec<PathBuf> {
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let exts: &[&str] = match ext.as_str() {
        "scx" => &["sct"],
        "frx" => &["frt"],
        "vcx" => &["vct"],
        "lbx" => &["lbt"],
        "mnx" => &["mnt"],
        "pjx" => &["pjt"],
        "dbf" => &["fpt", "cdx"],
        "dbc" => &["dct", "dcx"],
        _ => &[],
    };
    exts.iter()
        .map(|e| fsutil::companion(original, e))
        .collect()
}

fn relative<'a>(workspace: &Path, original: &'a Path) -> Result<&'a Path> {
    original
        .strip_prefix(workspace)
        .map_err(|_| FoxProError::SandboxViolation {
            workspace: workspace.to_path_buf(),
            requested: original.to_path_buf(),
        })
}

fn backup_dir(workspace: &Path, original: &Path) -> Result<PathBuf> {
    let rel = relative(workspace, original)?;
    Ok(workspace
        .join(BACKUP_DIR)
        .join(rel.parent().unwrap_or(Path::new(""))))
}

fn file_name(original: &Path) -> Result<String> {
    original
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| FoxProError::InvalidArgument(format!("not a file: {}", original.display())))
}

fn copy_to_backup(workspace: &Path, original: &Path, ts: &str) -> Result<Option<PathBuf>> {
    if !original.is_file() {
        return Ok(None);
    }
    let dir = backup_dir(workspace, original)?;
    let name = file_name(original)?;
    fs::create_dir_all(&dir)?;

    let mut candidate = dir.join(format!("{name}.{ts}.bak"));
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{name}.{ts}-{n}.bak"));
        n += 1;
    }
    fs::copy(original, &candidate)?;
    prune(workspace, original);
    Ok(Some(candidate))
}

/// Back up a single file. Returns `None` when the file does not exist yet.
pub fn create(workspace: &Path, original: &Path) -> Result<Option<PathBuf>> {
    copy_to_backup(workspace, original, &timestamp())
}

/// Back up `original` and all of its companion files with one timestamp.
pub fn create_group(workspace: &Path, original: &Path) -> Result<Vec<FileBackup>> {
    let ts = timestamp();
    let mut files = vec![original.to_path_buf()];
    files.extend(companions(original));
    files
        .into_iter()
        .map(|file| {
            let backup = copy_to_backup(workspace, &file, &ts)?;
            Ok(FileBackup { file, backup })
        })
        .collect()
}

fn parse_timestamp(name: &str, file_name: &str) -> Option<String> {
    let middle = name.strip_prefix(file_name)?.strip_prefix('.')?;
    let ts = middle.strip_suffix(".bak")?;
    let valid = !ts.is_empty()
        && ts.as_bytes()[0].is_ascii_digit()
        && ts
            .chars()
            .all(|c| c.is_ascii_digit() || c == '_' || c == '-');
    valid.then(|| ts.to_string())
}

/// Backups for `original`, newest first.
pub fn list(workspace: &Path, original: &Path) -> Result<Vec<BackupEntry>> {
    let dir = backup_dir(workspace, original)?;
    let name = file_name(original)?;
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<BackupEntry> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let entry_name = e.file_name().to_string_lossy().into_owned();
            let ts = parse_timestamp(&entry_name, &name)
                .or_else(|| parse_timestamp(&entry_name.to_lowercase(), &name.to_lowercase()))?;
            Some(BackupEntry {
                path: e.path(),
                timestamp: ts,
            })
        })
        .collect();
    // Timestamps are zero-padded, so lexical order is chronological.
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(entries)
}

fn prune(workspace: &Path, original: &Path) {
    if let Ok(entries) = list(workspace, original) {
        for old in entries.into_iter().skip(KEEP_PER_FILE) {
            if let Err(e) = fs::remove_file(&old.path) {
                tracing::warn!(path = %old.path.display(), "failed to prune backup: {e}");
            }
        }
    }
}

/// Restore `original` (and its companions) from the latest backup, or from the
/// backup whose timestamp starts with `timestamp`. The current state is backed
/// up first so the rollback itself can be undone.
pub fn restore(
    workspace: &Path,
    original: &Path,
    timestamp: Option<&str>,
) -> Result<Vec<Restored>> {
    let backups = list(workspace, original)?;
    if backups.is_empty() {
        return Err(FoxProError::NotFound(format!(
            "No backups found for {}",
            original.display()
        )));
    }

    let chosen = match timestamp {
        Some(ts) => backups
            .into_iter()
            .find(|b| b.timestamp.starts_with(ts.trim()))
            .ok_or_else(|| FoxProError::NotFound(format!("No backup matching timestamp {ts}")))?,
        None => backups.into_iter().next().ok_or_else(|| {
            FoxProError::NotFound(format!("No backups found for {}", original.display()))
        })?,
    };

    // Plan the whole restore before touching anything.
    let mut plan = vec![(original.to_path_buf(), chosen.path.clone())];
    for companion in companions(original) {
        if let Some(b) = list(workspace, &companion)?
            .into_iter()
            .find(|b| b.timestamp == chosen.timestamp)
        {
            plan.push((companion, b.path));
        }
    }

    let safety_ts = self::timestamp();
    let mut restored = Vec::with_capacity(plan.len());
    for (file, from) in plan {
        let previous_state_backup = copy_to_backup(workspace, &file, &safety_ts)?;
        let bytes = fs::read(&from)?;
        fsutil::atomic_write(&file, &bytes)?;
        restored.push(Restored {
            file,
            from,
            timestamp: chosen.timestamp.clone(),
            previous_state_backup,
        });
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn backup_and_restore() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("src").join("main.prg");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "version 1").unwrap();

        let backup = create(dir.path(), &file).unwrap().unwrap();
        fs::write(&file, "version 2").unwrap();

        restore(dir.path(), &file, None).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "version 1");
        assert!(backup.to_string_lossy().contains(".mcp-backup"));
        assert!(backup.to_string_lossy().contains("main.prg."));
    }

    #[test]
    fn backups_of_different_files_do_not_mix() {
        let dir = TempDir::new().unwrap();
        let prg = dir.path().join("customer.prg");
        let other = dir.path().join("customer.h");
        fs::write(&prg, "prg v1").unwrap();
        fs::write(&other, "h v1").unwrap();

        create(dir.path(), &prg).unwrap();
        create(dir.path(), &other).unwrap();
        fs::write(&prg, "prg v2").unwrap();

        assert_eq!(list(dir.path(), &prg).unwrap().len(), 1);
        restore(dir.path(), &prg, None).unwrap();
        assert_eq!(fs::read_to_string(&prg).unwrap(), "prg v1");
    }

    #[test]
    fn group_backup_restores_companions() {
        let dir = TempDir::new().unwrap();
        let scx = dir.path().join("form.scx");
        let sct = dir.path().join("form.sct");
        fs::write(&scx, "scx v1").unwrap();
        fs::write(&sct, "sct v1").unwrap();

        let backups = create_group(dir.path(), &scx).unwrap();
        assert_eq!(backups.len(), 2);
        fs::write(&scx, "scx v2").unwrap();
        fs::write(&sct, "sct v2").unwrap();

        let restored = restore(dir.path(), &scx, None).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(fs::read_to_string(&sct).unwrap(), "sct v1");
    }

    #[test]
    fn restore_with_timestamp_prefix() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.prg");
        fs::write(&file, "v1").unwrap();
        let b = create(dir.path(), &file).unwrap().unwrap();
        fs::write(&file, "v2").unwrap();
        let ts = parse_timestamp(&b.file_name().unwrap().to_string_lossy(), "a.prg").unwrap();
        restore(dir.path(), &file, Some(&ts[..8])).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    }
}
