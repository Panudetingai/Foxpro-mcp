use crate::error::{FoxProError, Result};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};

pub fn create(workspace: &Path, original: &Path) -> Result<Option<PathBuf>> {
    if !original.exists() {
        return Ok(None);
    }

    let rel = original
        .strip_prefix(workspace)
        .map_err(|_| FoxProError::SandboxViolation {
            workspace: workspace.to_path_buf(),
            requested: original.to_path_buf(),
        })?;

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let backup_dir = workspace
        .join(".mcp-backup")
        .join(rel.parent().unwrap_or(Path::new("")));
    let backup_name = format!(
        "{}.{timestamp}.bak",
        original
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
    );
    let backup_path = backup_dir.join(backup_name);

    fs::create_dir_all(&backup_dir)?;
    fs::copy(original, &backup_path)?;

    Ok(Some(backup_path))
}

pub fn list(workspace: &Path, original: &Path) -> Result<Vec<PathBuf>> {
    let rel = original
        .strip_prefix(workspace)
        .map_err(|_| FoxProError::SandboxViolation {
            workspace: workspace.to_path_buf(),
            requested: original.to_path_buf(),
        })?;
    let backup_dir = workspace
        .join(".mcp-backup")
        .join(rel.parent().unwrap_or(Path::new("")));

    if !backup_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(&backup_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();

    entries.sort_by_cached_key(|p| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    entries.reverse();
    Ok(entries)
}

pub fn restore(workspace: &Path, original: &Path, timestamp: Option<&str>) -> Result<PathBuf> {
    let backups = list(workspace, original)?;

    if backups.is_empty() {
        return Err(FoxProError::Config(format!(
            "No backups found for {}",
            original.display()
        )));
    }

    let chosen = if let Some(ts) = timestamp {
        backups
            .into_iter()
            .find(|p| p.to_string_lossy().contains(ts))
            .ok_or_else(|| FoxProError::Config(format!("No backup matching timestamp {ts}")))?
    } else {
        backups.into_iter().next().unwrap()
    };

    fs::copy(&chosen, original)?;
    Ok(chosen)
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
    }
}
