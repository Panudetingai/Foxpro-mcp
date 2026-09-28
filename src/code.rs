use crate::backup;
use crate::encoding::{self, UTF8};
use crate::error::{FoxProError, Result};
use crate::fsutil;
use regex::RegexBuilder;
use similar::TextDiff;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Upper bound for text returned by `read_code` without an explicit range.
pub const DEFAULT_MAX_READ_BYTES: usize = 256 * 1024;
/// Diffs longer than this are truncated in tool responses.
const MAX_DIFF_CHARS: usize = 32 * 1024;
/// Files larger than this are skipped by `search_code`.
const MAX_SEARCH_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MATCH_LINE_CHARS: usize = 400;

/// Directories never searched: VCS metadata, build output and our own state.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    backup::BACKUP_DIR,
    ".mcp-vfp",
    ".mcp",
    "target",
    "node_modules",
];

/// FoxPro and other binary file types that are never searched as text.
const BINARY_EXTENSIONS: &[&str] = &[
    "dbf", "cdx", "fpt", "idx", "ndx", "dbc", "dct", "dcx", "scx", "sct", "vcx", "vct", "frx",
    "frt", "lbx", "lbt", "mnx", "mnt", "pjx", "pjt", "fxp", "app", "exe", "dll", "ocx", "bmp",
    "ico", "cur", "jpg", "jpeg", "png", "gif", "zip", "7z", "pdf", "err", "bak", "tmp",
];

/// Source extensions that Visual FoxPro reads as ANSI text, not UTF-8.
const FOXPRO_SOURCE_EXTENSIONS: &[&str] = &["prg", "h", "mpr", "qpr", "spr", "fpw", "txt", "ini"];

#[derive(Debug, Clone)]
pub struct FileContent {
    pub path: PathBuf,
    pub text: String,
    pub encoding: String,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub truncated: bool,
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
    pub dry_run: bool,
    pub created: bool,
    pub encoding: String,
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    pub files_scanned: usize,
    pub files_skipped: usize,
}

#[derive(Debug, Clone, Default)]
pub struct PatchOptions {
    pub dry_run: bool,
    pub backup: bool,
    pub replace_all: bool,
}

#[derive(Debug, Clone)]
pub struct PatchResult {
    pub replacements: usize,
    pub backup_path: Option<PathBuf>,
    pub diff: String,
    pub dry_run: bool,
    pub line_endings_normalized: bool,
}

/// Read and decode a file. Optionally return only lines [start_line, end_line]
/// (1-based, inclusive). Without a range the text is capped at `max_bytes`.
pub fn read_file(
    path: &Path,
    start_line: Option<usize>,
    end_line: Option<usize>,
    encoding: Option<&str>,
    max_bytes: Option<usize>,
) -> Result<FileContent> {
    if let Some(label) = encoding {
        encoding::lookup(label)?;
    }
    let bytes = fs::read(path)?;
    let (encoding, text) = encoding::decode_bytes(&bytes, encoding);
    let total_lines = text.lines().count();
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_READ_BYTES);

    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(usize::MAX).min(total_lines.max(start));

    let mut out = String::new();
    let mut last = start.saturating_sub(1);
    let mut truncated = false;
    for (i, line) in text.lines().enumerate().skip(start - 1) {
        let line_no = i + 1;
        if line_no > end {
            break;
        }
        if out.len() + line.len() + 1 > max_bytes && line_no > start {
            truncated = true;
            break;
        }
        if line_no > start {
            out.push('\n');
        }
        if line.len() > max_bytes {
            // A single enormous line (minified/generated file): cut it.
            let mut cut = max_bytes;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&line[..cut]);
            last = line_no;
            truncated = true;
            break;
        }
        out.push_str(line);
        last = line_no;
    }

    Ok(FileContent {
        path: path.to_path_buf(),
        text: out,
        encoding,
        total_lines,
        start_line: start,
        end_line: last,
        truncated,
    })
}

fn is_foxpro_source(path: &Path) -> bool {
    FOXPRO_SOURCE_EXTENSIONS
        .iter()
        .any(|ext| fsutil::has_extension(path, ext))
}

