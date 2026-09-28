use crate::error::{FoxProError, Result};
use regex::RegexBuilder;
use similar::TextDiff;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct FileContent {
    pub path: PathBuf,
    pub text: String,
    pub encoding: String,
}

#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    pub dry_run: bool,
    pub backup: bool,
    pub encoding: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WriteResult {
    pub bytes_written: usize,
    pub backup_path: Option<PathBuf>,
    pub diff: String,
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct PatchResult {
    pub replacements: usize,
    pub backup_path: Option<PathBuf>,
    pub diff: String,
}

/// Read and decode a file. Optionally return only lines [start_line, end_line] (1-based, inclusive).
pub fn read_file(path: &Path, start_line: Option<usize>, end_line: Option<usize>) -> Result<FileContent> {
    let bytes = fs::read(path)?;
    let (encoding, text) = decode_bytes(&bytes, None);

    let text = match (start_line, end_line) {
        (None, None) => text,
        _ => {
            let start = start_line.unwrap_or(1).saturating_sub(1);
            let end = end_line.map(|n| n.saturating_sub(1)).unwrap_or(usize::MAX);
            text.lines()
                .enumerate()
                .filter(|(i, _)| *i >= start && *i <= end)
                .map(|(_, line)| line)
                .collect::<Vec<_>>()
                .join("\n")
        }
    };

    Ok(FileContent {
        path: path.to_path_buf(),
        text,
        encoding,
    })
}

/// Write content to a file. If the file exists and backup is enabled, copy it to `.mcp-backup` first.
pub fn write_file(
    path: &Path,
    content: &str,
    workspace: &Path,
    options: WriteOptions,
    backup_fn: &dyn Fn(&Path, &Path) -> Result<Option<PathBuf>>,
) -> Result<WriteResult> {
    let original_bytes = if path.exists() { Some(fs::read(path)?) } else { None };
    let (old_encoding, old_text) = original_bytes
        .as_ref()
        .map(|b| decode_bytes(b, options.encoding.as_deref()))
        .map(|(e, t)| (Some(e), t))
        .unwrap_or((None, String::new()));

    let diff = make_diff(&old_text, content);
    let encoding = options.encoding.or(old_encoding).unwrap_or_else(|| "utf-8".to_string());

    if options.dry_run {
        return Ok(WriteResult {
            bytes_written: 0,
            backup_path: None,
            diff,
        });
    }

    let backup_path = if options.backup {
        backup_fn(path, workspace)?
    } else {
        None
    };

    let bytes = encode_string(content, &encoding)?;
    fs::create_dir_all(path.parent().unwrap_or(Path::new("")))?;
    fs::write(path, &bytes)?;

    Ok(WriteResult {
        bytes_written: bytes.len(),
        backup_path,
        diff,
    })
}

/// Apply an exact-match patch. Replaces the first occurrence of `old_text` with `new_text`.
pub fn apply_patch(
    path: &Path,
    old_text: &str,
    new_text: &str,
    workspace: &Path,
    dry_run: bool,
    backup: bool,
    backup_fn: &dyn Fn(&Path, &Path) -> Result<Option<PathBuf>>,
) -> Result<PatchResult> {
    let content = read_file(path, None, None)?;
    let original = content.text;

    if !original.contains(old_text) {
        return Err(FoxProError::Config(format!(
            "Patch target not found in {}",
            path.display()
        )));
    }

    let replaced = original.replacen(old_text, new_text, 1);
    let diff = make_diff(&original, &replaced);

    if dry_run {
        return Ok(PatchResult {
            replacements: 1,
            backup_path: None,
            diff,
        });
    }

    let backup_path = if backup { backup_fn(path, workspace)? } else { None };
    let bytes = encode_string(&replaced, &content.encoding)?;
    fs::write(path, &bytes)?;

    Ok(PatchResult {
        replacements: 1,
        backup_path,
        diff,
    })
}

/// Search files under `workspace` for `pattern`.
pub fn search_files(
    workspace: &Path,
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    file_type: Option<&str>,
    max_results: Option<usize>,
) -> Result<Vec<SearchMatch>> {
    let max = max_results.unwrap_or(100);
    let matcher = if regex {
        RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| FoxProError::Config(format!("Invalid regex: {e}")))?
    } else {
        let escaped = regex::escape(pattern);
        RegexBuilder::new(&escaped)
            .case_insensitive(case_insensitive)
            .build()
            .expect("escaped literal is valid regex")
    };

    let mut matches = Vec::new();

    for entry in WalkDir::new(workspace)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_backup_dir(e.path(), workspace))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        if let Some(ext) = file_type {
            let file_ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or("");
            if file_ext != ext {
                continue;
            }
        }

        let bytes = match fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let (_, text) = decode_bytes(&bytes, None);
        for (line_no, line) in text.lines().enumerate() {
            if let Some(m) = matcher.find(line) {
                matches.push(SearchMatch {
                    file: entry.path().to_path_buf(),
                    line: line_no + 1,
                    column: m.start() + 1,
                    text: line.to_string(),
                });
                if matches.len() >= max {
                    return Ok(matches);
                }
            }
        }
    }

    Ok(matches)
}

