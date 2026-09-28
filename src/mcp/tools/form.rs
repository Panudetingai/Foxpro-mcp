//! Form tools (Phase 4).

use super::{Args, Tool, ToolOutput, blocking, mutate, schema, to_value, unknown};
use crate::designer::{self, DesignerDocument};
use crate::error::{FoxProError, Result};
use crate::form::{self, FormDocument, controls};
use crate::mcp::AppState;
use serde_json::{Map, Value, json};
use std::sync::Arc;

macro_rules! common_write {
    () => {
        "Validates the whole form, backs up .scx/.sct, writes atomically and returns a diff; dry_run returns the diff only. Compiled code (OBJCODE) of changed objects is cleared, so run COMPILE FORM or build with RECOMPILE before DO FORM."
    };
}

fn control_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "description": format!("One of: {}", controls::supported_types().join(", ")) },
            "name": { "type": "string" },
            "parent": { "type": "string", "description": "Container path, e.g. \"Pageframe1.Page1\" (default: the form)" },
            "properties": { "type": "object", "description": "VFP properties, e.g. {\"Left\": 10, \"Caption\": \"Save\", \"ForeColor\": [255,0,0], \"Value\": \"=DATE()\"}" },
            "methods": { "type": "object", "description": "Event code by name, e.g. {\"Click\": \"THISFORM.Release()\"}" }
        },
        "required": ["type"]
    })
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "foxpro.inspect_form",
            description: "Parse an .scx form into JSON: form properties, data environment cursors, every control (type, path, position, size, caption, properties, method names). include_code adds method source.",
            input_schema: schema(
                json!({
                    "file": { "type": "string" },
                    "include_code": { "type": "boolean" }
                }),
                &["file"],
            ),
        },
        Tool {
            name: "foxpro.create_form",
            description: "Create a new .scx/.sct form (with data environment) and optionally its controls in one call. Encoding defaults to Windows-874 when any text is Thai, else Windows-1252.",
            input_schema: schema(
                json!({
                    "file": { "type": "string", "description": "e.g. forms/customer.scx" },
                    "name": { "type": "string", "description": "Form object name (default: file name)" },
                    "caption": { "type": "string" },
                    "width": { "type": "number" },
                    "height": { "type": "number" },
                    "properties": { "type": "object" },
                    "controls": { "type": "array", "items": control_schema() },
                    "encoding": { "type": "string", "enum": ["windows-1252", "windows-874"] },
                    "overwrite": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                }),
                &["file"],
            ),
        },
        Tool {
            name: "foxpro.add_control",
            description: concat!(
                "Add a control to a form. Pages (to a PageFrame), OptionButtons (to an OptionGroup) and Columns (to a Grid) are added as numbered member objects. ",
                common_write!()
            ),
            input_schema: schema(
                json!({
                    "form": { "type": "string" },
                    "type": { "type": "string", "description": format!("One of: {}", controls::supported_types().join(", ")) },
                    "name": { "type": "string" },
                    "parent": { "type": "string" },
                    "properties": { "type": "object" },
                    "methods": { "type": "object" },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean" }
                }),
                &["form", "type"],
            ),
        },
        Tool {
            name: "foxpro.update_control",
            description: concat!(
                "Change properties of a control, page or the form itself (object \"\" or the form name): position, size, caption, value, font, colors ([r,g,b] or \"#RRGGBB\"), visible, enabled, readonly, controlsource, rowsource, format, inputmask, ... Strings starting with '=' are expressions. Setting Name renames the object and its children. `remove` resets properties to their defaults. ",
                common_write!()
            ),
            input_schema: schema(
                json!({
                    "form": { "type": "string" },
                    "object": { "type": "string", "description": "Name or path, e.g. \"txtName\" or \"Pageframe1.Page1.txtName\"" },
                    "properties": { "type": "object" },
                    "remove": { "type": "array", "items": { "type": "string" } },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean" }
                }),
                &["form", "object"],
            ),
        },
        Tool {
            name: "foxpro.remove_control",
            description: concat!(
                "Remove a control and everything inside it (the control must exist). Only the last page/option/column of a container can be removed. ",
                common_write!()
            ),
            input_schema: schema(
                json!({
                    "form": { "type": "string" },
                    "object": { "type": "string" },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean" }
                }),
                &["form", "object"],
            ),
        },
        Tool {
            name: "foxpro.update_method",
            description: concat!(
                "Add or replace one event/method (Click, Init, Valid, ...) of a control or the form, preserving all other methods. Pass only the body (no PROCEDURE/ENDPROC). remove=true deletes the method. Custom form methods are declared automatically. ",
                common_write!()
            ),
            input_schema: schema(
                json!({
                    "form": { "type": "string" },
                    "object": { "type": "string", "description": "Control name/path; \"\" for the form" },
                    "method": { "type": "string" },
                    "code": { "type": "string" },
                    "remove": { "type": "boolean" },
                    "dry_run": { "type": "boolean" },
                    "backup": { "type": "boolean" }
                }),
                &["form", "object", "method"],
            ),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "foxpro.inspect_form"
            | "foxpro.create_form"
            | "foxpro.add_control"
            | "foxpro.update_control"
            | "foxpro.remove_control"
            | "foxpro.update_method"
    )
}

/// (type, name, parent, properties, methods) of one control in create_form.
type ControlArgs = (
    String,
    Option<String>,
    Option<String>,
    Map<String, Value>,
    Map<String, Value>,
);

