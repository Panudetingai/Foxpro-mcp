use crate::code::{decode_bytes, encode_string};
use crate::error::{FoxProError, Result};
use regex::RegexBuilder;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildType {
    Exe,
    App,
    Dll,
}

impl BuildType {
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

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub output_file: PathBuf,
    pub captured_output: String,
    pub command: String,
}

#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub errors: Vec<BuildError>,
    pub output_file: PathBuf,
    pub command: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildError {
    pub file: String,
    pub line: Option<usize>,
    pub message: String,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct VfpEngine {
    pub(crate) executable: PathBuf,
    workspace: PathBuf,
    timeout_secs: u64,
}

impl VfpEngine {
    pub fn new(vfp_path: Option<PathBuf>, workspace: PathBuf, timeout_secs: u64) -> Result<Self> {
        let executable = resolve_vfp_executable(vfp_path.as_deref())?;
        Ok(Self {
            executable,
            workspace,
            timeout_secs,
        })
    }

    pub async fn run_code(
        &self,
        code: &str,
        timeout_secs: Option<u64>,
        dry_run: bool,
    ) -> Result<RunOutput> {
        let timeout = timeout_secs.unwrap_or(self.timeout_secs);
        let run_dir = run_dir(&self.workspace);
        let script = run_code_script(code, &run_dir);

        if dry_run {
            return Ok(RunOutput {
                success: false,
                exit_code: None,
                stdout: script,
                stderr: String::new(),
                output_file: run_dir.join("output.txt"),
                captured_output: String::new(),
                command: String::new(),
            });
        }

        fs::create_dir_all(&run_dir)?;
        let runner_path = write_runner(&script, &run_dir)?;
        self.execute(&runner_path, timeout).await
    }

    pub async fn run_file(
        &self,
        prg_path: &Path,
        timeout_secs: Option<u64>,
        dry_run: bool,
    ) -> Result<RunOutput> {
        let timeout = timeout_secs.unwrap_or(self.timeout_secs);
        let run_dir = run_dir(&self.workspace);
        let script = run_file_script(prg_path, &run_dir);

        if dry_run {
            return Ok(RunOutput {
                success: false,
                exit_code: None,
                stdout: script,
                stderr: String::new(),
                output_file: run_dir.join("output.txt"),
                captured_output: String::new(),
                command: String::new(),
            });
        }

        fs::create_dir_all(&run_dir)?;
        let runner_path = write_runner(&script, &run_dir)?;
        self.execute(&runner_path, timeout).await
    }

    pub async fn build(
        &self,
        project: &Path,
        build_type: BuildType,
        output: Option<&Path>,
        timeout_secs: Option<u64>,
        dry_run: bool,
    ) -> Result<BuildOutput> {
        let timeout = timeout_secs.unwrap_or(self.timeout_secs);
        let run_dir = run_dir(&self.workspace);
        let output_path = output
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| project.with_extension(build_type.extension()));
        let script = build_script(project, build_type, &output_path, &run_dir);

        if dry_run {
            return Ok(BuildOutput {
                success: false,
                exit_code: None,
                stdout: script,
                stderr: String::new(),
                errors: Vec::new(),
                output_file: output_path,
                command: String::new(),
                dry_run: true,
            });
        }

        fs::create_dir_all(&run_dir)?;
        let runner_path = write_runner(&script, &run_dir)?;
        let run_output = self.execute(&runner_path, timeout).await?;
        let err_file = project.with_extension("err");
        let errors = parse_build_errors(&run_output.captured_output, &err_file)?;
        let success = run_output.success && errors.is_empty();

        Ok(BuildOutput {
            success,
            exit_code: run_output.exit_code,
            stdout: run_output.stdout,
            stderr: run_output.stderr,
            errors,
            output_file: output_path,
            command: run_output.command,
            dry_run: false,
        })
    }

    async fn execute(&self, runner_path: &Path, timeout_secs: u64) -> Result<RunOutput> {
        let command_str = format!(r#"DO "{}""#, escape_path(runner_path));
        let command_display = format!("{} -c {}", self.executable.to_string_lossy(), command_str);

        let mut child = Command::new(&self.executable)
            .arg("-c")
            .arg(&command_str)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| FoxProError::VfpNotConfigured(format!("Failed to start VFP: {e}")))?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_fut = read_pipe(stdout_pipe);
        let stderr_fut = read_pipe(stderr_pipe);

        let status = match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(Ok(s)) => Some(s),
            Ok(Err(e)) => return Err(FoxProError::Io(e)),
            Err(_) => {
                let _ = child.kill().await;
                return Err(FoxProError::Timeout(timeout_secs));
            }
        };

        let stdout = stdout_fut.await;
        let stderr = stderr_fut.await;

        let run_dir = run_dir(&self.workspace);
        let output_file = run_dir.join("output.txt");
        let captured_output = read_text_file(&output_file);
        let success = status.as_ref().map(|s| s.success()).unwrap_or(false);
        let exit_code = status.and_then(|s| s.code());

        Ok(RunOutput {
            success,
            exit_code,
            stdout,
            stderr,
            output_file,
            captured_output,
            command: command_display,
        })
    }
}

