//! Filesystem helpers that keep files consistent if the process dies mid-write.

use crate::error::Result;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A process-unique suffix for temporary names.
pub fn unique_suffix() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}-{n}", std::process::id())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!(".{name}.{}.tmp", unique_suffix()))
}

/// Write `bytes` to a temporary file next to `path`, flush it to disk and
/// rename it over the destination. Readers never observe a half-written file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_path_for(path);
    let result = (|| -> Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Atomically replace several files. All contents are staged first so a
/// failure while staging leaves every destination untouched.
pub fn atomic_write_all(files: &[(&Path, &[u8])]) -> Result<()> {
    let mut staged: Vec<(PathBuf, &Path)> = Vec::with_capacity(files.len());
    let stage = (|| -> Result<()> {
        for (path, bytes) in files {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let tmp = temp_path_for(path);
            let mut file = fs::File::create(&tmp)?;
            staged.push((tmp.clone(), path));
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = stage {
        for (tmp, _) in &staged {
            let _ = fs::remove_file(tmp);
        }
        return Err(e);
    }
    for (tmp, dest) in &staged {
        fs::rename(tmp, dest)?;
    }
    Ok(())
}

/// Path relative to `base` using forward-compatible display, or the full path
/// when it is not below `base`.
pub fn display_relative(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// Case-insensitive extension check (FoxPro projects often use upper-case names).
pub fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext.trim_start_matches('.')))
}

/// Find a companion file (e.g. `.sct` for `.scx`) regardless of the case of
/// its extension. Returns the lower-case variant when none exists yet, or the
/// upper-case variant when the primary file uses an upper-case extension.
pub fn companion(path: &Path, ext: &str) -> PathBuf {
    let lower = path.with_extension(ext.to_ascii_lowercase());
    let upper = path.with_extension(ext.to_ascii_uppercase());
    if lower.exists() {
        return lower;
    }
    if upper.exists() {
        return upper;
    }
    let primary_upper = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.chars().all(|c| !c.is_ascii_lowercase()));
    if primary_upper { upper } else { lower }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_replaces_content() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        atomic_write(&file, b"one").unwrap();
        atomic_write(&file, b"two").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        let leftovers = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn atomic_write_all_writes_every_file() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.scx");
        let b = dir.path().join("a.sct");
        atomic_write_all(&[(&a, b"x"), (&b, b"y")]).unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"x");
        assert_eq!(fs::read(&b).unwrap(), b"y");
    }
}
