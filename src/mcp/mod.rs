mod protocol;
mod tools;

use crate::config::Config;
use crate::error::Result;
use crate::sandbox::Sandbox;
use crate::ui::Instances;
use crate::vfp::VfpEngine;
use protocol::{Response, error_response, success_response};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};

/// Protocol revisions this server implements, newest first.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Requests larger than this are rejected instead of being parsed.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// State shared by all concurrently running requests.
pub struct AppState {
    pub config: Config,
    pub sandbox: Sandbox,
    pub vfp: Option<VfpEngine>,
    /// Serializes every workspace mutation.
    pub write_lock: tokio::sync::Mutex<()>,
    pub instances: Instances,
}

pub struct Server {
    state: Arc<AppState>,
}

type InFlight = Arc<Mutex<HashMap<String, AbortHandle>>>;

impl Server {
    pub fn new(config: Config) -> Result<Self> {
        let sandbox = Sandbox::new(config.workspace.clone());
        let vfp = match VfpEngine::new(
            config.vfp_path.clone(),
            config.workspace.clone(),
            config.vfp_timeout,
        ) {
            Ok(engine) => Some(engine),
            Err(e) => {
                tracing::warn!("VFP runtime not available: {e}");
                None
            }
        };
        Ok(Self {
            state: Arc::new(AppState {
                config,
                sandbox,
                vfp,
                write_lock: tokio::sync::Mutex::new(()),
                instances: Instances::default(),
            }),
        })
    }

    /// Serve JSON-RPC over stdio. Each request runs in its own task so a long
    /// VFP build never blocks `ping`, `tools/list` or cancellation; a single
    /// writer task keeps stdout messages whole.
    pub async fn run(&self) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let (tx, mut rx) = mpsc::channel::<String>(64);

        let writer = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = rx.recv().await {
                if stdout.write_all(msg.as_bytes()).await.is_err()
                    || stdout.write_all(b"\n").await.is_err()
                    || stdout.flush().await.is_err()
                {
                    tracing::error!("stdout closed; stopping");
                    break;
                }
            }
        });

        let in_flight: InFlight = Arc::default();
        let mut tasks = JoinSet::new();
        tracing::info!("FoxPro MCP server ready");

        while let Some(line) = lines.next_line().await? {
            // Reap finished tasks so the set does not grow without bound.
            while tasks.try_join_next().is_some() {}

            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_MESSAGE_BYTES {
                let resp = error_response(None, -32600, "Request too large");
                let _ = tx.send(serde_json::to_string(&resp)?).await;
                continue;
            }
            let message: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    let resp = error_response(None, -32700, format!("Parse error: {e}"));
                    let _ = tx.send(serde_json::to_string(&resp)?).await;
                    continue;
                }
            };

            let batch = match message {
                Value::Array(items) if items.is_empty() => {
                    let resp = error_response(None, -32600, "Invalid Request: empty batch");
                    let _ = tx.send(serde_json::to_string(&resp)?).await;
                    continue;
                }
                Value::Array(items) => items,
                single => vec![single],
            };

            for msg in batch {
                if let Some(cancelled) = cancelled_request(&msg) {
                    if let Some(handle) =
                        in_flight.lock().ok().and_then(|mut m| m.remove(&cancelled))
                    {
                        handle.abort();
                        tracing::info!(request = %cancelled, "request cancelled by client");
                    }
                    continue;
                }
                let state = self.state.clone();
                let tx = tx.clone();
                let key = msg
                    .get("id")
                    .filter(|id| !id.is_null())
                    .map(|id| id.to_string());
                let registry = in_flight.clone();
                let task_key = key.clone();
                let handle = tasks.spawn(async move {
                    if let Some(resp) = handle_message(state, msg).await
                        && let Ok(text) = serde_json::to_string(&resp)
                    {
                        let _ = tx.send(text).await;
                    }
                    if let Some(k) = task_key
                        && let Ok(mut m) = registry.lock()
                    {
                        m.remove(&k);
                    }
                });
                if let Some(k) = key
                    && let Ok(mut m) = in_flight.lock()
                {
                    m.insert(k, handle);
                }
            }
        }

        // stdin closed: let in-flight requests finish so their responses are
        // delivered, then stop the writer.
        while tasks.join_next().await.is_some() {}
        drop(tx);
        let _ = writer.await;
        let _ = self.state.instances.close(None, false).await;
        Ok(())
    }
}

/// `notifications/cancelled` → the id of the request to abort.
fn cancelled_request(msg: &Value) -> Option<String> {
    (msg.get("method")?.as_str()? == "notifications/cancelled")
        .then(|| msg.get("params")?.get("requestId").map(|id| id.to_string()))
        .flatten()
}