fn resolve_vfp_executable(path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = path {
        if p.exists() {
            return Ok(p.canonicalize()?);
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

fn run_dir(workspace: &Path) -> PathBuf {
    workspace.join(".mcp-vfp").join("run")
}

fn write_runner(script: &str, run_dir: &Path) -> Result<PathBuf> {
    let runner_path = run_dir.join("runner.prg");
    let bytes = encode_vfp_source(script);
    fs::write(&runner_path, &bytes)?;
    Ok(runner_path)
}

fn encode_vfp_source(text: &str) -> Vec<u8> {
    let has_thai = text.chars().any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}'));
    let label = if has_thai {
        "windows-874"
    } else {
        "windows-1252"
    };
    encode_string(text, label).unwrap_or_else(|_| text.as_bytes().to_vec())
}

fn read_text_file(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => decode_bytes(&bytes, None).1,
        Err(_) => String::new(),
    }
}

async fn read_pipe<T: AsyncReadExt + Unpin>(pipe: Option<T>) -> String {
    match pipe {
        Some(mut p) => {
            let mut buf = Vec::new();
            if p.read_to_end(&mut buf).await.is_ok() {
                decode_bytes(&buf, None).1
            } else {
                String::new()
            }
        }
        None => String::new(),
    }
}

fn escape_path(path: &Path) -> String {
    path.to_string_lossy().replace('"', "\"\"")
}

fn wrap(user_code: &str, run_dir: &Path) -> String {
    let out_file = escape_path(&run_dir.join("output.txt"));
    let err_file = escape_path(&run_dir.join("error.txt"));
    format!(
        "* FoxPro MCP runner generated by foxpro-mcp\n\
         SET SAFETY OFF\n\
         SET TALK OFF\n\
         SET ECHO OFF\n\
         SET STATUS BAR OFF\n\
         SET NOTIFY OFF\n\
         PRIVATE m.mcp_out, m.mcp_err\n\
         m.mcp_out = \"{}\"\n\
         m.mcp_err = \"{}\"\n\
         SET ALTERNATE TO (m.mcp_out)\n\
         SET ALTERNATE ON\n\
         ON ERROR DO _mcp_on_error WITH ERROR(), MESSAGE(), PROGRAM(), LINENO()\n\
         PROCEDURE _mcp_on_error\n\
         PARAMETERS nErr, cMsg, cProg, nLine\n\
         LOCAL cLine\n\
         cLine = TRANSFORM(nErr) + \" \" + cMsg + \" in \" + cProg + \" line \" + TRANSFORM(nLine) + CHR(13) + CHR(10)\n\
         STRTOFILE(cLine, m.mcp_err, .T.)\n\
         RETURN\n\n\
         {}\n\n\
         SET ALTERNATE OFF\n\
         SET ALTERNATE TO\n\
         QUIT\n",
        out_file, err_file, user_code
    )
}

fn run_code_script(code: &str, run_dir: &Path) -> String {
    wrap(code, run_dir)
}

fn run_file_script(prg_path: &Path, run_dir: &Path) -> String {
    let code = format!(r#"DO "{}""#, escape_path(prg_path));
    wrap(&code, run_dir)
}

fn build_script(project: &Path, build_type: BuildType, output: &Path, run_dir: &Path) -> String {
    let project_stem = project.with_extension("");
    let err_file = escape_path(&project.with_extension("err"));
    let build_command = format!(
        r#"{} "{}" FROM "{}""#,
        build_type.command(),
        escape_path(output),
        escape_path(&project_stem)
    );
    let capture = format!(
        "IF FILE(\"{}\")\n\
         ? \"-----BUILD ERRORS-----\"\n\
         TYPE \"{}\"\n\
         ? \"-----END BUILD ERRORS-----\"\n\
         ENDIF\n",
        err_file, err_file
    );
    wrap(&format!("{}\n{}", build_command, capture), run_dir)
}

fn parse_build_errors(output: &str, err_file: &Path) -> Result<Vec<BuildError>> {
    let mut errors = Vec::new();
    let mut in_block = false;
    for line in output.lines() {
        if line.contains("-----BUILD ERRORS-----") {
            in_block = true;
            continue;
        }
        if line.contains("-----END BUILD ERRORS-----") {
            in_block = false;
            continue;
        }
        if in_block && !line.trim().is_empty() {
            errors.push(parse_error_line(line));
        }
    }

    if errors.is_empty() && err_file.exists() {
        let text = read_text_file(err_file);
        for line in text.lines() {
            if !line.trim().is_empty() {
                errors.push(parse_error_line(line));
            }
        }
    }

    Ok(errors)
}

fn parse_error_line(line: &str) -> BuildError {
    let raw = line.to_string();

    let line_regex = RegexBuilder::new(r"\((\d+)\)")
        .build()
        .expect("valid regex");

    if let Some(m) = line_regex.find(line) {
        let line_no: usize = line[m.start() + 1..m.end() - 1].parse().unwrap_or(0);
        let file = line[..m.start()].trim().to_string();
        let message = line[m.end()..]
            .trim()
            .trim_start_matches(':')
            .trim()
            .to_string();
        BuildError {
            file,
            line: Some(line_no),
            message,
            raw,
        }
    } else {
        BuildError {
            file: String::new(),
            line: None,
            message: raw.clone(),
            raw,
        }
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
        assert_eq!(engine.executable, fake.canonicalize().unwrap());
    }

    #[test]
    fn run_code_script_contains_user_code() {
        let dir = TempDir::new().unwrap();
        let run_dir = run_dir(dir.path());
        let script = run_code_script("? 42", &run_dir);
        assert!(script.contains("? 42"));
        assert!(script.contains("SET ALTERNATE TO"));
    }

    #[test]
    fn run_file_script_references_target() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("test.prg");
        let run_dir = run_dir(dir.path());
        let script = run_file_script(&target, &run_dir);
        assert!(script.contains(&format!(r#"DO "{}""#, escape_path(&target))));
    }

    #[test]
    fn build_script_contains_build_command() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("myapp.pjx");
        let output = dir.path().join("myapp.exe");
        let run_dir = run_dir(dir.path());
        let script = build_script(&project, BuildType::Exe, &output, &run_dir);
        assert!(script.contains("BUILD EXE"));
        assert!(script.contains("myapp"));
    }

    #[tokio::test]
    async fn dry_run_does_not_create_runner_file() {
        let dir = TempDir::new().unwrap();
        let engine = fake_engine(&dir);
        let out = engine.run_code("? 1", None, true).await.unwrap();
        assert!(!out.success);
        assert!(out.stdout.contains("? 1"));
        assert!(!run_dir(dir.path()).exists());
    }
}
