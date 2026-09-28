//! MCP tool registry and dispatch.
//!
//! Handlers that touch the filesystem run on the blocking thread pool so the
//! stdio loop stays responsive. Mutating tools additionally take the
//! workspace write lock, so two concurrent edits of the same file can never
//! interleave.

mod agent;
mod code;
mod db;
mod form;
mod report;
mod runtime;
mod ui;

use crate::error::{FoxProError, Result};
use crate::mcp::AppState;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

#[derive(Debug, Serialize, Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Extra MCP content blocks (images) returned next to the JSON result.
#[derive(Debug, Default)]
pub struct ToolOutput {
    pub value: Value,
    pub images: Vec<(String, String)>,
}

impl From<Value> for ToolOutput {
    fn from(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
        }
    }
}

pub fn all() -> Vec<Tool> {
    let mut tools = vec![
        Tool {
            name: "foxpro.status",
            description: "Server status: version, workspace, VFP runtime availability and running UI instances.",
            input_schema: schema(json!({}), &[]),
        },
        Tool {
            name: "foxpro.get_workspace",
            description: "Return the absolute workspace directory used as the sandbox root.",
            input_schema: schema(json!({}), &[]),
        },
    ];
    tools.extend(code::tools());
    tools.extend(runtime::tools());
    tools.extend(form::tools());
    tools.extend(report::tools());
    tools.extend(db::tools());
    tools.extend(ui::tools());
    tools.extend(agent::tools());
    tools
}

/// `tools/list` payload, built once.
pub static TOOL_LIST: LazyLock<Value> = LazyLock::new(|| json!({ "tools": all() }));

/// Accept `foxpro.read_code`, `foxpro_read_code` and `read_code`: some MCP
/// clients rewrite dots in tool names because their model APIs forbid them.
pub fn canonical_name(name: &str) -> String {
    let n = name.trim();
    let bare = n
        .strip_prefix("foxpro.")
        .or_else(|| n.strip_prefix("foxpro__"))
        .or_else(|| n.strip_prefix("foxpro_"))
        .unwrap_or(n);
    format!("foxpro.{bare}")
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub fn call(
    state: Arc<AppState>,
    name: String,
    args: Map<String, Value>,
) -> BoxFuture<'static, Result<ToolOutput>> {
    Box::pin(async move {
        let name = canonical_name(&name);
        let a = Args(args);
        match name.as_str() {
            "foxpro.status" => status(&state).await,
            "foxpro.get_workspace" => Ok(json!({
                "workspace": state.sandbox.workspace().to_string_lossy()
            })
            .into()),
            n if code::handles(n) => code::call(n, a, state).await,
            n if runtime::handles(n) => runtime::call(n, a, state).await,
            n if form::handles(n) => form::call(n, a, state).await,
            n if report::handles(n) => report::call(n, a, state).await,
            n if db::handles(n) => db::call(n, a, state).await,
            n if ui::handles(n) => ui::call(n, a, state).await,
            n if agent::handles(n) => agent::call(n, a, state).await,
            _ => Err(FoxProError::InvalidArgument(format!(
                "Unknown tool: {name}. Call tools/list for available tools."
            ))),
        }
    })
}

async fn status(state: &AppState) -> Result<ToolOutput> {
    let instances = state.instances.list().await;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "workspace": state.config.workspace.to_string_lossy(),
        "log_level": state.config.log_level,
        "platform": std::env::consts::OS,
        "vfp": match &state.vfp {
            Some(e) => json!({ "available": true, "executable": e.executable().to_string_lossy(), "default_timeout_secs": state.config.vfp_timeout }),
            None => json!({ "available": false, "hint": "set --vfp-path, FOXPRO_PATH or vfp_path in foxpro-mcp.json" }),
        },
        "ui_instances": instances,
    })
    .into())
}

/// Run a synchronous handler on the blocking pool.
pub async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| FoxProError::Internal(format!("handler failed: {e}")))?
}

