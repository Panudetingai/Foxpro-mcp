//! `foxpro.agent_loop` (Phase 6): one call that runs
//! modify → compile → build → run → verify, and optionally rolls every
//! modification back when a stage fails. The "fix" step stays with the agent:
//! the response lists concrete next actions derived from the errors.

use super::{Args, Tool, ToolOutput, canonical_name, runtime, schema, unknown};
use crate::error::{FoxProError, Result};
use crate::fsutil;
use crate::mcp::AppState;
use crate::vfp;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_STEPS: usize = 50;

/// Tools with their own stage, or that would recurse / leave processes behind.
const BLOCKED_IN_STEPS: &[&str] = &[
    "foxpro.agent_loop",
    "foxpro.run",
    "foxpro.build",
    "foxpro.test",
    "foxpro.launch",
    "foxpro.screenshot",
    "foxpro.close",
    "foxpro.rollback",
];

pub fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "foxpro.agent_loop",
        description: "Run one iteration of modify → compile → build → run → verify. `steps` are ordinary tool calls (write_code, apply_patch, add_control, update_method, ...); modified forms are compiled with COMPILE FORM; then the optional build and run stages execute and `verify` checks the output. With rollback_on_failure every modified file is restored if any stage fails. dry_run previews all steps without writing or running anything. Returns per-stage results and suggested next actions for fixing failures.",
        input_schema: schema(
            json!({
                "steps": { "type": "array", "items": { "type": "object", "properties": {
                    "tool": { "type": "string" }, "arguments": { "type": "object" } }, "required": ["tool"] } },
                "compile_forms": { "type": "boolean", "description": "COMPILE FORM every modified .scx (default true when VFP is available)" },
                "build": { "type": "object", "properties": {
                    "project": {"type":"string"}, "type": {"type":"string"}, "output": {"type":"string"} } },
                "run": { "type": "object", "properties": { "path": {"type":"string"}, "code": {"type":"string"} } },
                "verify": { "type": "object", "properties": {
                    "output_contains": { "type": "array", "items": {"type":"string"} },
                    "output_not_contains": { "type": "array", "items": {"type":"string"} },
                    "no_errors": { "type": "boolean", "description": "Default true" } } },
                "rollback_on_failure": { "type": "boolean" },
                "timeout": { "type": "integer" },
                "dry_run": { "type": "boolean" }
            }),
            &[],
        ),
    }]
}

pub fn handles(name: &str) -> bool {
    name == "foxpro.agent_loop"
}

fn absolute(ws: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        ws.join(path)
    }
}

/// Files a step modified and how to undo it: `(file, backup)`; a `None`
/// backup means the file was created by the step.
fn touched_files(ws: &Path, result: &Value) -> Vec<(PathBuf, Option<PathBuf>)> {
    if result["dry_run"].as_bool() == Some(true) {
        return Vec::new();
    }
    let mut out = Vec::new();
    // Designer tools: {"backups": [{file, backup}], "written": [...]}.
    if let Some(written) = result["written"].as_array() {
        let backups = result["backups"].as_array().cloned().unwrap_or_default();
        for w in written.iter().filter_map(Value::as_str) {
            let file = absolute(ws, w);
            let backup = backups
                .iter()
                .find(|b| b["file"].as_str().map(|f| absolute(ws, f)) == Some(file.clone()))
                .and_then(|b| b["backup"].as_str())
                .map(|b| absolute(ws, b));
            out.push((file, backup));
        }
        return out;
    }
    // Code tools: {"path", "backup_path", "created"}.
    if let Some(path) = result["path"].as_str()
        && (result.get("bytes_written").is_some() || result.get("replacements").is_some())
    {
        let backup = result["backup_path"].as_str().map(|b| absolute(ws, b));
        out.push((absolute(ws, path), backup));
    }
    out
}

fn rollback(touched: &[(PathBuf, Option<PathBuf>)]) -> Vec<Value> {
    // Undo in reverse order so the oldest backup of a file wins.
    touched
        .iter()
        .rev()
        .map(|(file, backup)| {
            let outcome = match backup {
                Some(b) => std::fs::read(b)
                    .map_err(FoxProError::from)
                    .and_then(|bytes| fsutil::atomic_write(file, &bytes))
                    .map(|_| "restored"),
                None => std::fs::remove_file(file)
                    .map_err(FoxProError::from)
                    .map(|_| "deleted (was created by the loop)"),
            };
            match outcome {
                Ok(action) => json!({ "file": file.to_string_lossy(), "action": action }),
                Err(e) => json!({ "file": file.to_string_lossy(), "error": e.to_string() }),
            }
        })
        .collect()
}