/// Pick the encoding used to save `content` to `path`, preserving the
/// encoding of an existing file whenever it is unambiguous.
fn target_encoding(
    path: &Path,
    original: Option<&[u8]>,
    detected: Option<&str>,
    requested: Option<&str>,
    content: &str,
) -> Result<&'static encoding_rs::Encoding> {
    if let Some(label) = requested {
        return encoding::lookup(label);
    }
    let ascii_only = original.is_some_and(|b| b.is_ascii());
    match detected {
        // Pure-ASCII files decode as UTF-8 but carry no real encoding
        // information; VFP sources must stay ANSI.
        Some(label) if label == UTF8 && ascii_only && is_foxpro_source(path) => {
            Ok(encoding::ansi_for_text(content))
        }
        Some(label) => encoding::lookup(label),
        None if is_foxpro_source(path) => Ok(encoding::ansi_for_text(content)),
        None => Ok(encoding_rs::UTF_8),
    }
}

/// Write content to a file. If the file exists and backup is enabled, copy it
/// to `.mcp-backup` first. `backup_workspace` is the workspace root used for
/// the backup layout (None disables backups regardless of options).
pub fn write_file(
    path: &Path,
    content: &str,
    backup_workspace: Option<&Path>,
    options: WriteOptions,
) -> Result<WriteResult> {
    let original_bytes = if path.is_file() {
        Some(fs::read(path)?)
    } else if path.exists() {
        return Err(FoxProError::InvalidArgument(format!(
            "{} exists and is not a file",
            path.display()
        )));
    } else {
        None
    };
    let (old_encoding, old_text) = match &original_bytes {
        Some(b) => {
            let (e, t) = encoding::decode_bytes(b, None);
            (Some(e), t)
        }
        None => (None, String::new()),
    };

    let enc = target_encoding(
        path,
        original_bytes.as_deref(),
        old_encoding.as_deref(),
        options.encoding.as_deref(),
        content,
    )?;
    // Encode before anything else so an unrepresentable character never
    // results in a partially applied change.
    let bytes = encoding::encode_with(content, enc)?;
    let diff = make_diff(&old_text, content);
    let created = original_bytes.is_none();
    let encoding = enc.name().to_lowercase();

    if options.dry_run {
        return Ok(WriteResult {
            bytes_written: 0,
            backup_path: None,
            diff,
            dry_run: true,
            created,
            encoding,
        });
    }

    let backup_path = match backup_workspace {
        Some(ws) if options.backup => backup::create(ws, path)?,
        _ => None,
    };
    fsutil::atomic_write(path, &bytes)?;

    Ok(WriteResult {
        bytes_written: bytes.len(),
        backup_path,
        diff,
        dry_run: false,
        created,
        encoding,
    })
}

