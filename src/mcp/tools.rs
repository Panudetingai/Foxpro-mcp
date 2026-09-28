use crate::backup;
use crate::code;
use crate::code::{FileContent, PatchResult, WriteOptions, WriteResult};
use crate::config::Config;
use crate::error::{FoxProError, Result};
use crate::sandbox::Sandbox;
use crate::vfp::{BuildOutput, BuildType, RunOutput, VfpEngine};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

pub fn all() -> Vec<Tool> {
    vec![
        status(),
        get_workspace(),
        read_code(),
        write_code(),
        search_code(),
        apply_patch(),
        rollback(),
        run(),
        build(),
        test(),
    ]
}

pub struct ToolContext<'a> {
    pub config: &'a Config,
    pub sandbox: &'a Sandbox,
    pub vfp_engine: Option<&'a VfpEngine>,
}

pub async fn call(name: &str, arguments: Option<&Value>, ctx: ToolContext<'_>) -> Result<Value> {
    let args = arguments.ok_or_else(|| FoxProError::Rpc("missing arguments".to_string()))?;
    let args = args
        .as_object()
        .ok_or_else(|| FoxProError::Rpc("arguments must be an object".to_string()))?;

    match name {
        "foxpro.status" => status_handler(ctx),
        "foxpro.get_workspace" => get_workspace_handler(ctx),
        "foxpro.read_code" => read_code_handler(args, ctx),
        "foxpro.write_code" => write_code_handler(args, ctx),
        "foxpro.search_code" => search_code_handler(args, ctx),
        "foxpro.apply_patch" => apply_patch_handler(args, ctx),
        "foxpro.rollback" => rollback_handler(args, ctx),
        "foxpro.run" => run_handler(args, ctx).await,
        "foxpro.build" => build_handler(args, ctx).await,
        "foxpro.test" => test_handler(args, ctx).await,
        _ => Err(FoxProError::Rpc(format!("Unknown tool: {name}"))),
    }
}

fn status_handler(ctx: ToolContext<'_>) -> Result<Value> {
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "workspace": ctx.config.workspace.to_string_lossy(),
        "log_level": ctx.config.log_level,
    }))
}

fn get_workspace_handler(ctx: ToolContext<'_>) -> Result<Value> {
    Ok(json!({
        "workspace": ctx.sandbox.workspace().to_string_lossy()
    }))
}

fn read_code_handler(args: &serde_json::Map<String, Value>, ctx: ToolContext<'_>) -> Result<Value> {
    let raw_path = required_str(args, "path")?;
    let path = ctx.sandbox.validate(Path::new(raw_path))?;
    let start = get_usize(args, "start_line");
    let end = get_usize(args, "end_line");

    let fc = code::read_file(&path, start, end)?;
    Ok(file_content_value(fc))
}

fn write_code_handler(
    args: &serde_json::Map<String, Value>,
    ctx: ToolContext<'_>,
) -> Result<Value> {
    let raw_path = required_str(args, "path")?;
    let path = ctx.sandbox.resolve(Path::new(raw_path))?;
    let content = required_str(args, "content")?;
    let options = WriteOptions {
        dry_run: get_bool(args, "dry_run").unwrap_or(false),
        backup: get_bool(args, "backup").unwrap_or(true),
        encoding: get_str(args, "encoding").map(|s| s.to_string()),
    };

    let result = code::write_file(
        &path,
        content,
        ctx.sandbox.workspace(),
        options,
        &|original, workspace| backup::create(workspace, original),
    )?;
    Ok(write_result_value(result))
}