async fn handle_message(state: Arc<AppState>, msg: Value) -> Option<Response> {
    let id = msg.get("id").cloned().filter(|v| !v.is_null());
    let Some(obj) = msg.as_object() else {
        return Some(error_response(
            None,
            -32600,
            "Invalid Request: expected an object",
        ));
    };
    // Responses to server-initiated requests carry no method; ignore them.
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error_response(
            id,
            -32600,
            "Invalid Request: missing method",
        ));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error_response(
            id,
            -32600,
            "Invalid Request: jsonrpc must be 2.0",
        ));
    }
    let params = obj.get("params");
    let is_notification = id.is_none();

    let response = match method {
        "initialize" => success_response(id, initialize(params)),
        "ping" => success_response(id, json!({})),
        "tools/list" => success_response(id, tools::TOOL_LIST.clone()),
        "tools/call" => tools_call(state, id, params).await,
        "prompts/list" => success_response(id, json!({ "prompts": [] })),
        "resources/list" => success_response(id, json!({ "resources": [] })),
        "resources/templates/list" => success_response(id, json!({ "resourceTemplates": [] })),
        "prompts/get" => error_response(id, -32602, "Unknown prompt"),
        "resources/read" => error_response(id, -32602, "Unknown resource"),
        m if m.starts_with("notifications/") => return None,
        m => {
            if is_notification {
                tracing::debug!(method = %m, "ignored notification");
                return None;
            }
            error_response(id, -32601, format!("Method not found: {m}"))
        }
    };
    (!is_notification).then_some(response)
}

fn initialize(params: Option<&Value>) -> Value {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // Echo the client's version when we support it; otherwise offer our latest.
    let version = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == requested)
        .copied()
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "foxpro-mcp",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Visual FoxPro development tools. Paths are relative to the workspace. Mutating tools accept dry_run and keep backups in .mcp-backup (restore with foxpro.rollback). Inspect before modifying: foxpro.inspect_form / inspect_report / describe_table."
    })
}

