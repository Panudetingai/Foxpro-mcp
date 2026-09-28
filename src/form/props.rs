//! The PROPERTIES memo of SCX/VCX records: one `Name = value` per line
//! (CRLF separated). Character values are stored as quoted literals,
//! expressions as `(expr)`, colors as `r,g,b`, logicals as `.T.`/`.F.`.
//!
//! Parsing is lossless: lines that do not look like `key = value` are kept
//! verbatim so re-serialising never damages data we do not understand.

use crate::error::{FoxProError, Result};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
enum Line {
    Pair { key: String, value: String },
    Raw(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PropList {
    lines: Vec<Line>,
}

const NUMERIC_PROPS: &[&str] = &[
    "left",
    "top",
    "width",
    "height",
    "fontsize",
    "tabindex",
    "borderstyle",
    "backstyle",
    "specialeffect",
    "alignment",
    "maxlength",
    "scrollbars",
    "columncount",
    "buttoncount",
    "pagecount",
    "rowsourcetype",
    "boundcolumn",
    "style",
    "interval",
    "windowtype",
    "borderwidth",
    "drawmode",
    "drawstyle",
    "fillstyle",
    "curvature",
    "rotation",
    "scalemode",
    "showwindow",
    "windowstate",
    "mousepointer",
    "displaycount",
    "tabstyle",
    "tabstretch",
    "activepage",
    "titlebar",
    "partition",
    "rowheight",
    "headerheight",
    "gridlines",
    "increment",
    "keyboardhighvalue",
    "keyboardlowvalue",
    "spinnerhighvalue",
    "spinnerlowvalue",
    "stretch",
    "picturemargin",
    "margin",
    "datasessionid",
    "datasession",
    "buffermode",
    "fontcharset",
    "anchor",
];

const LOGICAL_PROPS: &[&str] = &[
    "visible",
    "enabled",
    "readonly",
    "fontbold",
    "fontitalic",
    "fontunderline",
    "fontstrikethru",
    "autosize",
    "wordwrap",
    "autocenter",
    "closable",
    "controlbox",
    "maxbutton",
    "minbutton",
    "movable",
    "docreate",
    "default",
    "cancel",
    "tabstop",
    "themes",
    "alwaysontop",
    "keypreview",
    "showtips",
    "sorted",
    "multiselect",
    "deletemark",
    "recordmark",
    "allowaddnew",
    "erasepage",
    "integralheight",
    "centered",
    "autoopentables",
    "autoclosetables",
    "fontshadow",
    "fontoutline",
    "hideselection",
    "nofocusrect",
    "allowheadersizing",
    "allowrowsizing",
    "enablehyperlinks",
    "desktop",
    "tabs",
    "selectonentry",
];

fn is_numeric_prop(key: &str) -> bool {
    let k = base_key(key).to_ascii_lowercase();
    NUMERIC_PROPS.iter().any(|p| p.eq_ignore_ascii_case(&k))
}

fn is_logical_prop(key: &str) -> bool {
    let k = base_key(key).to_ascii_lowercase();
    LOGICAL_PROPS.contains(&k.as_str())
}

fn is_color_prop(key: &str) -> bool {
    base_key(key).to_ascii_lowercase().ends_with("color")
}

/// `Page1.Caption` → `Caption`.
fn base_key(key: &str) -> &str {
    key.rsplit('.').next().unwrap_or(key)
}

impl PropList {
    pub fn parse(text: &str) -> Self {
        let body = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        if body.is_empty() {
            return Self::default();
        }
        let lines = body
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .map(|l| match l.split_once(" = ") {
                Some((k, v))
                    if !k.trim().is_empty()
                        && !k.trim().contains(' ')
                        && k.trim()
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') =>
                {
                    Line::Pair {
                        key: k.trim().to_string(),
                        value: v.to_string(),
                    }
                }
                _ => Line::Raw(l.to_string()),
            })
            .collect();
        Self { lines }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Pair { key, value } => {
                    out.push_str(key);
                    out.push_str(" = ");
                    out.push_str(value);
                }
                Line::Raw(r) => out.push_str(r),
            }
            out.push_str("\r\n");
        }
        out
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().find_map(|l| match l {
            Line::Pair { key: k, value } if k.eq_ignore_ascii_case(key) => Some(value.as_str()),
            _ => None,
        })
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.lines.iter().filter_map(|l| match l {
            Line::Pair { key, value } => Some((key.as_str(), value.as_str())),
            Line::Raw(_) => None,
        })
    }