fn search_code_handler(
    args: &serde_json::Map<String, Value>,
    ctx: ToolContext<'_>,
) -> Result<Value> {
    let raw_path = get_str(args, "path").unwrap_or(".");
    let search_root = ctx.sandbox.validate(Path::new(raw_path))?;
    if !search_root.is_dir() {
        return Err(FoxProError::Config(format!(
            "{} is not a directory",
            search_root.display()
        )));
    }

    let pattern = required_str(args, "pattern")?;
    let regex = get_bool(args, "regex").unwrap_or(false);
    let case_insensitive = get_bool(args, "case_insensitive").unwrap_or(true);
    let file_type = get_str(args, "file_type");
    let max_results = get_usize(args, "max_results");

    let matches = code::search_files(
        &search_root,
        pattern,
        regex,
        case_insensitive,
        file_type,
        max_results,
    )?;
    Ok(json!(
        matches
            .iter()
            .map(|m| json!({
                "file": m.file.to_string_lossy(),
                "line": m.line,
                "column": m.column,
                "text": m.text,
            }))
            .collect::<Vec<_>>()
    ))
}

fn apply_patch_handler(
    args: &serde_json::Map<String, Value>,
    ctx: ToolContext<'_>,
) -> Result<Value> {
    let raw_path = required_str(args, "path")?;
    let path = ctx.sandbox.validate(Path::new(raw_path))?;
    let old_text = required_str(args, "old_text")?;
    let new_text = required_str(args, "new_text")?;
    let dry_run = get_bool(args, "dry_run").unwrap_or(false);
    let backup = get_bool(args, "backup").unwrap_or(true);

    let result = code::apply_patch(
        &path,
        old_text,
        new_text,
        ctx.sandbox.workspace(),
        dry_run,
        backup,
        &|original, workspace| backup::create(workspace, original),
    )?;
    Ok(patch_result_value(result))
}

fn rollback_handler(args: &serde_json::Map<String, Value>, ctx: ToolContext<'_>) -> Result<Value> {
    let raw_path = required_str(args, "path")?;
    let path = ctx.sandbox.validate(Path::new(raw_path))?;
    let timestamp = get_str(args, "timestamp");

    let restored_from = backup::restore(ctx.sandbox.workspace(), &path, timestamp)?;
    Ok(json!({
        "restored": path.to_string_lossy(),
        "from": restored_from.to_string_lossy()
    }))
}

fn file_content_value(fc: FileContent) -> Value {
    json!({
        "path": fc.path.to_string_lossy(),
        "encoding": fc.encoding,
        "text": fc.text,
    })
}

fn write_result_value(result: WriteResult) -> Value {
    let dry_run = result.bytes_written == 0 && result.backup_path.is_none();
    json!({
        "bytes_written": result.bytes_written,
        "backup_path": result.backup_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "diff": result.diff,
        "dry_run": dry_run,
    })
}

fn patch_result_value(result: PatchResult) -> Value {
    json!({
        "replacements": result.replacements,
        "backup_path": result.backup_path.map(|p| p.to_string_lossy().into_owned()),
        "diff": result.diff,
    })
}

fn run_output_value(result: RunOutput) -> Value {
    json!({
        "success": result.success,
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "captured_output": result.captured_output,
        "output_file": result.output_file.to_string_lossy(),
        "command": result.command,
    })
}

fn build_output_value(result: BuildOutput) -> Value {
    json!({
        "success": result.success,
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "errors": result.errors,
        "output_file": result.output_file.to_string_lossy(),
        "command": result.command,
        "dry_run": result.dry_run,
    })
}

fn require_vfp_engine<'a>(ctx: &'a ToolContext<'a>) -> Result<&'a VfpEngine> {
    ctx.vfp_engine
        .ok_or_else(|| FoxProError::VfpNotConfigured("Set --vfp-path or FOXPRO_PATH".to_string()))
}

async fn run_handler(args: &serde_json::Map<String, Value>, ctx: ToolContext<'_>) -> Result<Value> {
    let engine = require_vfp_engine(&ctx)?;
    let path_arg = get_str(args, "path");
    let code_arg = get_str(args, "code");
    let timeout = get_usize(args, "timeout").map(|n| n as u64);
    let dry_run = get_bool(args, "dry_run").unwrap_or(false);

    if path_arg.is_none() && code_arg.is_none() {
        return Err(FoxProError::Rpc(
            "One of 'path' or 'code' is required".to_string(),
        ));
    }

    let output = if let Some(raw) = path_arg {
        let path = ctx.sandbox.validate(Path::new(raw))?;
        engine.run_file(&path, timeout, dry_run).await?
    } else {
        engine.run_code(code_arg.unwrap(), timeout, dry_run).await?
    };

    Ok(run_output_value(output))
}