fn make_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff().context_radius(3).to_string()
}

fn decode_bytes(bytes: &[u8], preferred: Option<&str>) -> (String, String) {
    if let Some(label) = preferred {
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (text, _) = enc.decode_without_bom_handling(bytes);
            return (label.to_lowercase(), text.into_owned());
        }
    }

    // Try UTF-8 first.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return ("utf-8".to_string(), text.to_string());
    }

    let win1252_invalid = count_invalid_windows1252(bytes);
    let win874_invalid = count_invalid_windows874(bytes);

    let (enc, label) = if win874_invalid < win1252_invalid {
        (encoding_rs::WINDOWS_874, "windows-874")
    } else if win1252_invalid < win874_invalid {
        (encoding_rs::WINDOWS_1252, "windows-1252")
    } else {
        // Tie-break by looking for Thai characters in the Windows-874 decode.
        let (thai, _) = encoding_rs::WINDOWS_874.decode_without_bom_handling(bytes);
        if thai.chars().any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}')) {
            (encoding_rs::WINDOWS_874, "windows-874")
        } else {
            (encoding_rs::WINDOWS_1252, "windows-1252")
        }
    };

    let (text, _) = enc.decode_without_bom_handling(bytes);
    (label.to_string(), text.into_owned())
}

fn encode_string(text: &str, encoding: &str) -> Result<Vec<u8>> {
    let enc = encoding_rs::Encoding::for_label(encoding.as_bytes())
        .ok_or_else(|| FoxProError::Config(format!("Unsupported encoding: {encoding}")))?;
    let (bytes, _, _) = enc.encode(text);
    Ok(bytes.into_owned())
}

fn count_invalid_windows1252(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .filter(|&&b| matches!(b, 0x81 | 0x8D | 0x8F | 0x90 | 0x9D))
        .count()
}

fn count_invalid_windows874(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .filter(|&&b| matches!(b, 0x80 | 0x85 | 0x91..=0x97))
        .count()
}

fn is_backup_dir(path: &Path, workspace: &Path) -> bool {
    path.strip_prefix(workspace)
        .ok()
        .map(|p| p.components().any(|c| c.as_os_str() == ".mcp-backup"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn no_backup(_: &Path, _: &Path) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    #[test]
    fn read_line_range() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.txt");
        fs::File::create(&file)
            .unwrap()
            .write_all(b"line1\nline2\nline3\nline4\n")
            .unwrap();

        let fc = read_file(&file, Some(2), Some(3)).unwrap();
        assert_eq!(fc.text, "line2\nline3");
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.prg");
        let opts = WriteOptions::default();
        let res = write_file(&file, "Hello FoxPro", dir.path(), opts, &no_backup).unwrap();
        assert_eq!(res.bytes_written, 12);

        let fc = read_file(&file, None, None).unwrap();
        assert_eq!(fc.text, "Hello FoxPro");
    }

    #[test]
    fn apply_patch_exact_match() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.prg");
        fs::write(&file, "OLD TEXT HERE").unwrap();

        let res = apply_patch(&file, "OLD", "NEW", dir.path(), false, false, &no_backup).unwrap();
        assert_eq!(res.replacements, 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "NEW TEXT HERE");
    }

    #[test]
    fn search_finds_literal_case_insensitive() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.prg"), "LOCAL x\nSTORE 1 TO x").unwrap();
        fs::write(dir.path().join("b.prg"), "* local comment").unwrap();

        let matches = search_files(dir.path(), "local", false, true, Some("prg"), None).unwrap();
        assert_eq!(matches.len(), 2);
    }
}