    /// Set a raw (already encoded) value. Existing keys are updated in place;
    /// new object properties go before `Name`, member properties at the end.
    pub fn set_raw(&mut self, key: &str, value: String) {
        for line in &mut self.lines {
            if let Line::Pair { key: k, value: v } = line
                && k.eq_ignore_ascii_case(key)
            {
                *v = value;
                return;
            }
        }
        let new = Line::Pair {
            key: key.to_string(),
            value,
        };
        let position = if key.contains('.') {
            // Members of the same object stay together, before later members.
            let prefix = format!("{}.", key.split('.').next().unwrap_or(""));
            self.lines
                .iter()
                .rposition(|l| matches!(l, Line::Pair { key: k, .. } if k.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase())))
                .map(|p| p + 1)
                .unwrap_or(self.lines.len())
        } else {
            self.lines
                .iter()
                .position(|l| match l {
                    Line::Pair { key: k, .. } => k.eq_ignore_ascii_case("Name") || k.contains('.'),
                    Line::Raw(_) => false,
                })
                .unwrap_or(self.lines.len())
        };
        self.lines.insert(position, new);
    }

    /// Encode a JSON value for `key` and store it.
    pub fn set(&mut self, key: &str, value: &Value) -> Result<()> {
        let raw = encode_value(key, value)?;
        self.set_raw(key, raw);
        Ok(())
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.lines.len();
        self.lines
            .retain(|l| !matches!(l, Line::Pair { key: k, .. } if k.eq_ignore_ascii_case(key)));
        before != self.lines.len()
    }

    /// Remove every `prefix.*` member property.
    pub fn remove_member(&mut self, member: &str) {
        let prefix = format!("{}.", member.to_ascii_lowercase());
        self.lines.retain(|l| {
            !matches!(l, Line::Pair { key, .. } if key.to_ascii_lowercase().starts_with(&prefix))
        });
    }

    /// Member names declared as `X.Name = "X"`.
    pub fn members(&self) -> Vec<String> {
        self.pairs()
            .filter_map(|(k, v)| {
                let (member, prop) = k.rsplit_once('.')?;
                (prop.eq_ignore_ascii_case("Name") && !member.contains('.'))
                    .then(|| decode_string(v).unwrap_or_else(|| member.to_string()))
            })
            .collect()
    }

    /// Properties of one member (`Page1.Caption` → `Caption`).
    pub fn member_props(&self, member: &str) -> Map<String, Value> {
        let prefix = format!("{}.", member.to_ascii_lowercase());
        self.pairs()
            .filter(|(k, _)| k.to_ascii_lowercase().starts_with(&prefix))
            .map(|(k, v)| (k[prefix.len()..].to_string(), decode_value(v)))
            .collect()
    }

    /// Object-level properties (members excluded) as JSON.
    pub fn to_json(&self) -> Map<String, Value> {
        self.pairs()
            .filter(|(k, _)| !k.contains('.'))
            .map(|(k, v)| (k.to_string(), decode_value(v)))
            .collect()
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.trim().parse().ok())
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.get(key).and_then(decode_string)
    }
}

/// Strip VFP string delimiters: `"x"`, `'x'` or `[x]`.
pub fn decode_string(raw: &str) -> Option<String> {
    let t = raw.trim();
    let mut chars = t.chars();
    let first = chars.next()?;
    let last = t.chars().last()?;
    let ok = t.len() >= 2 && matches!((first, last), ('"', '"') | ('\'', '\'') | ('[', ']'));
    ok.then(|| t[1..t.len() - 1].to_string())
}