fn line_of_offset(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// Apply an exact-match patch. The match must be unique unless `replace_all`
/// is set. LF-only patch text is matched against CRLF files (VFP default).
pub fn apply_patch(
    path: &Path,
    old_text: &str,
    new_text: &str,
    backup_workspace: Option<&Path>,
    options: PatchOptions,
) -> Result<PatchResult> {
    if old_text.is_empty() {
        return Err(FoxProError::InvalidArgument(
            "old_text must not be empty".into(),
        ));
    }
    let bytes = fs::read(path)?;
    let (encoding, original) = encoding::decode_bytes(&bytes, None);

    let mut old = old_text.to_string();
    let mut new = new_text.to_string();
    let mut normalized = false;
    if !original.contains(&old)
        && original.contains("\r\n")
        && old.contains('\n')
        && !old.contains("\r\n")
    {
        let crlf_old = old.replace('\n', "\r\n");
        if original.contains(&crlf_old) {
            old = crlf_old;
            new = new.replace("\r\n", "\n").replace('\n', "\r\n");
            normalized = true;
        }
    }

    let positions: Vec<usize> = original.match_indices(&old).map(|(i, _)| i).collect();
    if positions.is_empty() {
        return Err(FoxProError::NotFound(format!(
            "Patch target not found in {} (exact match required, including whitespace)",
            path.display()
        )));
    }
    if positions.len() > 1 && !options.replace_all {
        let lines: Vec<String> = positions
            .iter()
            .take(20)
            .map(|&p| line_of_offset(&original, p).to_string())
            .collect();
        return Err(FoxProError::Conflict(format!(
            "old_text matches {} locations (lines {}); include more context to make it unique or set replace_all",
            positions.len(),
            lines.join(", ")
        )));
    }

    let replaced = if options.replace_all {
        original.replace(&old, &new)
    } else {
        original.replacen(&old, &new, 1)
    };
    let enc = encoding::lookup(&encoding)?;
    let out_bytes = encoding::encode_with(&replaced, enc)?;
    let diff = make_diff(&original, &replaced);
    let replacements = positions.len();

    if options.dry_run {
        return Ok(PatchResult {
            replacements,
            backup_path: None,
            diff,
            dry_run: true,
            line_endings_normalized: normalized,
        });
    }

    let backup_path = match backup_workspace {
        Some(ws) if options.backup => backup::create(ws, path)?,
        _ => None,
    };
    fsutil::atomic_write(path, &out_bytes)?;

    Ok(PatchResult {
        replacements,
        backup_path,
        diff,
        dry_run: false,
        line_endings_normalized: normalized,
    })
}

fn parse_extensions(file_type: Option<&str>) -> Vec<String> {
    file_type
        .map(|s| {
            s.split([',', ';', ' '])
                .map(|e| e.trim().trim_start_matches("*.").trim_start_matches('.'))
                .filter(|e| !e.is_empty())
                .map(|e| e.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &text[..idx]),
        None => text.to_string(),
    }
}

/// Search text files under `root` for `pattern`.
pub fn search_files(
    root: &Path,
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    file_type: Option<&str>,
    max_results: Option<usize>,
) -> Result<SearchResult> {
    if pattern.is_empty() {
        return Err(FoxProError::InvalidArgument(
            "pattern must not be empty".into(),
        ));
    }
    let max = max_results.unwrap_or(100).clamp(1, 5000);
    let source = if regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    let matcher = RegexBuilder::new(&source)
        .case_insensitive(case_insensitive)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| FoxProError::InvalidArgument(format!("Invalid regex: {e}")))?;
    let extensions = parse_extensions(file_type);

    let mut result = SearchResult {
        matches: Vec::new(),
        truncated: false,
        files_scanned: 0,
        files_skipped: 0,
    };

    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !(e.file_type().is_dir()
                    && SKIPPED_DIRS
                        .iter()
                        .any(|d| e.file_name().eq_ignore_ascii_case(d)))
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("search: skipping unreadable entry: {e}");
                result.files_skipped += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if !extensions.is_empty() {
            if !extensions.contains(&ext) {
                continue;
            }
        } else if BINARY_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }

        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_SEARCH_FILE_BYTES {
            result.files_skipped += 1;
            continue;
        }
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                result.files_skipped += 1;
                continue;
            }
        };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            result.files_skipped += 1;
            continue;
        }
        result.files_scanned += 1;

        let (_, text) = encoding::decode_bytes(&bytes, None);
        for (line_no, line) in text.lines().enumerate() {
            if let Some(m) = matcher.find(line) {
                result.matches.push(SearchMatch {
                    file: path.to_path_buf(),
                    line: line_no + 1,
                    column: line[..m.start()].chars().count() + 1,
                    text: truncate_chars(line.trim_end(), MAX_MATCH_LINE_CHARS),
                });
                if result.matches.len() >= max {
                    result.truncated = true;
                    return Ok(result);
                }
            }
        }
    }

    Ok(result)
}