async fn tools_call(state: Arc<AppState>, id: Option<Value>, params: Option<&Value>) -> Response {
    let Some(Value::Object(params)) = params else {
        return error_response(id, -32602, "Invalid params: expected object");
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error_response(id, -32602, "Invalid params: missing 'name'");
    };
    // Clients may omit `arguments` for tools without parameters.
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return error_response(id, -32602, "Invalid params: 'arguments' must be an object");
        }
    };

    let started = std::time::Instant::now();
    let result = tools::call(state, name.to_string(), arguments).await;
    let elapsed = started.elapsed().as_millis();

    match result {
        Ok(out) => {
            tracing::debug!(tool = name, elapsed_ms = elapsed as u64, "tool succeeded");
            let mut value = out.value;
            if let Value::Object(m) = &mut value {
                m.entry("success").or_insert(json!(true));
            }
            let mut content = vec![json!({
                "type": "text",
                "text": serde_json::to_string(&value).unwrap_or_default(),
            })];
            for (mime, data) in out.images {
                content.push(json!({ "type": "image", "data": data, "mimeType": mime }));
            }
            success_response(id, json!({ "content": content, "isError": false }))
        }
        Err(e) => {
            tracing::info!(
                tool = name,
                elapsed_ms = elapsed as u64,
                code = e.code(),
                "tool failed: {e}"
            );
            success_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": e.to_json().to_string() }],
                    "isError": true
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_state(dir: &TempDir) -> Arc<AppState> {
        let ws = Sandbox::canonicalize(dir.path()).unwrap();
        let config = Config {
            workspace: ws.clone(),
            log_level: "error".to_string(),
            vfp_path: None,
            vfp_timeout: 30,
        };
        Arc::new(AppState {
            sandbox: Sandbox::new(ws),
            config,
            vfp: None,
            write_lock: tokio::sync::Mutex::new(()),
            instances: Instances::default(),
        })
    }

    async fn request(state: &Arc<AppState>, method: &str, params: Value) -> Value {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = handle_message(state.clone(), msg).await.unwrap();
        serde_json::to_value(resp).unwrap()
    }

    async fn call_tool(state: &Arc<AppState>, name: &str, args: Value) -> (bool, Value) {
        let v = request(
            state,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        )
        .await;
        let text = v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        (
            v["result"]["isError"].as_bool().unwrap(),
            serde_json::from_str(&text).unwrap(),
        )
    }

    #[tokio::test]
    async fn tools_list_uses_input_schema_camel_case() {
        let dir = TempDir::new().unwrap();
        let v = request(&test_state(&dir), "tools/list", json!({})).await;
        let first = &v["result"]["tools"][0];
        assert!(first.get("inputSchema").is_some());
        assert!(first.get("input_schema").is_none());
    }

    #[tokio::test]
    async fn initialize_negotiates_supported_version() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let v = request(
            &state,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await;
        assert_eq!(v["result"]["protocolVersion"], json!("2025-06-18"));
        let v = request(
            &state,
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        )
        .await;
        assert_eq!(
            v["result"]["protocolVersion"],
            json!(SUPPORTED_PROTOCOL_VERSIONS[0])
        );
    }

    #[tokio::test]
    async fn ping_and_notifications() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let v = request(&state, "ping", Value::Null).await;
        assert_eq!(v["result"], json!({}));
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_message(state.clone(), note).await.is_none());
        let response = json!({ "jsonrpc": "2.0", "id": 5, "result": {} });
        assert!(handle_message(state, response).await.is_none());
    }

    #[tokio::test]
    async fn tool_without_arguments_and_structured_errors() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let v = request(&state, "tools/call", json!({ "name": "foxpro.status" })).await;
        assert_eq!(v["result"]["isError"], json!(false));

        let (is_error, body) = call_tool(
            &state,
            "foxpro.read_code",
            json!({ "path": "../etc/passwd" }),
        )
        .await;
        assert!(is_error);
        assert_eq!(body["success"], json!(false));
        assert!(body["error"]["code"].is_string());

        let (is_error, body) = call_tool(&state, "foxpro_run", json!({ "code": "? 1" })).await;
        assert!(is_error);
        assert_eq!(body["error"]["code"], json!("VFP_NOT_CONFIGURED"));
    }

    #[tokio::test]
    async fn end_to_end_form_workflow() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);

        let (err, body) = call_tool(&state, "foxpro.create_form", json!({
            "file": "forms/search.scx",
            "caption": "ค้นหาลูกค้า",
            "width": 800,
            "height": 500,
            "controls": [
                { "type": "Label", "name": "lblId", "caption": "รหัสลูกค้า", "left": 20, "top": 20 },
                { "type": "TextBox", "name": "txtId", "left": 120, "top": 16 },
                { "type": "CommandButton", "name": "btnSearch", "caption": "ค้นหา",
                  "methods": { "Click": "THISFORM.Refresh()" } }
            ]
        })).await;
        assert!(!err, "{body}");
        assert_eq!(body["result"]["encoding"], json!("windows-874"));

        let (err, body) = call_tool(&state, "foxpro.update_control", json!({
            "form": "forms/search.scx", "object": "txtId", "properties": { "Width": 200 }, "dry_run": true
        })).await;
        assert!(!err, "{body}");
        assert!(body["diff"].as_str().unwrap().contains("Width = 200"));

        let (err, body) = call_tool(
            &state,
            "foxpro.inspect_form",
            json!({ "file": "forms/search.scx" }),
        )
        .await;
        assert!(!err, "{body}");
        let txt = body["controls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "txtId")
            .unwrap();
        assert_eq!(txt["width"], json!(100.0), "dry run must not write");

        let (err, _) = call_tool(&state, "foxpro.update_method", json!({
            "form": "forms/search.scx", "object": "btnSearch", "method": "Click", "code": "WAIT WINDOW 'x'"
        })).await;
        assert!(!err);
        let (err, body) = call_tool(
            &state,
            "foxpro.rollback",
            json!({ "path": "forms/search.scx" }),
        )
        .await;
        assert!(!err, "{body}");
        assert_eq!(body["restored"].as_array().unwrap().len(), 2);

        let (_, body) = call_tool(
            &state,
            "foxpro.inspect_form",
            json!({ "file": "forms/search.scx", "include_code": true }),
        )
        .await;
        let btn = body["controls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "btnSearch")
            .unwrap();
        assert_eq!(btn["methods"][0]["code"], json!("THISFORM.Refresh()"));
    }

    #[tokio::test]
    async fn agent_loop_rolls_back_failed_steps() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        std::fs::write(dir.path().join("main.prg"), "? 'v1'\r\n").unwrap();

        let (err, body) = call_tool(&state, "foxpro.agent_loop", json!({
            "rollback_on_failure": true,
            "steps": [
                { "tool": "foxpro.apply_patch", "arguments": { "path": "main.prg", "old_text": "v1", "new_text": "v2" } },
                { "tool": "foxpro.write_code", "arguments": { "path": "new.prg", "content": "? 1" } },
                { "tool": "foxpro.apply_patch", "arguments": { "path": "main.prg", "old_text": "missing", "new_text": "x" } }
            ]
        })).await;
        assert!(!err, "{body}");
        assert_eq!(body["status"], json!("failed"));
        assert_eq!(body["failed_stage"], json!("modify"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.prg")).unwrap(),
            "? 'v1'\r\n"
        );
        assert!(!dir.path().join("new.prg").exists());
    }

    #[tokio::test]
    async fn report_and_table_tools() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let (err, body) = call_tool(
            &state,
            "foxpro.create_report",
            json!({
                "file": "reports/daily.frx",
                "orientation": "portrait",
                "groups": [{ "expression": "invoice.cust_id" }],
                "fields": [
                    { "band": "page_header", "text": "เลขที่บิล", "x": 20, "y": 10 },
                    { "expression": "invoice_no", "x": 20, "y": 2, "width": 100 }
                ]
            }),
        )
        .await;
        assert!(!err, "{body}");
        let (err, body) = call_tool(
            &state,
            "foxpro.inspect_report",
            json!({ "file": "reports/daily.frx" }),
        )
        .await;
        assert!(!err, "{body}");
        assert_eq!(body["fields"].as_array().unwrap().len(), 2);

        let (err, body) = call_tool(
            &state,
            "foxpro.query_table",
            json!({ "table": "reports/daily.frx" }),
        )
        .await;
        assert!(err, "only .dbf files are tables: {body}");
    }
}
