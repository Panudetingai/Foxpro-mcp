//! Visual FoxPro 9 runtime integration: run programs, build projects.
//!
//! Every invocation gets its own directory under `.mcp-vfp/run/` containing a
//! generated `runner.prg`, a `config.fpw` (passed through `FOXPROWCFG`) and
//! the captured output/error files. Concurrent runs therefore never share
//! state, and stale output from an earlier run can never be mistaken for the
//! result of the current one.

use crate::encoding;
use crate::error::{FoxProError, Result};
use crate::fsutil;
use regex::Regex;
use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = 3600;
/// Captured output returned to the agent is capped at this many bytes.
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
/// The error handler aborts the program after this many errors.
const MAX_RUNTIME_ERRORS: usize = 50;
/// Run directories older than this are removed (left over from crashes).
const STALE_RUN_DIR: Duration = Duration::from_secs(6 * 3600);

pub const STATE_DIR: &str = ".mcp-vfp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildType {
    Exe,
    App,
    Dll,
}

impl BuildType {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "exe" => Ok(BuildType::Exe),
            "app" => Ok(BuildType::App),
            "dll" => Ok(BuildType::Dll),
            t => Err(FoxProError::InvalidArgument(format!(
                "Unsupported build type: {t} (expected exe, app or dll)"
            ))),
        }
    }

    pub fn command(&self) -> &'static str {
        match self {
            BuildType::Exe => "BUILD EXE",
            BuildType::App => "BUILD APP",
            BuildType::Dll => "BUILD DLL",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            BuildType::Exe => "exe",
            BuildType::App => "app",
            BuildType::Dll => "dll",
        }
    }
}

