mod protocol;
mod tools;

use crate::config::Config;
use crate::error::Result;
use crate::sandbox::Sandbox;
use protocol::{error_response, Request, Response};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct Server {
    sandbox: Sandbox,
    config: Config,
}

impl Server {
    pub fn new(config: Config) -> Result<Self> {
        let sandbox = Sandbox::new(config.workspace.clone());
        Ok(Self { sandbox, config })
    }

    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();
        let mut stdout = tokio::io::stdout();

        tracing::info!("FoxPro MCP server ready");

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(response) = self.handle_line(line).await {
                let text = serde_json::to_string(&response)? + "\n";
                stdout.write_all(text.as_bytes()).await?;
                stdout.flush().await?;
            }
        }

        Ok(())
    }

    async fn handle_line(&self, line: &str) -> Option<Response> {
        let request: Request = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(e) => {
                return Some(error_response(None, -32700, format!("Parse error: {e}")));
            }
        };

        if request.jsonrpc != "2.0" {
            return Some(error_response(
                request.id,
                -32600,
                "Invalid Request: jsonrpc must be 2.0",
            ));
        }

        match request.method.as_str() {
            "initialize" => Some(self.initialize(&request)),
            "ping" => Some(self.ping(&request)),
            "notifications/initialized" | "notifications/cancelled" | "notifications/progress" => {
                None
            }
            "tools/list" => Some(self.tools_list(&request)),
            "tools/call" => Some(self.tools_call(&request)),
            "prompts/list" => Some(self.list_named(&request, "prompts", json!([]))),
            "prompts/get" => Some(self.not_found(&request, "Unknown prompt")),
            "resources/list" => Some(self.list_named(&request, "resources", json!([]))),
            "resources/templates/list" => {
                Some(self.list_named(&request, "resourceTemplates", json!([])))
            }
            "resources/read" => Some(self.not_found(&request, "Unknown resource")),
            "shutdown" | "exit" => Some(self.shutdown(&request)),
            _ => {
                if is_notification(&request.id) {
                    tracing::debug!(method = %request.method, "ignored MCP notification");
                    None
                } else {
                    Some(error_response(
                        request.id.clone(),
                        -32601,
                        format!("Method not found: {}", request.method),
                    ))
                }
            }
        }
    }

    fn initialize(&self, request: &Request) -> Response {
        let protocol_version = negotiated_protocol_version(request.params.as_ref());
        Response {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: Some(json!({
                "protocolVersion": protocol_version.as_str(),
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "foxpro-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            error: None,
        }
    }

    fn ping(&self, request: &Request) -> Response {
        Response {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: Some(json!({})),
            error: None,
        }
    }

    fn list_named(&self, request: &Request, field: &str, items: Value) -> Response {
        Response {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: Some(json!({ field: items })),
            error: None,
        }
    }

    fn not_found(&self, request: &Request, message: &str) -> Response {
        error_response(
            request.id.clone(),
            -32602,
            message.to_string(),
        )
    }

    fn shutdown(&self, request: &Request) -> Response {
        Response {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: Some(Value::Null),
            error: None,
        }
    }

    fn tools_list(&self, request: &Request) -> Response {
        Response {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: Some(json!({ "tools": tools::all() })),
            error: None,
        }
    }

    fn tools_call(&self, request: &Request) -> Response {
        let params = match &request.params {
            Some(Value::Object(map)) => map,
            _ => {
                return error_response(
                    request.id.clone(),
                    -32602,
                    "Invalid params: expected object",
                );
            }
        };

        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return error_response(
                    request.id.clone(),
                    -32602,
                    "Invalid params: missing 'name'",
                );
            }
        };

        let arguments = params.get("arguments");
        let ctx = tools::ToolContext {
            config: &self.config,
            sandbox: &self.sandbox,
        };

        match tools::call(name, arguments, ctx) {
            Ok(value) => Response {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                result: Some(json!({
                    "content": [
                        { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }
                    ],
                    "isError": false
                })),
                error: None,
            },
            Err(e) => Response {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                result: Some(json!({
                    "content": [
                        { "type": "text", "text": e.to_string() }
                    ],
                    "isError": true
                })),
                error: None,
            },
        }
    }
}

fn is_notification(id: &Option<Value>) -> bool {
    id.as_ref().is_none_or(Value::is_null)
}

fn negotiated_protocol_version(params: Option<&Value>) -> String {
    params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .filter(|v| {
            !v.is_empty()
                && (v.starts_with("2024-") || v.starts_with("2025-") || v.starts_with("2026-"))
        })
        .unwrap_or("2024-11-05")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use protocol::Request;

    fn test_server() -> Server {
        let dir = std::env::temp_dir().join(format!("foxpro-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            workspace: dir,
            log_level: "error".to_string(),
        };
        Server::new(config).unwrap()
    }

    #[test]
    fn tools_list_uses_input_schema_camel_case() {
        let server = test_server();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "tools/list".to_string(),
            params: Some(json!({})),
        };
        let resp = server.tools_list(&req);
        let tools = resp.result.unwrap()["tools"].clone();
        let first = &tools[0];
        assert!(first.get("inputSchema").is_some());
        assert!(first.get("input_schema").is_none());
    }

    #[test]
    fn ping_returns_empty_object() {
        let server = test_server();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(9)),
            method: "ping".to_string(),
            params: None,
        };
        let resp = server.ping(&req);
        assert_eq!(resp.result.unwrap(), json!({}));
    }
}