async fn build_handler(
    args: &serde_json::Map<String, Value>,
    ctx: ToolContext<'_>,
) -> Result<Value> {
    let engine = require_vfp_engine(&ctx)?;
    let raw_project = required_str(args, "project")?;
    let project = ctx.sandbox.validate(Path::new(raw_project))?;

    let build_type = match get_str(args, "type").unwrap_or("exe") {
        "exe" => BuildType::Exe,
        "app" => BuildType::App,
        "dll" => BuildType::Dll,
        t => return Err(FoxProError::Rpc(format!("Unsupported build type: {t}"))),
    };

    let output = get_str(args, "output").map(Path::new);
    let output_path = output.map(|p| ctx.sandbox.resolve(p)).transpose()?;
    let timeout = get_usize(args, "timeout").map(|n| n as u64);
    let dry_run = get_bool(args, "dry_run").unwrap_or(false);

    let result = engine
        .build(
            &project,
            build_type,
            output_path.as_deref(),
            timeout,
            dry_run,
        )
        .await?;
    Ok(build_output_value(result))
}

async fn test_handler(
    args: &serde_json::Map<String, Value>,
    ctx: ToolContext<'_>,
) -> Result<Value> {
    let engine = require_vfp_engine(&ctx)?;
    let raw_project = required_str(args, "project")?;
    let project = ctx.sandbox.validate(Path::new(raw_project))?;
    let raw_test = required_str(args, "test")?;
    let test_path = ctx.sandbox.validate(Path::new(raw_test))?;

    let output = get_str(args, "output").map(Path::new);
    let output_path = output.map(|p| ctx.sandbox.resolve(p)).transpose()?;
    let timeout = get_usize(args, "timeout").map(|n| n as u64);
    let dry_run = get_bool(args, "dry_run").unwrap_or(false);

    let build_result = engine
        .build(
            &project,
            BuildType::Exe,
            output_path.as_deref(),
            timeout,
            dry_run,
        )
        .await?;
    let run_result = engine.run_file(&test_path, timeout, dry_run).await?;

    Ok(json!({
        "build": build_output_value(build_result),
        "run": run_output_value(run_result),
    }))
}

// Tool metadata -----------------------------------------------------------

fn status() -> Tool {
    Tool {
        name: "foxpro.status",
        description: "Return the server status, version and configured workspace.",
        input_schema: empty_schema(),
    }
}

fn get_workspace() -> Tool {
    Tool {
        name: "foxpro.get_workspace",
        description: "Return the absolute workspace directory used as the sandbox root.",
        input_schema: empty_schema(),
    }
}

fn read_code() -> Tool {
    Tool {
        name: "foxpro.read_code",
        description: "Read a file with optional line range and encoding detection.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer" },
                "end_line": { "type": "integer" },
                "encoding": { "type": "string" }
            },
            "required": ["path"]
        }),
    }
}

fn write_code() -> Tool {
    Tool {
        name: "foxpro.write_code",
        description: "Write content to a file. Backs up the existing file unless disabled. Supports dry-run and diff output.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "dry_run": { "type": "boolean" },
                "backup": { "type": "boolean" },
                "encoding": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
    }
}

fn search_code() -> Tool {
    Tool {
        name: "foxpro.search_code",
        description: "Search for a literal string or regex across files in the workspace.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "pattern": { "type": "string" },
                "regex": { "type": "boolean" },
                "case_insensitive": { "type": "boolean" },
                "file_type": { "type": "string" },
                "max_results": { "type": "integer" }
            },
            "required": ["pattern"]
        }),
    }
}

