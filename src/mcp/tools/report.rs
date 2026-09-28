//! Report tools (Phase 5).

use super::{Args, Tool, ToolOutput, blocking, mutate, schema, to_value, unknown};
use crate::designer::{self, DesignerDocument};
use crate::encoding;
use crate::error::{FoxProError, Result};
use crate::mcp::AppState;
use crate::report::{FieldSpec, ReportDocument};
use serde_json::{Value, json};
use std::sync::Arc;

macro_rules! common_write {
    () => {
        "Positions are pixels (96 dpi) relative to the band. Bands grow automatically to fit. Backs up .frx/.frt, writes atomically, returns a diff; dry_run previews only."
    };
}

fn field_properties() -> Value {
    json!({
        "kind": { "type": "string", "enum": ["field", "label", "line", "box"], "description": "Default: field when expression is given, else label" },
        "band": { "type": "string", "description": "title, page_header, group_header:N, detail (default), group_footer:N, page_footer, summary" },
        "x": { "type": "number" }, "y": { "type": "number" },
        "width": { "type": "number" }, "height": { "type": "number" },
        "expression": { "type": "string", "description": "Field expression, e.g. customer.name" },
        "text": { "type": "string", "description": "Label text" },
        "picture": { "type": "string", "description": "Format/picture, e.g. \"@Z 999,999.99\"" },
        "font": { "type": "object", "properties": { "face": {"type":"string"}, "size": {"type":"number"}, "bold": {"type":"boolean"}, "italic": {"type":"boolean"}, "underline": {"type":"boolean"} } },
        "total": { "type": "string", "enum": ["none", "count", "sum", "average", "lowest", "highest", "stddev", "variance"] },
        "align": { "type": "string", "enum": ["left", "right", "center"] },
        "stretch": { "type": "boolean" }
    })
}