fn error_actions(prefix: &str, errors: &Value, out: &mut Vec<String>) {
    for e in errors.as_array().into_iter().flatten().take(20) {
        let msg = e["message"].as_str().unwrap_or("error");
        let file = e["file"].as_str().or(e["program"].as_str()).unwrap_or("");
        match e["line"].as_u64() {
            Some(line) if !file.is_empty() => {
                out.push(format!("{prefix}: fix {file} line {line}: {msg}"))
            }
            _ if !file.is_empty() => out.push(format!("{prefix}: fix {file}: {msg}")),
            _ => out.push(format!("{prefix}: {msg}")),
        }
    }
}

async fn compile_forms(state: &AppState, forms: &[PathBuf], timeout: Option<u64>) -> Result<Value> {
    let engine = state
        .vfp
        .as_ref()
        .ok_or_else(|| FoxProError::VfpNotConfigured("VFP runtime not configured".into()))?;
    let mut code = String::new();
    for f in forms {
        let err = f.with_extension("err");
        let _ = std::fs::remove_file(&err);
        code.push_str(&format!("COMPILE FORM {}\r\n", vfp::quote(f)));
    }
    let run = engine.run_code(&code, timeout, false).await?;
    let mut errors = serde_json::to_value(&run.errors)?;
    for f in forms {
        let err = f.with_extension("err");
        if let Ok(bytes) = std::fs::read(&err) {
            let (_, text) = crate::encoding::decode_bytes(&bytes, None);
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Some(arr) = errors.as_array_mut() {
                    arr.push(json!({ "file": f.to_string_lossy(), "message": line.trim() }));
                }
            }
        }
    }
    let ok = errors.as_array().is_none_or(Vec::is_empty) && !run.timed_out;
    Ok(json!({ "success": ok, "forms": forms, "errors": errors }))
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    if name != "foxpro.agent_loop" {
        return Err(unknown(name));
    }
    let ws = state.sandbox.workspace().to_path_buf();
    let dry_run = a.flag("dry_run", false)?;
    let rollback_on_failure = a.flag("rollback_on_failure", false)?;
    let timeout = a.timeout()?;
    let steps = a.array("steps")?;
    if steps.len() > MAX_STEPS {
        return Err(FoxProError::InvalidArgument(format!(
            "at most {MAX_STEPS} steps per loop"
        )));
    }

    let mut report = Map::new();
    let mut next_actions: Vec<String> = Vec::new();
    let mut failed_stage: Option<&str> = None;
    let mut touched: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();

    // 1. modify
    let mut step_results = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step["tool"]
            .as_str()
            .map(canonical_name)
            .ok_or_else(|| FoxProError::InvalidArgument(format!("steps[{i}] needs a 'tool'")))?;
        if BLOCKED_IN_STEPS.contains(&tool.as_str()) {
            return Err(FoxProError::InvalidArgument(format!(
                "steps[{i}]: {tool} cannot be used as a step (use the build/run/verify options instead)"
            )));
        }
        let mut args = match &step["arguments"] {
            Value::Object(m) => m.clone(),
            Value::Null => Map::new(),
            _ => {
                return Err(FoxProError::InvalidArgument(format!(
                    "steps[{i}].arguments must be an object"
                )));
            }
        };
        if dry_run {
            args.insert("dry_run".into(), json!(true));
        }
        if rollback_on_failure {
            // Rollback needs a backup of every modified file.
            args.insert("backup".into(), json!(true));
        }
        match super::call(state.clone(), tool.clone(), args).await {
            Ok(out) => {
                touched.extend(touched_files(&ws, &out.value));
                step_results.push(json!({ "tool": tool, "success": true, "result": out.value }));
            }
            Err(e) => {
                next_actions.push(format!("step {i} ({tool}) failed: {e}"));
                step_results
                    .push(json!({ "tool": tool, "success": false, "error": e.to_json()["error"] }));
                failed_stage = Some("modify");
                break;
            }
        }
    }
    report.insert("steps".into(), json!(step_results));

    // 2. compile modified forms
    let forms: Vec<PathBuf> = touched
        .iter()
        .map(|(f, _)| f.clone())
        .filter(|f| fsutil::has_extension(f, "scx"))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let want_compile = a.flag("compile_forms", state.vfp.is_some())?;
    if failed_stage.is_none() && want_compile && !forms.is_empty() && !dry_run {
        match compile_forms(&state, &forms, timeout).await {
            Ok(v) => {
                if v["success"] != json!(true) {
                    failed_stage = Some("compile");
                    error_actions("compile", &v["errors"], &mut next_actions);
                }
                report.insert("compile".into(), v);
            }
            Err(e) => {
                failed_stage = Some("compile");
                next_actions.push(format!("compile: {e}"));
                report.insert("compile".into(), e.to_json());
            }
        }
    }

    // 3. build
    let build_args = a.object("build")?;
    if failed_stage.is_none() && !build_args.is_empty() {
        let mut b = build_args;
        b.insert("dry_run".into(), json!(dry_run));
        if let Some(t) = timeout {
            b.insert("timeout".into(), json!(t));
        }
        match runtime::build(&state, &Args(b)).await {
            Ok(v) => {
                if v["success"] != json!(true) && !dry_run {
                    failed_stage = Some("build");
                    error_actions("build", &v["errors"], &mut next_actions);
                    error_actions("build", &v["runtime_errors"], &mut next_actions);
                    if v["errors"].as_array().is_none_or(Vec::is_empty)
                        && v["runtime_errors"].as_array().is_none_or(Vec::is_empty)
                    {
                        next_actions.push(
                            "build: no output file was produced; check the project and output path"
                                .into(),
                        );
                    }
                }
                report.insert("build".into(), v);
            }
            Err(e) => {
                failed_stage = Some("build");
                next_actions.push(format!("build: {e}"));
                report.insert("build".into(), e.to_json());
            }
        }
    }

    // 4. run
    let run_args = a.object("run")?;
    let mut run_output = String::new();
    let mut run_errors = json!([]);
    if failed_stage.is_none() && !run_args.is_empty() {
        let mut r = run_args;
        r.insert("dry_run".into(), json!(dry_run));
        if let Some(t) = timeout {
            r.insert("timeout".into(), json!(t));
        }
        match runtime::run(&state, &Args(r)).await {
            Ok(v) => {
                run_output = v["captured_output"].as_str().unwrap_or("").to_string();
                run_errors = v["errors"].clone();
                report.insert("run".into(), v);
            }
            Err(e) => {
                failed_stage = Some("run");
                next_actions.push(format!("run: {e}"));
                report.insert("run".into(), e.to_json());
            }
        }
    }

    // 5. verify
    if failed_stage.is_none() && report.contains_key("run") && !dry_run {
        let verify = a.object("verify")?;
        let mut checks = Vec::new();
        let no_errors = verify
            .get("no_errors")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if no_errors {
            let passed = run_errors.as_array().is_none_or(Vec::is_empty);
            if !passed {
                error_actions("run", &run_errors, &mut next_actions);
            }
            checks.push(json!({ "check": "no runtime errors", "passed": passed }));
        }
        let list = |k: &str| -> Vec<String> {
            verify
                .get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        for s in list("output_contains") {
            let passed = run_output.contains(&s);
            if !passed {
                next_actions.push(format!("verify: expected output to contain {s:?}"));
            }
            checks.push(json!({ "check": format!("output contains {s:?}"), "passed": passed }));
        }
        for s in list("output_not_contains") {
            let passed = !run_output.contains(&s);
            if !passed {
                next_actions.push(format!("verify: output must not contain {s:?}"));
            }
            checks.push(
                json!({ "check": format!("output does not contain {s:?}"), "passed": passed }),
            );
        }
        if checks.iter().any(|c| c["passed"] == json!(false)) {
            failed_stage = Some("verify");
        }
        report.insert("verification".into(), json!({ "checks": checks }));
    }

    // 6. rollback
    if failed_stage.is_some() && rollback_on_failure && !dry_run && !touched.is_empty() {
        let _guard = state.write_lock.lock().await;
        let files = touched.clone();
        let rolled = super::blocking(move || Ok(rollback(&files))).await?;
        report.insert("rolled_back".into(), json!(rolled));
    }

    report.insert("success".into(), json!(failed_stage.is_none()));
    report.insert(
        "status".into(),
        json!(match (failed_stage, dry_run) {
            (None, true) => "previewed",
            (None, false) => "passed",
            (Some(_), _) => "failed",
        }),
    );
    report.insert("failed_stage".into(), json!(failed_stage));
    report.insert("dry_run".into(), json!(dry_run));
    report.insert(
        "modified_files".into(),
        json!(
            touched
                .iter()
                .map(|(f, _)| fsutil::display_relative(f, &ws))
                .collect::<Vec<_>>()
        ),
    );
    report.insert("next_actions".into(), json!(next_actions));
    Ok(Value::Object(report).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touched_files_from_results() {
        let ws = Path::new("/ws");
        let designer = json!({
            "dry_run": false,
            "written": ["/ws/a.scx", "/ws/a.sct"],
            "backups": [{ "file": "/ws/a.scx", "backup": "/ws/.mcp-backup/a.scx.1.bak" }],
        });
        let t = touched_files(ws, &designer);
        assert_eq!(t.len(), 2);
        assert!(t[0].1.is_some());
        assert!(t[1].1.is_none());

        let code = json!({ "path": "src/a.prg", "bytes_written": 3, "backup_path": null, "dry_run": false });
        assert_eq!(
            touched_files(ws, &code),
            vec![(PathBuf::from("/ws/src/a.prg"), None)]
        );
        assert!(
            touched_files(
                ws,
                &json!({ "dry_run": true, "path": "x", "bytes_written": 0 })
            )
            .is_empty()
        );
    }
}
