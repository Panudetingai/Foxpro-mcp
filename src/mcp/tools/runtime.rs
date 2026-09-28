//! VFP runtime tools: run, build, test.

use super::{Args, Tool, ToolOutput, schema, to_value, unknown};
use crate::error::{FoxProError, Result};
use crate::mcp::AppState;
use crate::vfp::{BuildType, VfpEngine};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "foxpro.run",
            description: "Run a .prg/.scx file or inline FoxPro code with the VFP9 runtime (headless). Captures ? output (SET ALTERNATE), trapped runtime errors (number, message, program, line) and enforces a timeout. dry_run returns the generated runner.",
            input_schema: schema(
                json!({
                    "path": { "type": "string", "description": ".prg or .scx inside the workspace" },
                    "code": { "type": "string", "description": "Inline FoxPro code (may define its own procedures)" },
                    "timeout": { "type": "integer", "description": "Seconds (default from config)" },
                    "dry_run": { "type": "boolean" }
                }),
                &[],
            ),
        },
        Tool {
            name: "foxpro.build",
            description: "BUILD EXE/APP/DLL from a .pjx project (with RECOMPILE). Returns structured compile errors from the .err file.",
            input_schema: schema(
                json!({
                    "project": { "type": "string", "description": ".pjx project file" },
                    "type": { "type": "string", "enum": ["exe", "app", "dll"] },
                    "output": { "type": "string", "description": "Output file (default: project name + extension)" },
                    "timeout": { "type": "integer" },
                    "dry_run": { "type": "boolean" }
                }),
                &["project"],
            ),
        },
        Tool {
            name: "foxpro.test",
            description: "Build a project, then (only if the build succeeded) run a test program. Returns both results and an overall success flag.",
            input_schema: schema(
                json!({
                    "project": { "type": "string" },
                    "test": { "type": "string", "description": "Test .prg to run after the build" },
                    "type": { "type": "string", "enum": ["exe", "app", "dll"] },
                    "output": { "type": "string" },
                    "timeout": { "type": "integer" },
                    "dry_run": { "type": "boolean" }
                }),
                &["project", "test"],
            ),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(name, "foxpro.run" | "foxpro.build" | "foxpro.test")
}

fn engine(state: &AppState) -> Result<&VfpEngine> {
    state.vfp.as_ref().ok_or_else(|| {
        FoxProError::VfpNotConfigured(
            "Set --vfp-path, FOXPRO_PATH or vfp_path in foxpro-mcp.json".into(),
        )
    })
}

pub async fn run(state: &AppState, a: &Args) -> Result<Value> {
    let engine = engine(state)?;
    let timeout = a.timeout()?;
    let dry_run = a.flag("dry_run", false)?;
    let out = match (a.str("path")?, a.str("code")?) {
        (Some(_), Some(_)) => {
            return Err(FoxProError::InvalidArgument(
                "pass either 'path' or 'code', not both".into(),
            ));
        }
        (Some(p), None) => {
            let path = state.sandbox.validate(std::path::Path::new(p))?;
            engine.run_file(&path, timeout, dry_run).await?
        }
        (None, Some(code)) => engine.run_code(code, timeout, dry_run).await?,
        (None, None) => {
            return Err(FoxProError::InvalidArgument(
                "One of 'path' or 'code' is required".into(),
            ));
        }
    };
    to_value(&out)
}

pub async fn build(state: &AppState, a: &Args) -> Result<Value> {
    let engine = engine(state)?;
    let project = state.sandbox.validate(&a.req_path(&["project"])?)?;
    let build_type = BuildType::parse(a.str("type")?.unwrap_or("exe"))?;
    let output = a
        .str("output")?
        .map(|p| state.sandbox.resolve(std::path::Path::new(p)))
        .transpose()?;
    let _guard = state.write_lock.lock().await;
    let out = engine
        .build(
            &project,
            build_type,
            output.as_deref(),
            a.timeout()?,
            a.flag("dry_run", false)?,
        )
        .await?;
    to_value(&out)
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    match name {
        "foxpro.run" => Ok(run(&state, &a).await?.into()),
        "foxpro.build" => Ok(build(&state, &a).await?.into()),
        "foxpro.test" => {
            let test = state.sandbox.validate(&a.req_path(&["test"])?)?;
            let build_result = build(&state, &a).await?;
            let built = build_result["success"].as_bool().unwrap_or(false);
            let dry_run = a.flag("dry_run", false)?;
            let run_result = if built || dry_run {
                let engine = engine(&state)?;
                to_value(&engine.run_file(&test, a.timeout()?, dry_run).await?)?
            } else {
                json!({ "skipped": true, "reason": "build failed; fix the build errors first" })
            };
            let success = built && run_result["success"].as_bool().unwrap_or(false);
            Ok(json!({
                "success": success,
                "dry_run": dry_run,
                "build": build_result,
                "run": run_result,
            })
            .into())
        }
        other => Err(unknown(other)),
    }
}