/// An error trapped by the runner's ON ERROR handler.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RuntimeError {
    pub number: Option<i64>,
    pub message: String,
    pub program: String,
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunOutput {
    pub success: bool,
    pub dry_run: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub captured_output: String,
    pub output_truncated: bool,
    pub errors: Vec<RuntimeError>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    pub command: String,
    pub duration_ms: u128,
    /// Generated runner script (dry-run only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildError {
    pub file: String,
    pub line: Option<usize>,
    pub message: String,
    pub raw: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildOutput {
    pub success: bool,
    pub dry_run: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub errors: Vec<BuildError>,
    pub runtime_errors: Vec<RuntimeError>,
    pub output_file: PathBuf,
    pub output_exists: bool,
    pub captured_output: String,
    pub command: String,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VfpEngine {
    pub(crate) executable: PathBuf,
    workspace: PathBuf,
    timeout_secs: u64,
}

/// A prepared run directory. Removed when dropped.
pub(crate) struct RunDir {
    pub path: PathBuf,
    keep: bool,
}

impl RunDir {
    fn create(workspace: &Path) -> Result<Self> {
        let root = workspace.join(STATE_DIR).join("run");
        fs::create_dir_all(&root)?;
        prune_stale(&root);
        let path = root.join(fsutil::unique_suffix());
        fs::create_dir_all(&path)?;
        Ok(Self { path, keep: false })
    }

    /// Keep the directory after drop (used by long-lived UI instances).
    pub fn persist(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }

    pub fn output_file(&self) -> PathBuf {
        self.path.join("output.txt")
    }

    pub fn error_file(&self) -> PathBuf {
        self.path.join("error.txt")
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn prune_stale(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_RUN_DIR);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

impl VfpEngine {
    pub fn new(vfp_path: Option<PathBuf>, workspace: PathBuf, timeout_secs: u64) -> Result<Self> {
        let executable = resolve_vfp_executable(vfp_path.as_deref())?;
        Ok(Self {
            executable,
            workspace,
            timeout_secs: timeout_secs.clamp(1, MAX_TIMEOUT_SECS),
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    fn timeout_for(&self, requested: Option<u64>) -> u64 {
        requested
            .unwrap_or(self.timeout_secs)
            .clamp(1, MAX_TIMEOUT_SECS)
    }

    /// Run inline FoxPro code. The code is written to its own program file so
    /// it may contain PROCEDURE/FUNCTION definitions of its own.
    pub async fn run_code(
        &self,
        code: &str,
        timeout_secs: Option<u64>,
        dry_run: bool,
    ) -> Result<RunOutput> {
        let source = normalize_newlines(code);
        let user_bytes = encoding::encode_with(&source, encoding::ansi_for_text(&source))?;
        let placeholder = self.workspace.join(STATE_DIR).join("run").join("<run-id>");
        if dry_run {
            let script = runner_script(
                &self.workspace,
                &placeholder,
                &format!("DO {}", quote(&placeholder.join("user.prg"))),
                false,
            );
            return Ok(dry_run_output(format!("{script}\n* user.prg:\n{source}")));
        }
        let dir = RunDir::create(&self.workspace)?;
        let user_prg = dir.path.join("user.prg");
        fs::write(&user_prg, user_bytes)?;
        let body = format!("DO {}", quote(&user_prg));
        self.execute(&dir, &body, self.timeout_for(timeout_secs))
            .await
    }

    pub async fn run_file(
        &self,
        prg_path: &Path,
        timeout_secs: Option<u64>,
        dry_run: bool,
    ) -> Result<RunOutput> {
        let body = do_command(prg_path);
        if dry_run {
            let placeholder = self.workspace.join(STATE_DIR).join("run").join("<run-id>");
            return Ok(dry_run_output(runner_script(
                &self.workspace,
                &placeholder,
                &body,
                false,
            )));
        }
        let dir = RunDir::create(&self.workspace)?;
        self.execute(&dir, &body, self.timeout_for(timeout_secs))
            .await
    }

    pub async fn build(
        &self,
        project: &Path,
        build_type: BuildType,
        output: Option<&Path>,
        timeout_secs: Option<u64>,
        dry_run: bool,
    ) -> Result<BuildOutput> {
        if !fsutil::has_extension(project, "pjx") {
            return Err(FoxProError::InvalidArgument(format!(
                "{} is not a project (.pjx) file",
                project.display()
            )));
        }
        let output_path = output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project.with_extension(build_type.extension()));
        let err_file = project.with_extension("err");
        // RECOMPILE makes sure forms whose object code was cleared by
        // form/report tools are compiled again.
        let body = format!(
            "{} {} FROM {} RECOMPILE",
            build_type.command(),
            quote(&output_path),
            quote(project)
        );

        if dry_run {
            let placeholder = self.workspace.join(STATE_DIR).join("run").join("<run-id>");
            let out = dry_run_output(runner_script(&self.workspace, &placeholder, &body, false));
            return Ok(BuildOutput {
                success: false,
                dry_run: true,
                timed_out: false,
                exit_code: None,
                errors: Vec::new(),
                runtime_errors: Vec::new(),
                output_exists: output_path.exists(),
                output_file: output_path,
                captured_output: String::new(),
                command: String::new(),
                duration_ms: 0,
                script: out.script,
            });
        }

        // A stale .err file from an earlier build must not be reported again.
        if err_file.exists() {
            fs::remove_file(&err_file)?;
        }
        let started = SystemTime::now();
        let dir = RunDir::create(&self.workspace)?;
        let run = self
            .execute(&dir, &body, self.timeout_for(timeout_secs))
            .await?;
        let errors = parse_build_errors(&err_file);
        let output_fresh = fs::metadata(&output_path)
            .and_then(|m| m.modified())
            .is_ok_and(|m| m >= started - Duration::from_secs(2));

        Ok(BuildOutput {
            success: run.success && errors.is_empty() && output_fresh,
            dry_run: false,
            timed_out: run.timed_out,
            exit_code: run.exit_code,
            errors,
            runtime_errors: run.errors,
            output_exists: output_path.exists(),
            output_file: output_path,
            captured_output: run.captured_output,
            command: run.command,
            duration_ms: run.duration_ms,
            script: None,
        })
    }

    /// Write `runner.prg` and `config.fpw` into `dir` and build the command.
    /// VFP is started as `vfp9.exe -T runner.prg`; the config file is passed
    /// through the FOXPROWCFG environment variable, which avoids quoting
    /// problems with the `-C` switch.
    fn command_for(&self, dir: &RunDir, body: &str, interactive: bool) -> Result<Command> {
        let script = runner_script(&self.workspace, &dir.path, body, interactive);
        let runner = dir.path.join("runner.prg");
        fs::write(
            &runner,
            encoding::encode_with(&script, encoding::ansi_for_text(&script))?,
        )?;
        let config = dir.path.join("config.fpw");
        let config_text = if interactive {
            "RESOURCE = OFF\r\nTALK = OFF\r\n"
        } else {
            "SCREEN = OFF\r\nRESOURCE = OFF\r\nTALK = OFF\r\nSAFETY = OFF\r\n"
        };
        fs::write(&config, config_text)?;

        let mut cmd = Command::new(&self.executable);
        cmd.arg("-T")
            .arg(&runner)
            .env("FOXPROWCFG", &config)
            .current_dir(&self.workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        Ok(cmd)
    }

    /// Prepare a run directory with runner + config for a long-lived,
    /// interactive VFP process (UI verification).
    pub(crate) fn prepare_interactive(&self, body: &str) -> Result<(RunDir, Command)> {
        let dir = RunDir::create(&self.workspace)?;
        let cmd = self.command_for(&dir, body, true)?;
        Ok((dir, cmd))
    }

    async fn execute(&self, dir: &RunDir, body: &str, timeout_secs: u64) -> Result<RunOutput> {
        let mut cmd = self.command_for(dir, body, false)?;
        let command_display = format!(
            "{} -T {}",
            self.executable.display(),
            dir.path.join("runner.prg").display()
        );

        let started = std::time::Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| FoxProError::VfpNotConfigured(format!("Failed to start VFP: {e}")))?;

        // Drain pipes concurrently so a chatty process can never block on a
        // full pipe buffer.
        let stdout_task = tokio::spawn(read_pipe(child.stdout.take()));
        let stderr_task = tokio::spawn(read_pipe(child.stderr.take()));

        let (status, timed_out) =
            match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
                Ok(Ok(s)) => (Some(s), false),
                Ok(Err(e)) => return Err(FoxProError::Io(e)),
                Err(_) => {
                    kill_tree(&mut child).await;
                    (None, true)
                }
            };

        // Processes started by the program (RUN, ShellExecute) may inherit
        // the pipes and keep them open; never wait for them for long.
        let (stdout, stderr): (String, String) = timeout(Duration::from_secs(2), async {
            let out = stdout_task.await.unwrap_or_default();
            let err = stderr_task.await.unwrap_or_default();
            (out, err)
        })
        .await
        .unwrap_or_default();

        let (captured_output, output_truncated) = read_capped(&dir.output_file());
        let mut errors = parse_runtime_errors(&dir.error_file());
        if timed_out {
            errors.push(RuntimeError {
                number: None,
                message: format!(
                    "VFP did not finish within {timeout_secs} seconds and was terminated (a modal dialog, READ EVENTS or an endless loop can cause this)"
                ),
                program: String::new(),
                line: None,
            });
        }
        let exit_ok = status.as_ref().is_some_and(|s| s.success());

        Ok(RunOutput {
            success: exit_ok && errors.is_empty(),
            dry_run: false,
            timed_out,
            exit_code: status.and_then(|s| s.code()),
            captured_output,
            output_truncated,
            errors,
            stdout,
            stderr,
            command: command_display,
            duration_ms: started.elapsed().as_millis(),
            script: None,
        })
    }
}

/// Terminate a process and, on Windows, every process it started.
pub(crate) async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

fn dry_run_output(script: String) -> RunOutput {
    RunOutput {
        success: false,
        dry_run: true,
        timed_out: false,
        exit_code: None,
        captured_output: String::new(),
        output_truncated: false,
        errors: Vec::new(),
        stdout: String::new(),
        stderr: String::new(),
        command: String::new(),
        duration_ms: 0,
        script: Some(script),
    }
}

fn resolve_vfp_executable(path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = path {
        if p.is_file() {
            return Ok(dunce::canonicalize(p)?);
        }
        let name = p.to_string_lossy();
        if name.contains('/') || name.contains('\\') {
            return Err(FoxProError::VfpNotConfigured(format!(
                "VFP path does not exist: {name}"
            )));
        }
        return find_in_path(&name);
    }
    find_in_path("vfp9")
}

fn find_in_path(name: &str) -> Result<PathBuf> {
    let exe_name = if cfg!(windows) && !name.contains('.') {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    env::var_os("PATH")
        .and_then(|paths| {
            env::split_paths(&paths)
                .map(|dir| dir.join(&exe_name))
                .find(|p| p.is_file())
        })
        .ok_or_else(|| FoxProError::VfpNotConfigured(format!("{exe_name} not found in PATH")))
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

/// Quote a path as a FoxPro string literal, choosing a delimiter that does not
/// occur in the path.
pub fn quote(path: &Path) -> String {
    quote_str(&path.to_string_lossy())
}

pub fn quote_str(s: &str) -> String {
    if !s.contains('"') {
        format!("\"{s}\"")
    } else if !s.contains('\'') {
        format!("'{s}'")
    } else {
        format!("[{s}]")
    }
}

/// `DO <file>` for programs, `DO FORM <file>` for forms.
pub fn do_command(path: &Path) -> String {
    if fsutil::has_extension(path, "scx") {
        format!("DO FORM {}", quote(path))
    } else {
        format!("DO {}", quote(path))
    }
}

/// Generate the runner program. The error handler is a PROCEDURE and must be
/// the last thing in the file: VFP treats every line after a PROCEDURE
/// statement as part of that procedure.
pub(crate) fn runner_script(
    workspace: &Path,
    run_dir: &Path,
    body: &str,
    interactive: bool,
) -> String {
    let out_file = quote(&run_dir.join("output.txt"));
    let err_file = quote(&run_dir.join("error.txt"));
    let finish = if interactive {
        // Keep VFP (and any forms) open for UI verification.
        "SET ALTERNATE OFF\r\nSET ALTERNATE TO\r\nRETURN".to_string()
    } else {
        "SET ALTERNATE OFF\r\nSET ALTERNATE TO\r\nON ERROR\r\nQUIT".to_string()
    };
    let lines = [
        "* FoxPro MCP runner generated by foxpro-mcp".to_string(),
        "SET SAFETY OFF".into(),
        "SET TALK OFF".into(),
        "SET NOTIFY OFF".into(),
        "SET RESOURCE OFF".into(),
        "PUBLIC mcp_out, mcp_err, mcp_errcount".into(),
        format!("mcp_out = {out_file}"),
        format!("mcp_err = {err_file}"),
        "mcp_errcount = 0".into(),
        format!("SET DEFAULT TO {}", quote(workspace)),
        "SET ALTERNATE TO (mcp_out)".into(),
        "SET ALTERNATE ON".into(),
        "ON ERROR DO mcp_on_error WITH ERROR(), MESSAGE(), PROGRAM(), LINENO()".into(),
        normalize_newlines(body),
        finish,
        String::new(),
        "PROCEDURE mcp_on_error".into(),
        "LPARAMETERS nErr, cMsg, cProg, nLine".into(),
        "mcp_errcount = mcp_errcount + 1".into(),
        "STRTOFILE(TRANSFORM(nErr) + CHR(9) + CHRTRAN(cMsg, CHR(9)+CHR(13)+CHR(10), \"   \") + CHR(9) + cProg + CHR(9) + TRANSFORM(nLine) + CHR(13) + CHR(10), mcp_err, .T.)".into(),
        format!("IF mcp_errcount >= {MAX_RUNTIME_ERRORS}"),
        "  SET ALTERNATE OFF".into(),
        "  SET ALTERNATE TO".into(),
        "  QUIT".into(),
        "ENDIF".into(),
        "ENDPROC".into(),
        String::new(),
    ];
    lines.join("\r\n")
}

async fn read_pipe<T: AsyncReadExt + Unpin>(pipe: Option<T>) -> String {
    let Some(mut p) = pipe else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match p.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Keep draining past the cap so the child never blocks.
                if kept.len() < MAX_CAPTURE_BYTES {
                    let take = n.min(MAX_CAPTURE_BYTES - kept.len());
                    kept.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    encoding::decode_bytes(&kept, None).1
}

/// Read a text file written by VFP (ANSI), capped for agent-friendly output.
fn read_capped(path: &Path) -> (String, bool) {
    match fs::read(path) {
        Ok(bytes) => {
            let truncated = bytes.len() > MAX_CAPTURE_BYTES;
            let slice = if truncated {
                &bytes[bytes.len() - MAX_CAPTURE_BYTES..]
            } else {
                &bytes[..]
            };
            (encoding::decode_bytes(slice, None).1, truncated)
        }
        Err(_) => (String::new(), false),
    }
}

pub(crate) fn parse_runtime_errors(path: &Path) -> Vec<RuntimeError> {
    let (text, _) = read_capped(path);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 4 {
                RuntimeError {
                    number: parts[0].trim().parse().ok(),
                    message: parts[1].trim().to_string(),
                    program: parts[2].trim().to_string(),
                    line: parts[3].trim().parse().ok(),
                }
            } else {
                RuntimeError {
                    number: None,
                    message: line.trim().to_string(),
                    program: String::new(),
                    line: None,
                }
            }
        })
        .collect()
}

fn parse_build_errors(err_file: &Path) -> Vec<BuildError> {
    let (text, _) = read_capped(err_file);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(parse_error_line)
        .collect()
}

static LINE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+)\)").expect("valid regex"));

fn parse_error_line(line: &str) -> BuildError {
    let raw = line.to_string();
    if let Some(caps) = LINE_REF.captures(line)
        && let Some(m) = caps.get(0)
    {
        let line_no = caps.get(1).and_then(|n| n.as_str().parse().ok());
        return BuildError {
            file: line[..m.start()].trim().to_string(),
            line: line_no,
            message: line[m.end()..]
                .trim()
                .trim_start_matches(':')
                .trim()
                .to_string(),
            raw,
        };
    }
    BuildError {
        file: String::new(),
        line: None,
        message: raw.trim().to_string(),
        raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_engine(dir: &TempDir) -> VfpEngine {
        let fake_exe = dir.path().join("vfp9.exe");
        fs::File::create(&fake_exe).unwrap();
        VfpEngine::new(Some(fake_exe), dir.path().to_path_buf(), 30).unwrap()
    }

    #[test]
    fn resolve_from_explicit_file() {
        let dir = TempDir::new().unwrap();
        let fake = dir.path().join("vfp9.exe");
        fs::File::create(&fake).unwrap();
        let engine = VfpEngine::new(Some(fake.clone()), dir.path().to_path_buf(), 30).unwrap();
        assert_eq!(engine.executable, dunce::canonicalize(&fake).unwrap());
    }

    #[test]
    fn error_handler_procedure_is_last() {
        let dir = TempDir::new().unwrap();
        let script = runner_script(dir.path(), dir.path(), "? 42", false);
        let body = script.find("? 42").unwrap();
        let quit = script.find("QUIT").unwrap();
        let procedure = script.find("PROCEDURE mcp_on_error").unwrap();
        assert!(body < quit && quit < procedure);
        assert!(script.trim_end().ends_with("ENDPROC"));
    }

    #[test]
    fn quote_picks_safe_delimiter() {
        assert_eq!(quote_str("a b"), "\"a b\"");
        assert_eq!(quote_str("say \"hi\""), "'say \"hi\"'");
        assert_eq!(quote_str("it's \"x\""), "[it's \"x\"]");
    }

    #[test]
    fn do_command_uses_do_form_for_forms() {
        assert!(do_command(Path::new("a.scx")).starts_with("DO FORM "));
        assert!(do_command(Path::new("a.prg")).starts_with("DO "));
    }

    #[test]
    fn parses_trapped_runtime_errors() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("error.txt");
        fs::write(&f, "12\tVariable 'X' is not found.\tMAIN\t3\r\n").unwrap();
        let errors = parse_runtime_errors(&f);
        assert_eq!(
            errors,
            vec![RuntimeError {
                number: Some(12),
                message: "Variable 'X' is not found.".into(),
                program: "MAIN".into(),
                line: Some(3),
            }]
        );
    }

    #[test]
    fn parses_build_error_lines() {
        let e = parse_error_line(r"c:\app\main.prg(12): Unrecognized command verb.");
        assert_eq!(e.file, r"c:\app\main.prg");
        assert_eq!(e.line, Some(12));
        assert_eq!(e.message, "Unrecognized command verb.");
    }

    #[tokio::test]
    async fn dry_run_does_not_create_run_directory() {
        let dir = TempDir::new().unwrap();
        let engine = fake_engine(&dir);
        let out = engine.run_code("? 1", None, true).await.unwrap();
        assert!(out.dry_run);
        assert!(out.script.unwrap().contains("? 1"));
        assert!(!dir.path().join(STATE_DIR).exists());
    }

    #[tokio::test]
    async fn build_rejects_non_project_files() {
        let dir = TempDir::new().unwrap();
        let engine = fake_engine(&dir);
        let err = engine
            .build(&dir.path().join("x.prg"), BuildType::Exe, None, None, true)
            .await
            .unwrap_err();
        assert!(matches!(err, FoxProError::InvalidArgument(_)));
    }

    #[test]
    fn run_dir_is_removed_on_drop() {
        let dir = TempDir::new().unwrap();
        let run = RunDir::create(dir.path()).unwrap();
        let path = run.path.clone();
        assert!(path.exists());
        drop(run);
        assert!(!path.exists());
    }
}