/// Convert a stored raw value to JSON.
pub fn decode_value(raw: &str) -> Value {
    let t = raw.trim();
    if let Some(s) = decode_string(t) {
        return json!(s);
    }
    match t.to_ascii_uppercase().as_str() {
        ".T." => return json!(true),
        ".F." => return json!(false),
        ".NULL." => return Value::Null,
        _ => {}
    }
    if let Ok(n) = t.parse::<i64>() {
        return json!(n);
    }
    if t.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        && let Ok(n) = t.parse::<f64>()
    {
        return json!(n);
    }
    if t.starts_with('(') && t.ends_with(')') {
        return json!(format!("={}", &t[1..t.len() - 1]));
    }
    json!(t)
}

fn quote(s: &str) -> Result<String> {
    if s.contains('\r') || s.contains('\n') {
        return Err(FoxProError::Validation(
            "property values cannot contain line breaks".into(),
        ));
    }
    if !s.contains('"') {
        Ok(format!("\"{s}\""))
    } else if !s.contains('\'') {
        Ok(format!("'{s}'"))
    } else if !s.contains(']') {
        Ok(format!("[{s}]"))
    } else {
        Err(FoxProError::Validation(format!(
            "value {s:?} contains every FoxPro string delimiter"
        )))
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn rgb_from_int(n: i64) -> String {
    format!("{},{},{}", n & 0xFF, (n >> 8) & 0xFF, (n >> 16) & 0xFF)
}

fn parse_color(s: &str) -> Option<String> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix('#')
        && hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Some(format!(
            "{},{},{}",
            (v >> 16) & 0xFF,
            (v >> 8) & 0xFF,
            v & 0xFF
        ));
    }
    let parts: Vec<&str> = t.split(',').map(str::trim).collect();
    if parts.len() == 3 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        return Some(parts.join(","));
    }
    if let Ok(n) = t.parse::<i64>()
        && (0..=0xFF_FFFF).contains(&n)
    {
        return Some(rgb_from_int(n));
    }
    None
}

/// Encode a JSON value as a stored property value.
///
/// * booleans → `.T.`/`.F.`, null → `.NULL.`, numbers as-is
/// * `[r, g, b]` arrays, `"#RRGGBB"` and `"r,g,b"` for color properties
/// * strings starting with `=` are expressions → `(expr)`
/// * other strings are quoted, except numeric/logical properties given as text
pub fn encode_value(key: &str, value: &Value) -> Result<String> {
    let invalid = |why: &str| FoxProError::Validation(format!("property {key}: {why}"));
    match value {
        Value::Bool(b) => Ok(if *b { ".T." } else { ".F." }.to_string()),
        Value::Null => Ok(".NULL.".to_string()),
        Value::Number(n) => {
            let f = n.as_f64().ok_or_else(|| invalid("number out of range"))?;
            if is_color_prop(key) && f.fract() == 0.0 && (0.0..=16_777_215.0).contains(&f) {
                return Ok(rgb_from_int(f as i64));
            }
            Ok(format_number(f))
        }
        Value::Array(items) => {
            if items.len() == 3 && items.iter().all(|v| v.as_u64().is_some_and(|n| n <= 255)) {
                Ok(items
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(","))
            } else {
                Err(invalid("arrays are only accepted as [r, g, b] colors"))
            }
        }
        Value::Object(_) => Err(invalid("objects are not valid property values")),
        Value::String(s) => {
            if let Some(expr) = s.strip_prefix('=') {
                let expr = expr.trim();
                if expr.is_empty() || expr.contains('\r') || expr.contains('\n') {
                    return Err(invalid("expression must be a single non-empty line"));
                }
                return Ok(format!("({expr})"));
            }
            if is_color_prop(key)
                && let Some(c) = parse_color(s)
            {
                return Ok(c);
            }
            if is_numeric_prop(key) {
                return s
                    .trim()
                    .parse::<f64>()
                    .map(format_number)
                    .map_err(|_| invalid(&format!("expected a number, got {s:?}")));
            }
            if is_logical_prop(key) {
                return match s.trim().to_ascii_lowercase().as_str() {
                    ".t." | "true" | "yes" | "1" => Ok(".T.".into()),
                    ".f." | "false" | "no" | "0" => Ok(".F.".into()),
                    _ => Err(invalid(&format!("expected a logical value, got {s:?}"))),
                };
            }
            quote(s)
        }
    }
}