fn apply_patch() -> Tool {
    Tool {
        name: "foxpro.apply_patch",
        description: "Apply an exact-match patch. Replaces the first occurrence of old_text with new_text.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" },
                "dry_run": { "type": "boolean" },
                "backup": { "type": "boolean" }
            },
            "required": ["path", "old_text", "new_text"]
        }),
    }
}

fn rollback() -> Tool {
    Tool {
        name: "foxpro.rollback",
        description: "Restore a file from its latest backup, or from a backup matching the provided timestamp.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "timestamp": { "type": "string" }
            },
            "required": ["path"]
        }),
    }
}

fn run() -> Tool {
    Tool {
        name: "foxpro.run",
        description: "Execute a FoxPro .prg file or inline FoxPro code through the configured VFP9 runtime. Captures screen output and supports timeout and dry-run.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative or absolute path to a .prg inside the workspace" },
                "code": { "type": "string", "description": "Inline FoxPro source code to run" },
                "timeout": { "type": "integer", "description": "Timeout in seconds" },
                "dry_run": { "type": "boolean", "description": "Return the generated runner script without executing VFP" }
            },
            "required": []
        }),
    }
}

fn build() -> Tool {
    Tool {
        name: "foxpro.build",
        description: "Build a Visual FoxPro project (.pjx) into an EXE, APP or DLL. Parses build errors into structured JSON and supports dry-run.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "Path to the .pjx project file" },
                "type": { "type": "string", "enum": ["exe", "app", "dll"], "description": "Build output type" },
                "output": { "type": "string", "description": "Output executable path" },
                "timeout": { "type": "integer", "description": "Timeout in seconds" },
                "dry_run": { "type": "boolean", "description": "Return the generated build script without executing VFP" }
            },
            "required": ["project"]
        }),
    }
}

fn test() -> Tool {
    Tool {
        name: "foxpro.test",
        description: "Build a VFP project and then run a test .prg. Returns both build and run results.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "Path to the .pjx project file" },
                "test": { "type": "string", "description": "Path to the test .prg" },
                "output": { "type": "string", "description": "Output executable path" },
                "timeout": { "type": "integer", "description": "Timeout in seconds" },
                "dry_run": { "type": "boolean", "description": "Return generated scripts without executing VFP" }
            },
            "required": ["project", "test"]
        }),
    }
}

fn empty_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

// Argument helpers --------------------------------------------------------

fn required_str<'a>(args: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str> {
    get_str(args, key).ok_or_else(|| FoxProError::Rpc(format!("Missing required argument: {key}")))
}

fn get_str<'a>(args: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn get_bool(args: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

fn get_usize(args: &serde_json::Map<String, Value>, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx<'a>(config: &'a Config, sandbox: &'a Sandbox) -> ToolContext<'a> {
        ToolContext {
            config,
            sandbox,
            vfp_engine: None,
        }
    }

    #[test]
    fn serializes_mcp_input_schema_field() {
        let value = serde_json::to_value(status()).unwrap();
        assert!(value.get("inputSchema").is_some());
        assert!(value.get("input_schema").is_none());
    }

    #[tokio::test]
    async fn status_returns_version() {
        let config = Config {
            workspace: PathBuf::from("."),
            log_level: "info".to_string(),
            vfp_path: None,
            vfp_timeout: 30,
        };
        let sandbox = Sandbox::new(PathBuf::from("."));
        let result = call("foxpro.status", Some(&json!({})), ctx(&config, &sandbox))
            .await
            .unwrap();
        assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let config = Config {
            workspace: PathBuf::from("."),
            log_level: "info".to_string(),
            vfp_path: None,
            vfp_timeout: 30,
        };
        let sandbox = Sandbox::new(PathBuf::from("."));
        assert!(
            call(
                "foxpro.does_not_exist",
                Some(&json!({})),
                ctx(&config, &sandbox)
            )
            .await
            .is_err()
        );
    }
}
