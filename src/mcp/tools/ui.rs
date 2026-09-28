//! UI verification tools (Phase 6): launch, screenshot, close.

use super::{Args, Tool, ToolOutput, schema, unknown};
use crate::error::{FoxProError, Result};
use crate::fsutil::display_relative;
use crate::mcp::AppState;
use crate::ui::{self, LaunchTarget, Region};
use base64::Engine as _;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "foxpro.launch",
            description: "Start VFP interactively with a form (.scx → DO FORM), program (.prg/.app), project (.pjx → MODIFY PROJECT) or a built .exe, for visual verification. Returns an instance_id; the process keeps running until foxpro.close (max 4 instances).",
            input_schema: schema(
                json!({
                    "target": { "type": "string", "description": "File to launch" },
                    "dry_run": { "type": "boolean" }
                }),
                &["target"],
            ),
        },
        Tool {
            name: "foxpro.screenshot",
            description: "Capture a PNG of a launched instance's window (or the whole screen) and return it as an image. Optional region (pixels relative to the window) captures a single control. Windows only.",
            input_schema: schema(
                json!({
                    "instance_id": { "type": "integer" },
                    "region": { "type": "object", "properties": {
                        "left": {"type":"integer"}, "top": {"type":"integer"},
                        "width": {"type":"integer"}, "height": {"type":"integer"} } },
                    "delay_ms": { "type": "integer", "description": "Wait before capturing (default 500, max 10000)" },
                    "output": { "type": "string", "description": "PNG path inside the workspace (default .mcp-vfp/screenshots/...)" },
                    "include_image": { "type": "boolean", "description": "Return the image inline (default true)" },
                    "dry_run": { "type": "boolean" }
                }),
                &[],
            ),
        },
        Tool {
            name: "foxpro.close",
            description: "Terminate a launched instance (or all when instance_id is omitted) and return runtime errors it trapped.",
            input_schema: schema(
                json!({
                    "instance_id": { "type": "integer" },
                    "dry_run": { "type": "boolean" }
                }),
                &[],
            ),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(name, "foxpro.launch" | "foxpro.screenshot" | "foxpro.close")
}

fn instance_id(a: &Args) -> Result<Option<u32>> {
    a.u64("instance_id")?
        .map(|n| {
            u32::try_from(n)
                .map_err(|_| FoxProError::InvalidArgument("instance_id is out of range".into()))
        })
        .transpose()
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    let ws = state.sandbox.workspace().to_path_buf();
    let dry_run = a.flag("dry_run", false)?;
    match name {
        "foxpro.launch" => {
            let target = state
                .sandbox
                .validate(&a.req_path(&["target", "file", "path"])?)?;
            let target = LaunchTarget::from_path(&target)?;
            Ok(state
                .instances
                .launch(state.vfp.as_ref(), target, dry_run)
                .await?
                .into())
        }
        "foxpro.screenshot" => {
            let pid = match instance_id(&a)? {
                Some(id) => Some(state.instances.pid_of(id).await?),
                None => None,
            };
            let region = {
                let r = a.object("region")?;
                if r.is_empty() {
                    None
                } else {
                    let get = |k: &str| {
                        r.get(k).and_then(|v| v.as_i64()).ok_or_else(|| {
                            FoxProError::InvalidArgument(format!("region.{k} must be an integer"))
                        })
                    };
                    let region = Region {
                        left: get("left")?,
                        top: get("top")?,
                        width: get("width")?,
                        height: get("height")?,
                    };
                    if region.width <= 0 || region.height <= 0 {
                        return Err(FoxProError::InvalidArgument(
                            "region width/height must be positive".into(),
                        ));
                    }
                    Some(region)
                }
            };
            let output = match a.str("output")? {
                Some(p) => state.sandbox.resolve(std::path::Path::new(p))?,
                None => ui::default_screenshot_path(&ws),
            };
            if !crate::fsutil::has_extension(&output, "png") {
                return Err(FoxProError::InvalidArgument(
                    "output must be a .png file".into(),
                ));
            }
            if dry_run {
                return Ok(json!({
                    "dry_run": true,
                    "output": display_relative(&output, &ws),
                    "script": ui::screenshot_script(&output, pid, region),
                })
                .into());
            }
            let delay = a.u64("delay_ms")?.unwrap_or(500).min(10_000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let size = ui::capture(&output, pid, region).await?;

            let mut out = ToolOutput::from(json!({
                "output": display_relative(&output, &ws),
                "size": size,
            }));
            if a.flag("include_image", true)? {
                let len = tokio::fs::metadata(&output).await?.len();
                if len <= ui::MAX_INLINE_IMAGE_BYTES {
                    let bytes = tokio::fs::read(&output).await?;
                    out.images.push((
                        "image/png".into(),
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    ));
                } else {
                    out.value["note"] = json!("image is too large to inline; open the saved file");
                }
            }
            Ok(out)
        }
        "foxpro.close" => Ok(state
            .instances
            .close(instance_id(&a)?, dry_run)
            .await?
            .into()),
        other => Err(unknown(other)),
    }
}