pub fn make_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string();
    if diff.len() > MAX_DIFF_CHARS {
        let cut = truncate_chars(&diff, MAX_DIFF_CHARS);
        format!("{cut}\n… diff truncated ({} bytes total)", diff.len())
    } else {
        diff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn read_line_range() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.txt");
        fs::File::create(&file)
            .unwrap()
            .write_all(b"line1\nline2\nline3\nline4\n")
            .unwrap();

        let fc = read_file(&file, Some(2), Some(3), None, None).unwrap();
        assert_eq!(fc.text, "line2\nline3");
        assert_eq!(fc.total_lines, 4);
        assert_eq!((fc.start_line, fc.end_line), (2, 3));
    }

    #[test]
    fn read_is_capped() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("big.prg");
        fs::write(&file, "0123456789\n".repeat(100)).unwrap();
        let fc = read_file(&file, None, None, None, Some(50)).unwrap();
        assert!(fc.truncated);
        assert!(fc.text.len() <= 50);
        assert_eq!(fc.end_line, 4);
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.prg");
        let res = write_file(&file, "Hello FoxPro", None, WriteOptions::default()).unwrap();
        assert_eq!(res.bytes_written, 12);
        assert!(res.created);

        let fc = read_file(&file, None, None, None, None).unwrap();
        assert_eq!(fc.text, "Hello FoxPro");
    }

    #[test]
    fn new_prg_with_thai_is_written_as_windows_874() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("thai.prg");
        let res = write_file(&file, "? 'บันทึก'", None, WriteOptions::default()).unwrap();
        assert_eq!(res.encoding, "windows-874");
        let bytes = fs::read(&file).unwrap();
        assert!(std::str::from_utf8(&bytes).is_err());
    }

    #[test]
    fn unencodable_text_is_rejected_without_writing() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.prg");
        fs::write(&file, b"? 'caf\xe9'").unwrap(); // windows-1252
        let err = write_file(&file, "? 'สวัสดี'", None, WriteOptions::default()).unwrap_err();
        assert!(matches!(err, FoxProError::Encoding(_)));
        assert_eq!(fs::read(&file).unwrap(), b"? 'caf\xe9'");
    }

    #[test]
    fn apply_patch_exact_match() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.prg");
        fs::write(&file, "OLD TEXT HERE").unwrap();

        let res = apply_patch(&file, "OLD", "NEW", None, PatchOptions::default()).unwrap();
        assert_eq!(res.replacements, 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "NEW TEXT HERE");
    }

    #[test]
    fn apply_patch_rejects_ambiguous_and_empty() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.prg");
        fs::write(&file, "x = 1\nx = 1\n").unwrap();

        let err = apply_patch(&file, "x = 1", "x = 2", None, PatchOptions::default()).unwrap_err();
        assert!(matches!(err, FoxProError::Conflict(_)));
        assert!(apply_patch(&file, "", "y", None, PatchOptions::default()).is_err());

        let opts = PatchOptions {
            replace_all: true,
            ..Default::default()
        };
        let res = apply_patch(&file, "x = 1", "x = 2", None, opts).unwrap();
        assert_eq!(res.replacements, 2);
    }

    #[test]
    fn apply_patch_matches_crlf_files() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sample.prg");
        fs::write(&file, "IF .T.\r\n  ? 1\r\nENDIF\r\n").unwrap();

        let res = apply_patch(
            &file,
            "  ? 1\nENDIF",
            "  ? 2\nENDIF",
            None,
            PatchOptions::default(),
        )
        .unwrap();
        assert!(res.line_endings_normalized);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "IF .T.\r\n  ? 2\r\nENDIF\r\n"
        );
    }

    #[test]
    fn search_finds_literal_case_insensitive() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.prg"), "LOCAL x\nSTORE 1 TO x").unwrap();
        fs::write(dir.path().join("B.PRG"), "* local comment").unwrap();
        fs::write(dir.path().join("c.dbf"), "local\0binary").unwrap();

        let res = search_files(dir.path(), "local", false, true, Some(".prg"), None).unwrap();
        assert_eq!(res.matches.len(), 2);

        let res = search_files(dir.path(), "local", false, true, None, None).unwrap();
        assert_eq!(res.matches.len(), 2, "binary files are skipped");
    }
}