/// Validate geometry-related properties.
pub fn validate_geometry(props: &PropList, path: &str) -> Result<()> {
    for key in ["Left", "Top", "Width", "Height"] {
        if let Some(raw) = props.get(key) {
            let n: f64 = raw.trim().parse().map_err(|_| {
                FoxProError::Validation(format!("{path}.{key} must be numeric, got {raw}"))
            })?;
            if !(-32768.0..=32767.0).contains(&n) {
                return Err(FoxProError::Validation(format!(
                    "{path}.{key} = {n} is outside the valid range -32768..32767"
                )));
            }
            if (key == "Width" || key == "Height") && n < 0.0 {
                return Err(FoxProError::Validation(format!(
                    "{path}.{key} must not be negative"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Top = 0\r\nLeft = 0\r\nHeight = 250\r\nWidth = 375\r\nDoCreate = .T.\r\nCaption = \"Form1\"\r\nName = \"Form1\"\r\n";

    #[test]
    fn roundtrip_is_lossless() {
        let p = PropList::parse(SAMPLE);
        assert_eq!(p.to_text(), SAMPLE);
        let odd = "Weird line without equals\r\nName = \"x\"\r\n";
        assert_eq!(PropList::parse(odd).to_text(), odd);
    }

    #[test]
    fn set_inserts_before_name() {
        let mut p = PropList::parse(SAMPLE);
        p.set("BackColor", &json!([255, 0, 0])).unwrap();
        p.set("Caption", &json!("Customer")).unwrap();
        let text = p.to_text();
        assert!(text.contains("BackColor = 255,0,0\r\nName = \"Form1\""));
        assert!(text.contains("Caption = \"Customer\""));
    }

    #[test]
    fn encodes_by_property_kind() {
        assert_eq!(encode_value("Left", &json!("12")).unwrap(), "12");
        assert_eq!(encode_value("Visible", &json!("false")).unwrap(), ".F.");
        assert_eq!(
            encode_value("ForeColor", &json!("#FF0000")).unwrap(),
            "255,0,0"
        );
        assert_eq!(encode_value("BackColor", &json!(255)).unwrap(), "255,0,0");
        assert_eq!(
            encode_value("Value", &json!("=DATE()")).unwrap(),
            "(DATE())"
        );
        assert_eq!(
            encode_value("Caption", &json!("say \"hi\"")).unwrap(),
            "'say \"hi\"'"
        );
        assert!(encode_value("Left", &json!("abc")).is_err());
        assert!(encode_value("Caption", &json!("a\nb")).is_err());
    }

    #[test]
    fn decodes_values() {
        assert_eq!(decode_value("\"บันทึก\""), json!("บันทึก"));
        assert_eq!(decode_value(".T."), json!(true));
        assert_eq!(decode_value("24"), json!(24));
        assert_eq!(decode_value("255,255,255"), json!("255,255,255"));
        assert_eq!(decode_value("(date())"), json!("=date()"));
    }

    #[test]
    fn members() {
        let p = PropList::parse(
            "PageCount = 2\r\nName = \"Pageframe1\"\r\nPage1.Caption = \"A\"\r\nPage1.Name = \"Page1\"\r\nPage2.Name = \"Page2\"\r\n",
        );
        assert_eq!(p.members(), vec!["Page1", "Page2"]);
        assert_eq!(p.member_props("Page1")["Caption"], json!("A"));
        assert!(!p.to_json().contains_key("Page1.Caption"));
    }
}