fn control_args(v: &Value) -> Result<ControlArgs> {
    let o = v
        .as_object()
        .ok_or_else(|| FoxProError::InvalidArgument("each control must be an object".into()))?;
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
    let m = |k: &str| -> Result<Map<String, Value>> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(Map::new()),
            Some(Value::Object(m)) => Ok(m.clone()),
            Some(_) => Err(FoxProError::InvalidArgument(format!(
                "control '{k}' must be an object"
            ))),
        }
    };
    let mut props = m("properties")?;
    // Allow the flat shape from the spec: {"type":"Label","name":"lbl","caption":"x","left":10}.
    for (k, key) in [
        ("caption", "Caption"),
        ("left", "Left"),
        ("top", "Top"),
        ("width", "Width"),
        ("height", "Height"),
        ("value", "Value"),
    ] {
        if let Some(v) = o.get(k)
            && !props.keys().any(|p| p.eq_ignore_ascii_case(key))
        {
            props.insert(key.to_string(), v.clone());
        }
    }
    let control_type = s("type")
        .ok_or_else(|| FoxProError::InvalidArgument("control is missing 'type'".into()))?;
    Ok((control_type, s("name"), s("parent"), props, m("methods")?))
}

pub async fn call(name: &str, a: Args, state: Arc<AppState>) -> Result<ToolOutput> {
    let ws = state.sandbox.workspace().to_path_buf();
    let dry_run = a.flag("dry_run", false)?;
    let backup = a.flag("backup", true)?;

    match name {
        "foxpro.inspect_form" => {
            let path = state
                .sandbox
                .validate(&a.req_path(&["file", "form", "path"])?)?;
            let include_code = a.flag("include_code", false)?;
            blocking(move || {
                let doc = FormDocument::load(&path)?;
                Ok(to_value(&doc.definition(include_code)?)?.into())
            })
            .await
        }
        "foxpro.create_form" => {
            let path = state
                .sandbox
                .resolve(&a.req_path(&["file", "form", "path"])?)?;
            if !crate::fsutil::has_extension(&path, "scx") {
                return Err(FoxProError::InvalidArgument(
                    "form file must end with .scx".into(),
                ));
            }
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Form1".into());
            let form_name = a.str("name")?.map(str::to_string).unwrap_or_else(|| {
                if crate::meta::is_valid_name(&stem) {
                    stem.clone()
                } else {
                    "Form1".into()
                }
            });
            let mut props = a.object("properties")?;
            for (k, key) in [
                ("caption", "Caption"),
                ("width", "Width"),
                ("height", "Height"),
            ] {
                if let Some(v) = a.0.get(k).filter(|v| !v.is_null()) {
                    props.insert(key.into(), v.clone());
                }
            }
            let controls = a.array("controls")?;
            let encoding = a.str("encoding")?.map(str::to_string);
            let overwrite = a.flag("overwrite", false)?;
            mutate(&state, move || {
                if path.exists() && !overwrite {
                    return Err(FoxProError::Conflict(format!(
                        "{} already exists; set overwrite=true to replace it (a backup is kept)",
                        path.display()
                    )));
                }
                // Choose the encoding from all text that will be stored.
                let sample = format!(
                    "{}{}",
                    serde_json::to_string(&props)?,
                    serde_json::to_string(&controls)?
                );
                let enc = form::encoding_for_new(encoding.as_deref(), &sample)?;
                let mut doc = FormDocument::create(&path, &form_name, &props, enc)?;
                let mut added = Vec::new();
                for c in &controls {
                    let (t, n, p, cp, cm) = control_args(c)?;
                    added.push(
                        doc.add_control(&t, n.as_deref(), p.as_deref(), &cp, &cm)?["added"].clone(),
                    );
                }
                let result = json!({
                    "form": form_name,
                    "encoding": doc.encoding_name(),
                    "controls": added,
                });
                Ok(to_value(&designer::commit(&doc, "", &ws, result, dry_run, backup)?)?.into())
            })
            .await
        }
        "foxpro.add_control"
        | "foxpro.update_control"
        | "foxpro.remove_control"
        | "foxpro.update_method" => {
            let path = state
                .sandbox
                .validate(&a.req_path(&["form", "file", "path"])?)?;
            let op = name.to_string();
            mutate(&state, move || {
                let mut doc = FormDocument::load(&path)?;
                let before = doc.render_text();
                let result = match op.as_str() {
                    "foxpro.add_control" => {
                        let control_type = a.req_str("type")?;
                        doc.add_control(
                            control_type,
                            a.str("name")?,
                            a.str("parent")?,
                            &a.object("properties")?,
                            &a.object("methods")?,
                        )?
                    }
                    "foxpro.update_control" => {
                        let props = a.object("properties")?;
                        let remove = a.strings("remove")?;
                        if props.is_empty() && remove.is_empty() {
                            return Err(FoxProError::InvalidArgument(
                                "nothing to change: pass properties and/or remove".into(),
                            ));
                        }
                        doc.update_control(a.req_str("object")?, &props, &remove)?
                    }
                    "foxpro.remove_control" => doc.remove_control(a.req_str("object")?)?,
                    _ => {
                        let remove = a.flag("remove", false)?;
                        let code = if remove {
                            None
                        } else {
                            Some(a.req_str("code")?)
                        };
                        doc.update_method(a.req_str("object")?, a.req_str("method")?, code)?
                    }
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