/// Run a mutating synchronous handler under the workspace write lock.
pub async fn mutate<T, F>(state: &AppState, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let _guard = state.write_lock.lock().await;
    blocking(f).await
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// Typed access to tool arguments. A key that is present with the wrong type
/// is an error — silently ignoring e.g. `"dry_run": "yes"` would turn a
/// preview into a real write.
#[derive(Debug, Clone, Default)]
pub struct Args(pub Map<String, Value>);

impl Args {
    fn present(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    fn type_error(key: &str, expected: &str) -> FoxProError {
        FoxProError::InvalidArgument(format!("'{key}' must be {expected}"))
    }

    pub fn str(&self, key: &str) -> Result<Option<&str>> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(Self::type_error(key, "a string")),
        }
    }

    pub fn req_str(&self, key: &str) -> Result<&str> {
        self.str(key)?.ok_or_else(|| {
            FoxProError::InvalidArgument(format!("Missing required argument: {key}"))
        })
    }

    pub fn bool(&self, key: &str) -> Result<Option<bool>> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | ".t." | "yes" | "1" => Ok(Some(true)),
                "false" | ".f." | "no" | "0" => Ok(Some(false)),
                _ => Err(Self::type_error(key, "a boolean")),
            },
            Some(_) => Err(Self::type_error(key, "a boolean")),
        }
    }

    pub fn flag(&self, key: &str, default: bool) -> Result<bool> {
        Ok(self.bool(key)?.unwrap_or(default))
    }

    pub fn u64(&self, key: &str) -> Result<Option<u64>> {
        match self.present(key) {
            None => Ok(None),
            Some(v) => v
                .as_u64()
                .or_else(|| {
                    v.as_f64()
                        .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                        .map(|f| f as u64)
                })
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .map(Some)
                .ok_or_else(|| Self::type_error(key, "a non-negative integer")),
        }
    }

    pub fn usize(&self, key: &str) -> Result<Option<usize>> {
        Ok(self.u64(key)?.map(|n| n.min(usize::MAX as u64) as usize))
    }

    pub fn f64(&self, key: &str) -> Result<Option<f64>> {
        match self.present(key) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .filter(|f| f.is_finite())
                .map(Some)
                .ok_or_else(|| Self::type_error(key, "a number")),
        }
    }

    pub fn object(&self, key: &str) -> Result<Map<String, Value>> {
        match self.present(key) {
            None => Ok(Map::new()),
            Some(Value::Object(m)) => Ok(m.clone()),
            Some(_) => Err(Self::type_error(key, "an object")),
        }
    }

    pub fn array(&self, key: &str) -> Result<Vec<Value>> {
        match self.present(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => Ok(a.clone()),
            Some(_) => Err(Self::type_error(key, "an array")),
        }
    }

    pub fn strings(&self, key: &str) -> Result<Vec<String>> {
        match self.present(key) {
            None => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| Self::type_error(key, "an array of strings"))
                })
                .collect(),
            Some(_) => Err(Self::type_error(key, "an array of strings")),
        }
    }

    /// First present key among aliases (e.g. `file` / `path`).
    pub fn req_path(&self, keys: &[&str]) -> Result<PathBuf> {
        for k in keys {
            if let Some(s) = self.str(k)? {
                return Ok(PathBuf::from(s));
            }
        }
        Err(FoxProError::InvalidArgument(format!(
            "Missing required argument: {}",
            keys[0]
        )))
    }

    pub fn timeout(&self) -> Result<Option<u64>> {
        Ok(self
            .u64("timeout")?
            .map(|t| t.clamp(1, crate::vfp::MAX_TIMEOUT_SECS)))
    }
}

// ---------------------------------------------------------------------------
// Schema helpers
// ---------------------------------------------------------------------------

pub fn unknown(name: &str) -> FoxProError {
    FoxProError::InvalidArgument(format!("Unknown tool: {name}"))
}

pub fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

pub fn to_value<T: Serialize>(v: &T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_object_schema_and_unique_name() {
        let tools = all();
        let mut names = std::collections::HashSet::new();
        for t in &tools {
            assert!(names.insert(t.name), "duplicate tool {}", t.name);
            assert_eq!(t.input_schema["type"], json!("object"));
            assert!(t.name.starts_with("foxpro."));
            let v = serde_json::to_value(t).unwrap();
            assert!(v.get("inputSchema").is_some());
        }
        for required in [
            "foxpro.inspect_form",
            "foxpro.create_form",
            "foxpro.add_control",
            "foxpro.update_control",
            "foxpro.remove_control",
            "foxpro.update_method",
            "foxpro.inspect_report",
            "foxpro.create_report",
            "foxpro.add_report_field",
            "foxpro.update_report_field",
            "foxpro.remove_report_field",
            "foxpro.inspect_table",
            "foxpro.describe_table",
            "foxpro.query_table",
            "foxpro.find_records",
            "foxpro.agent_loop",
            "foxpro.launch",
            "foxpro.screenshot",
            "foxpro.close",
        ] {
            assert!(names.contains(required), "missing {required}");
        }
    }

    #[test]
    fn canonical_names() {
        assert_eq!(canonical_name("foxpro.read_code"), "foxpro.read_code");
        assert_eq!(canonical_name("foxpro_read_code"), "foxpro.read_code");
        assert_eq!(canonical_name("read_code"), "foxpro.read_code");
    }

    #[test]
    fn args_are_strictly_typed() {
        let a = Args(
            json!({"dry_run": "maybe", "n": -1, "flag": "true"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(a.bool("dry_run").is_err());
        assert!(a.u64("n").is_err());
        assert_eq!(a.bool("flag").unwrap(), Some(true));
        assert_eq!(a.bool("missing").unwrap(), None);
    }
}