pub fn tools() -> Vec<Tool> {
    let mut add_props = field_properties();
    add_props["report"] = json!({ "type": "string" });
    add_props["dry_run"] = json!({ "type": "boolean" });
    add_props["backup"] = json!({ "type": "boolean" });
    let mut upd_props = add_props.clone();
    upd_props["id"] = json!({ "type": "string", "description": "Object id from inspect_report (or a unique expression/label text)" });

    vec![
        Tool {
            name: "foxpro.inspect_report",
            description: "Parse an .frx report into JSON: page setup (orientation, paper size, margin), bands (with group expressions) and every field/label/line/box with band-relative pixel positions, fonts, pictures and totals.",
            input_schema: schema(json!({ "file": { "type": "string" } }), &["file"]),
        },
        Tool {
            name: "foxpro.create_report",
            description: "Create a new .frx/.frt report with page setup, bands, groups and optional initial fields/labels. Band heights in pixels.",
            input_schema: schema(
                json!({
                    "file": { "type": "string" },
                    "orientation": { "type": "string", "enum": ["portrait", "landscape"] },
                    "paper_size": { "type": "string", "enum": ["letter", "legal", "a3", "a4", "a5"] },
                    "margins": { "type": "object", "properties": { "left": {"type":"number"}, "right": {"type":"number"} } },
                    "title_height": { "type": "number", "description": "Adds a title band" },
                    "page_header_height": { "type": "number" },
                    "detail_height": { "type": "number" },
                    "page_footer_height": { "type": "number" },
                    "summary_height": { "type": "number", "description": "Adds a summary band" },
                    "groups": { "type": "array", "items": { "type": "object", "properties": {
                        "expression": {"type":"string"}, "header_height": {"type":"number"},
                        "footer_height": {"type":"number"}, "new_page": {"type":"boolean"} }, "required": ["expression"] } },
                    "font": { "type": "object", "properties": { "face": {"type":"string"}, "size": {"type":"number"} } },
                    "fields": { "type": "array", "items": { "type": "object", "properties": field_properties() } },
                    "encoding": { "type": "string", "enum": ["windows-1252", "windows-874"] },
                    "overwrite": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                }),
                &["file"],
            ),
        },
        Tool {
            name: "foxpro.add_report_field",
            description: concat!(
                "Add a field, label, line or box to a report. ",
                common_write!()
            ),
            input_schema: schema(add_props, &["report"]),
        },
        Tool {
            name: "foxpro.update_report_field",
            description: concat!(
                "Move, resize or change a report object (expression, text, band, font, picture, total, alignment). ",
                common_write!()
            ),
            input_schema: schema(upd_props, &["report", "id"]),
        },
        Tool {
            name: "foxpro.remove_report_field",
            description: "Remove one report object by id (must exist). Backs up and returns a diff; dry_run previews only.",
            input_schema: schema(
                json!({
                    "report": { "type": "string" },
                    "id": { "type": "string" },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean" }
                }),
                &["report", "id"],
            ),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "foxpro.inspect_report"
            | "foxpro.create_report"
            | "foxpro.add_report_field"
            | "foxpro.update_report_field"
            | "foxpro.remove_report_field"
    )
}

fn px(a: &Args, key: &str, default: f64) -> Result<f64> {
    let v = a.f64(key)?.unwrap_or(default);
    if !(0.0..=5000.0).contains(&v) {
        return Err(FoxProError::InvalidArgument(format!(
            "{key} must be between 0 and 5000"
        )));
    }
    Ok(v)
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    let ws = state.sandbox.workspace().to_path_buf();
    let dry_run = a.flag("dry_run", false)?;
    let backup = a.flag("backup", true)?;

    match name {
        "foxpro.inspect_report" => {
            let path = state
                .sandbox
                .validate(&a.req_path(&["file", "report", "path"])?)?;
            blocking(move || Ok(to_value(&ReportDocument::load(&path)?.definition()?)?.into()))
                .await
        }
        "foxpro.create_report" => {
            let path = state
                .sandbox
                .resolve(&a.req_path(&["file", "report", "path"])?)?;
            if !crate::fsutil::has_extension(&path, "frx") {
                return Err(FoxProError::InvalidArgument(
                    "report file must end with .frx".into(),
                ));
            }
            let margins = a.object("margins")?;
            let margin = |k: &str| -> Result<f64> {
                match margins.get(k) {
                    None => Ok(48.0),
                    Some(v) => v
                        .as_f64()
                        .filter(|f| (0.0..=1000.0).contains(f))
                        .ok_or_else(|| {
                            FoxProError::InvalidArgument(format!("margins.{k} must be 0..1000"))
                        }),
                }
            };
            let (left, right) = (margin("left")?, margin("right")?);
            let mut groups = Vec::new();
            for g in a.array("groups")? {
                let expr = g
                    .get("expression")
                    .and_then(Value::as_str)
                    .filter(|e| !e.trim().is_empty())
                    .ok_or_else(|| {
                        FoxProError::InvalidArgument("each group needs an expression".into())
                    })?;
                groups.push((
                    expr.to_string(),
                    g.get("header_height")
                        .and_then(Value::as_f64)
                        .unwrap_or(25.0),
                    g.get("footer_height")
                        .and_then(Value::as_f64)
                        .unwrap_or(25.0),
                    g.get("new_page").and_then(Value::as_bool).unwrap_or(false),
                ));
            }
            let bands = ReportDocument::band_layout(
                a.f64("title_height")?,
                px(&a, "page_header_height", 60.0)?,
                &groups,
                px(&a, "detail_height", 25.0)?,
                px(&a, "page_footer_height", 40.0)?,
                a.f64("summary_height")?,
            );
            let fields = a
                .array("fields")?
                .iter()
                .map(FieldSpec::from_json)
                .collect::<Result<Vec<_>>>()?;
            let font = a.object("font")?;
            let sample = serde_json::to_string(&a.0)?;
            let enc = match a.str("encoding")? {
                Some(l) => encoding::lookup(l)?,
                None => encoding::ansi_for_text(&sample),
            };
            let default_face = if enc == encoding_rs::WINDOWS_874 {
                "Tahoma"
            } else {
                "Arial"
            };
            let face = font
                .get("face")
                .and_then(Value::as_str)
                .unwrap_or(default_face)
                .to_string();
            let size = font.get("size").and_then(Value::as_f64).unwrap_or(
                if enc == encoding_rs::WINDOWS_874 {
                    10.0
                } else {
                    9.0
                },
            );
            let orientation = a.str("orientation")?.unwrap_or("portrait").to_string();
            let paper = a.str("paper_size")?.unwrap_or("a4").to_string();
            let overwrite = a.flag("overwrite", false)?;
            mutate(&state, move || {
                if path.exists() && !overwrite {
                    return Err(FoxProError::Conflict(format!(
                        "{} already exists; set overwrite=true to replace it (a backup is kept)",
                        path.display()
                    )));
                }
                let mut doc = ReportDocument::create(
                    &path,
                    enc,
                    &orientation,
                    &paper,
                    left,
                    right,
                    &bands,
                    &face,
                    size,
                )?;
                let mut added = Vec::new();
                for f in &fields {
                    added.push(doc.add_field(f)?["added"].clone());
                }
                let result = json!({ "encoding": enc.name().to_lowercase(), "fields": added });
                Ok(to_value(&designer::commit(&doc, "", &ws, result, dry_run, backup)?)?.into())
            })
            .await
        }
        "foxpro.add_report_field" | "foxpro.update_report_field" | "foxpro.remove_report_field" => {
            let path = state
                .sandbox
                .validate(&a.req_path(&["report", "file", "path"])?)?;
            let op = name.to_string();
            let spec = FieldSpec::from_json(&Value::Object(a.0.clone()))?;
            mutate(&state, move || {
                let mut doc = ReportDocument::load(&path)?;
                let before = doc.render_text();
                let result = match op.as_str() {
                    "foxpro.add_report_field" => doc.add_field(&spec)?,
                    "foxpro.update_report_field" => doc.update_field(a.req_str("id")?, &spec)?,
                    _ => doc.remove_field(a.req_str("id")?)?,
                };
                Ok(to_value(&designer::commit(
                    &doc, &before, &ws, result, dry_run, backup,
                )?)?
                .into())
            })
            .await
        }
        other => Err(unknown(other)),
    }
}
