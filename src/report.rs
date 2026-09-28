//! Visual FoxPro reports (.frx/.frt).
//!
//! An FRX is a VFP table: one header record (OBJTYPE 1, page setup), one
//! record per band (OBJTYPE 9, in print order), one per layout object
//! (5 = label, 6 = line, 7 = box, 8 = field, 17 = picture), font resources
//! (23) and the data environment (25/26).
//!
//! Positions are stored in 1/10000 inch ("FRU") and are absolute from the top
//! of the layout, including a 2083.333 FRU separator bar after every band.
//! The agent-facing API uses pixels at 96 DPI relative to the band, like the
//! Report Designer.

use crate::dbf::{FieldDef, Table};
use crate::designer::DesignerDocument;
use crate::error::{FoxProError, Result};
use crate::fsutil;
use crate::meta;
use crate::vfp;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const FRU_PER_PIXEL: f64 = 10_000.0 / 96.0;
const BAND_BAR: f64 = 2083.333;

const OBJ_HEADER: i64 = 1;
const OBJ_LABEL: i64 = 5;
const OBJ_LINE: i64 = 6;
const OBJ_BOX: i64 = 7;
const OBJ_FIELD: i64 = 8;
const OBJ_BAND: i64 = 9;
const OBJ_PICTURE: i64 = 17;
const OBJ_FONT: i64 = 23;
const OBJ_DATAENV: i64 = 25;

const BAND_TITLE: i64 = 0;
const BAND_PAGE_HEADER: i64 = 1;
const BAND_COLUMN_HEADER: i64 = 2;
const BAND_GROUP_HEADER: i64 = 3;
const BAND_DETAIL: i64 = 4;
const BAND_GROUP_FOOTER: i64 = 5;
const BAND_COLUMN_FOOTER: i64 = 6;
const BAND_PAGE_FOOTER: i64 = 7;
const BAND_SUMMARY: i64 = 8;

/// A band to create: (band code, height in pixels, group expression + new page).
pub type BandSpec = (i64, f64, Option<(String, bool)>);

/// The FRX table structure written by Visual FoxPro 6–9.
pub fn frx_fields() -> Vec<FieldDef> {
    let spec: &[(&str, char, u8, u8)] = &[
        ("PLATFORM", 'C', 8, 0),
        ("UNIQUEID", 'C', 10, 0),
        ("TIMESTAMP", 'N', 10, 0),
        ("OBJTYPE", 'N', 2, 0),
        ("OBJCODE", 'N', 3, 0),
        ("NAME", 'M', 4, 0),
        ("EXPR", 'M', 4, 0),
        ("VPOS", 'N', 9, 3),
        ("HPOS", 'N', 9, 3),
        ("HEIGHT", 'N', 9, 3),
        ("WIDTH", 'N', 9, 3),
        ("STYLE", 'M', 4, 0),
        ("PICTURE", 'M', 4, 0),
        ("ORDER", 'M', 4, 0),
        ("UNIQUE", 'L', 1, 0),
        ("COMMENT", 'M', 4, 0),
        ("ENVIRON", 'L', 1, 0),
        ("BOXCHAR", 'C', 1, 0),
        ("FILLCHAR", 'C', 1, 0),
        ("TAG", 'M', 4, 0),
        ("TAG2", 'M', 4, 0),
        ("PENRED", 'N', 5, 0),
        ("PENGREEN", 'N', 5, 0),
        ("PENBLUE", 'N', 5, 0),
        ("FILLRED", 'N', 5, 0),
        ("FILLGREEN", 'N', 5, 0),
        ("FILLBLUE", 'N', 5, 0),
        ("PENSIZE", 'N', 5, 0),
        ("PENPAT", 'N', 5, 0),
        ("FILLPAT", 'N', 5, 0),
        ("FONTFACE", 'M', 4, 0),
        ("FONTSTYLE", 'N', 3, 0),
        ("FONTSIZE", 'N', 3, 0),
        ("MODE", 'N', 3, 0),
        ("RULER", 'N', 1, 0),
        ("RULERLINES", 'N', 1, 0),
        ("GRID", 'L', 1, 0),
        ("GRIDV", 'N', 2, 0),
        ("GRIDH", 'N', 2, 0),
        ("FLOAT", 'L', 1, 0),
        ("STRETCH", 'L', 1, 0),
        ("STRETCHTOP", 'L', 1, 0),
        ("TOP", 'L', 1, 0),
        ("BOTTOM", 'L', 1, 0),
        ("SUPTYPE", 'N', 1, 0),
        ("SUPREST", 'N', 1, 0),
        ("NOREPEAT", 'L', 1, 0),
        ("RESETRPT", 'N', 2, 0),
        ("PAGEBREAK", 'L', 1, 0),
        ("COLBREAK", 'L', 1, 0),
        ("RESETPAGE", 'L', 1, 0),
        ("GENERAL", 'N', 3, 0),
        ("SPACING", 'N', 3, 0),
        ("DOUBLE", 'L', 1, 0),
        ("SWAPHEADER", 'L', 1, 0),
        ("SWAPFOOTER", 'L', 1, 0),
        ("EJECTBEFOR", 'L', 1, 0),
        ("EJECTAFTER", 'L', 1, 0),
        ("PLAIN", 'L', 1, 0),
        ("SUMMARY", 'L', 1, 0),
        ("ADDALIAS", 'L', 1, 0),
        ("OFFSET", 'N', 3, 0),
        ("TOPMARGIN", 'N', 3, 0),
        ("BOTMARGIN", 'N', 3, 0),
        ("TOTALTYPE", 'N', 2, 0),
        ("RESETTOTAL", 'N', 2, 0),
        ("RESOID", 'N', 3, 0),
        ("CURPOS", 'L', 1, 0),
        ("SUPALWAYS", 'L', 1, 0),
        ("SUPOVFLOW", 'L', 1, 0),
        ("SUPRPCOL", 'N', 1, 0),
        ("SUPGROUP", 'N', 2, 0),
        ("SUPVALCHNG", 'L', 1, 0),
        ("SUPEXPR", 'M', 4, 0),
        ("USER", 'M', 4, 0),
    ];
    spec.iter()
        .map(|(n, k, l, d)| FieldDef::new(n, *k, *l, *d))
        .collect()
}

fn px_to_fru(px: f64) -> f64 {
    (px * FRU_PER_PIXEL * 1000.0).round() / 1000.0
}

fn fru_to_px(fru: f64) -> f64 {
    (fru / FRU_PER_PIXEL * 100.0).round() / 100.0
}

fn band_name(code: i64) -> &'static str {
    match code {
        BAND_TITLE => "title",
        BAND_PAGE_HEADER => "page_header",
        BAND_COLUMN_HEADER => "column_header",
        BAND_GROUP_HEADER => "group_header",
        BAND_DETAIL => "detail",
        BAND_GROUP_FOOTER => "group_footer",
        BAND_COLUMN_FOOTER => "column_footer",
        BAND_PAGE_FOOTER => "page_footer",
        BAND_SUMMARY => "summary",
        _ => "unknown",
    }
}

fn kind_name(objtype: i64) -> &'static str {
    match objtype {
        OBJ_LABEL => "label",
        OBJ_LINE => "line",
        OBJ_BOX => "box",
        OBJ_FIELD => "field",
        OBJ_PICTURE => "picture",
        _ => "other",
    }
}

fn total_code(name: &str) -> Result<f64> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "" | "none" => 0.0,
        "count" => 1.0,
        "sum" => 2.0,
        "average" | "avg" => 3.0,
        "lowest" | "min" => 4.0,
        "highest" | "max" => 5.0,
        "stddev" => 6.0,
        "variance" => 7.0,
        other => {
            return Err(FoxProError::InvalidArgument(format!(
                "unknown total {other:?} (none, count, sum, average, lowest, highest, stddev, variance)"
            )));
        }
    })
}

fn total_name(code: i64) -> Option<&'static str> {
    match code {
        1 => Some("count"),
        2 => Some("sum"),
        3 => Some("average"),
        4 => Some("lowest"),
        5 => Some("highest"),
        6 => Some("stddev"),
        7 => Some("variance"),
        _ => None,
    }
}

/// Windows paper size codes (DMPAPER_*) and dimensions in inches.
fn paper(name: &str) -> Result<(i64, f64, f64)> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "letter" => (1, 8.5, 11.0),
        "legal" => (5, 8.5, 14.0),
        "a3" => (8, 11.69, 16.54),
        "a4" => (9, 8.27, 11.69),
        "a5" => (11, 5.83, 8.27),
        other => {
            return Err(FoxProError::InvalidArgument(format!(
                "unknown paper size {other:?} (letter, legal, a3, a4, a5)"
            )));
        }
    })
}

fn paper_name(code: i64) -> Option<&'static str> {
    match code {
        1 => Some("letter"),
        5 => Some("legal"),
        8 => Some("a3"),
        9 => Some("a4"),
        11 => Some("a5"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Domain model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct BandDefinition {
    #[serde(rename = "type")]
    pub band_type: &'static str,
    /// Reference usable as `band` argument, e.g. `group_header:1`.
    pub reference: String,
    pub height: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_expression: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new_page: bool,
}

#[derive(Debug, Serialize)]
pub struct FontDefinition {
    pub face: String,
    pub size: f64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
}

#[derive(Debug, Serialize)]
pub struct ReportField {
    pub id: String,
    pub kind: &'static str,
    pub band: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<FontDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stretch: bool,
}

#[derive(Debug, Serialize)]
pub struct PageSetup {
    pub orientation: &'static str,
    pub paper_size: Option<String>,
    pub left_margin: f64,
    pub columns: i64,
}

#[derive(Debug, Serialize)]
pub struct ReportDefinition {
    pub file: String,
    pub encoding: String,
    pub unit: &'static str,
    pub page: PageSetup,
    pub bands: Vec<BandDefinition>,
    pub fields: Vec<ReportField>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Options for new report objects and updates (all optional on update).
#[derive(Debug, Default, Clone)]
pub struct FieldSpec {
    pub kind: Option<String>,
    pub band: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub expression: Option<String>,
    pub text: Option<String>,
    pub picture: Option<String>,
    pub font_face: Option<String>,
    pub font_size: Option<f64>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub total: Option<String>,
    pub align: Option<String>,
    pub stretch: Option<bool>,
}

impl FieldSpec {
    pub fn from_json(v: &Value) -> Result<Self> {
        let obj = v
            .as_object()
            .ok_or_else(|| FoxProError::InvalidArgument("field must be an object".into()))?;
        let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
        let n = |k: &str| -> Result<Option<f64>> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_f64()
                    .filter(|f| f.is_finite())
                    .map(Some)
                    .ok_or_else(|| FoxProError::InvalidArgument(format!("{k} must be a number"))),
            }
        };
        let b = |k: &str| obj.get(k).and_then(Value::as_bool);
        let font = obj.get("font").and_then(Value::as_object);
        let fs = |k: &str| {
            font.and_then(|f| f.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let fb = |k: &str| font.and_then(|f| f.get(k)).and_then(Value::as_bool);
        Ok(Self {
            kind: s("kind").or_else(|| s("type")),
            band: s("band"),
            x: n("x")?,
            y: n("y")?,
            width: n("width")?,
            height: n("height")?,
            expression: s("expression"),
            text: s("text").or_else(|| s("caption")),
            picture: s("picture").or_else(|| s("format")),
            font_face: fs("face").or_else(|| s("font_face")),
            font_size: font
                .and_then(|f| f.get("size"))
                .and_then(Value::as_f64)
                .or(n("font_size")?),
            bold: fb("bold").or_else(|| b("bold")),
            italic: fb("italic").or_else(|| b("italic")),
            underline: fb("underline").or_else(|| b("underline")),
            total: s("total"),
            align: s("align"),
            stretch: b("stretch"),
        })
    }
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Band {
    row: usize,
    code: i64,
    start: f64,
    height: f64,
    reference: String,
}

pub struct ReportDocument {
    path: PathBuf,
    table: Table,
}

fn escape_label(text: &str) -> Result<String> {
    vfp_literal(text)
}

fn vfp_literal(text: &str) -> Result<String> {
    if text.contains('\r') || text.contains('\n') {
        return Err(FoxProError::Validation(
            "label text cannot contain line breaks".into(),
        ));
    }
    let q = vfp::quote_str(text);
    if q.starts_with('[') && text.contains(']') {
        return Err(FoxProError::Validation(format!(
            "text {text:?} contains every FoxPro string delimiter"
        )));
    }
    Ok(q)
}

fn unquote(expr: &str) -> String {
    crate::form::props::decode_string(expr).unwrap_or_else(|| expr.to_string())
}

impl ReportDocument {
    pub fn load(path: &Path) -> Result<Self> {
        if !fsutil::has_extension(path, "frx") && !fsutil::has_extension(path, "lbx") {
            return Err(FoxProError::InvalidArgument(format!(
                "{} is not a report (.frx) file",
                path.display()
            )));
        }
        let table = Table::load(path, None)?;
        for f in [
            "PLATFORM", "OBJTYPE", "OBJCODE", "EXPR", "VPOS", "HPOS", "HEIGHT", "WIDTH",
        ] {
            if table.field_index(f).is_err() {
                return Err(FoxProError::malformed(
                    path,
                    format!("not a VFP report: missing column {f}"),
                ));
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            table,
        })
    }

    fn num(&self, row: usize, field: &str) -> f64 {
        self.table
            .get_num(&self.table.rows[row], field)
            .unwrap_or(0.0)
    }

    fn int(&self, row: usize, field: &str) -> i64 {
        self.num(row, field).round() as i64
    }

    fn text(&self, row: usize, field: &str) -> String {
        self.table.get_text(&self.table.rows[row], field)
    }

    fn windows_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.table
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.deleted)
            .filter(|(_, r)| {
                self.table
                    .get_text(r, "PLATFORM")
                    .trim()
                    .eq_ignore_ascii_case("WINDOWS")
            })
            .map(|(i, _)| i)
    }

    fn header_row(&self) -> Result<usize> {
        self.windows_rows()
            .find(|&r| self.int(r, "OBJTYPE") == OBJ_HEADER)
            .ok_or_else(|| {
                FoxProError::malformed(&self.path, "report has no Windows layout header record")
            })
    }

    fn bands(&self) -> Vec<Band> {
        let rows: Vec<usize> = self
            .windows_rows()
            .filter(|&r| self.int(r, "OBJTYPE") == OBJ_BAND)
            .collect();
        let group_headers = rows
            .iter()
            .filter(|&&r| self.int(r, "OBJCODE") == BAND_GROUP_HEADER)
            .count();
        let mut start = 0.0;
        let mut gh = 0;
        let mut gf = 0;
        rows.into_iter()
            .map(|row| {
                let code = self.int(row, "OBJCODE");
                let height = self.num(row, "HEIGHT");
                let reference = match code {
                    BAND_GROUP_HEADER => {
                        gh += 1;
                        format!("group_header:{gh}")
                    }
                    BAND_GROUP_FOOTER => {
                        // Footers are stored innermost first.
                        gf += 1;
                        format!("group_footer:{}", group_headers + 1 - gf)
                    }
                    c => band_name(c).to_string(),
                };
                let band = Band {
                    row,
                    code,
                    start,
                    height,
                    reference,
                };
                start += height + BAND_BAR;
                band
            })
            .collect()
    }

    fn band_for(&self, bands: &[Band], vpos: f64) -> Option<usize> {
        bands.iter().rposition(|b| vpos + 0.5 >= b.start)
    }

    fn find_band(&self, bands: &[Band], reference: &str) -> Result<usize> {
        let r = reference
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_");
        let r = match r.as_str() {
            "group_header" | "group_header_1" => "group_header:1".to_string(),
            "group_footer" | "group_footer_1" => "group_footer:1".to_string(),
            other => other
                .replace("group_header_", "group_header:")
                .replace("group_footer_", "group_footer:"),
        };
        bands.iter().position(|b| b.reference == r).ok_or_else(|| {
            FoxProError::NotFound(format!(
                "band {reference:?} does not exist; available: {}",
                bands
                    .iter()
                    .map(|b| b.reference.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    fn object_rows(&self) -> Vec<usize> {
        self.windows_rows()
            .filter(|&r| {
                matches!(
                    self.int(r, "OBJTYPE"),
                    OBJ_LABEL | OBJ_LINE | OBJ_BOX | OBJ_FIELD | OBJ_PICTURE
                )
            })
            .collect()
    }

    fn object_id(&self, row: usize) -> String {
        let id = self.text(row, "UNIQUEID");
        let id = id.trim();
        if id.is_empty() {
            format!("#{}", row + 1)
        } else {
            id.to_string()
        }
    }

    fn find_object(&self, id: &str) -> Result<usize> {
        let id = id.trim();
        let rows = self.object_rows();
        if let Some(&r) = rows
            .iter()
            .find(|&&r| self.object_id(r).eq_ignore_ascii_case(id))
        {
            return Ok(r);
        }
        // Fall back to a unique match on expression or label text.
        let matches: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|&r| {
                let e = self.text(r, "EXPR");
                e.trim().eq_ignore_ascii_case(id) || unquote(e.trim()) == id
            })
            .collect();
        match matches.len() {
            1 => Ok(matches[0]),
            0 => Err(FoxProError::NotFound(format!(
                "report object {id:?} not found; use the id returned by foxpro.inspect_report"
            ))),
            _ => Err(FoxProError::Conflict(format!(
                "{id:?} matches {} objects; use one of the ids: {}",
                matches.len(),
                matches
                    .iter()
                    .map(|&r| self.object_id(r))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    fn field_def(&self, row: usize, bands: &[Band]) -> ReportField {
        let objtype = self.int(row, "OBJTYPE");
        let vpos = self.num(row, "VPOS");
        let (band, y) = match self.band_for(bands, vpos) {
            Some(i) => (bands[i].reference.clone(), vpos - bands[i].start),
            None => ("unknown".into(), vpos),
        };
        let expr = self.text(row, "EXPR").trim().to_string();
        let style = self.int(row, "FONTSTYLE");
        let has_font = matches!(objtype, OBJ_LABEL | OBJ_FIELD);
        let picture = self.text(row, "PICTURE");
        ReportField {
            id: self.object_id(row),
            kind: kind_name(objtype),
            band,
            x: fru_to_px(self.num(row, "HPOS")),
            y: fru_to_px(y),
            width: fru_to_px(self.num(row, "WIDTH")),
            height: fru_to_px(self.num(row, "HEIGHT")),
            expression: (objtype == OBJ_FIELD).then(|| expr.clone()),
            text: (objtype == OBJ_LABEL).then(|| unquote(&expr)),
            picture: (!picture.trim().is_empty()).then(|| unquote(picture.trim())),
            font: has_font.then(|| FontDefinition {
                face: self.text(row, "FONTFACE").trim().to_string(),
                size: self.num(row, "FONTSIZE"),
                bold: style & 1 != 0,
                italic: style & 2 != 0,
                underline: style & 4 != 0,
            }),
            total: (objtype == OBJ_FIELD)
                .then(|| total_name(self.int(row, "TOTALTYPE")))
                .flatten(),
            align: has_font.then(|| match self.int(row, "OFFSET") {
                1 => "right",
                2 => "center",
                _ => "left",
            }),
            stretch: self.table.get_bool(&self.table.rows[row], "STRETCH"),
        }
    }

    pub fn definition(&self) -> Result<ReportDefinition> {
        let header = self.header_row()?;
        let bands = self.bands();
        let setup = self.text(header, "EXPR");
        let setting = |key: &str| {
            setup.lines().find_map(|l| {
                let (k, v) = l.split_once('=')?;
                k.trim()
                    .eq_ignore_ascii_case(key)
                    .then(|| v.trim().to_string())
            })
        };
        let orientation = match setting("ORIENTATION").as_deref() {
            Some("1") => "landscape",
            _ => "portrait",
        };
        let paper_size = setting("PAPERSIZE").map(|p| {
            p.parse::<i64>()
                .ok()
                .and_then(paper_name)
                .map(str::to_string)
                .unwrap_or(p)
        });

        let mut warnings = Vec::new();
        if !bands.iter().any(|b| b.code == BAND_DETAIL) {
            warnings.push("report has no detail band".to_string());
        }
        let fields = self
            .object_rows()
            .into_iter()
            .map(|r| self.field_def(r, &bands))
            .collect();

        Ok(ReportDefinition {
            file: self.path.to_string_lossy().into_owned(),
            encoding: self.table.encoding.name().to_lowercase(),
            unit: "pixels (96 dpi), y relative to the band",
            page: PageSetup {
                orientation,
                paper_size,
                left_margin: fru_to_px(self.num(header, "HPOS")),
                columns: self.int(header, "VPOS").max(1),
            },
            bands: bands
                .iter()
                .map(|b| BandDefinition {
                    band_type: band_name(b.code),
                    reference: b.reference.clone(),
                    height: fru_to_px(b.height),
                    group_expression: (b.code == BAND_GROUP_HEADER)
                        .then(|| self.text(b.row, "EXPR").trim().to_string()),
                    new_page: self.table.get_bool(&self.table.rows[b.row], "PAGEBREAK"),
                })
                .collect(),
            fields,
            warnings,
        })
    }

    // -- creation -----------------------------------------------------------

    fn blank(&self, objtype: i64, objcode: i64) -> Result<crate::dbf::Row> {
        let t = &self.table;
        let mut row = t.blank_row();
        t.set_text(&mut row, "PLATFORM", "WINDOWS")?;
        t.set_text(&mut row, "UNIQUEID", &meta::unique_id())?;
        t.set_num(&mut row, "TIMESTAMP", meta::fox_timestamp())?;
        t.set_num(&mut row, "OBJTYPE", objtype as f64)?;
        t.set_num(&mut row, "OBJCODE", objcode as f64)?;
        for f in [
            "VPOS",
            "HPOS",
            "HEIGHT",
            "WIDTH",
            "PENRED",
            "PENGREEN",
            "PENBLUE",
            "FILLRED",
            "FILLGREEN",
            "FILLBLUE",
            "PENSIZE",
            "PENPAT",
            "FILLPAT",
            "FONTSTYLE",
            "FONTSIZE",
            "MODE",
            "RULER",
            "RULERLINES",
            "GRIDV",
            "GRIDH",
            "SUPTYPE",
            "SUPREST",
            "RESETRPT",
            "GENERAL",
            "SPACING",
            "OFFSET",
            "TOPMARGIN",
            "BOTMARGIN",
            "TOTALTYPE",
            "RESETTOTAL",
            "RESOID",
            "SUPRPCOL",
            "SUPGROUP",
        ] {
            t.set_num(&mut row, f, 0.0)?;
        }
        Ok(row)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        path: &Path,
        enc: &'static encoding_rs::Encoding,
        orientation: &str,
        paper_size: &str,
        left_margin_px: f64,
        right_margin_px: f64,
        bands_px: &[BandSpec],
        font_face: &str,
        font_size: f64,
    ) -> Result<Self> {
        let landscape = match orientation.to_ascii_lowercase().as_str() {
            "portrait" | "" => false,
            "landscape" => true,
            o => {
                return Err(FoxProError::InvalidArgument(format!(
                    "orientation must be portrait or landscape, got {o:?}"
                )));
            }
        };
        let (paper_code, w_in, h_in) = paper(paper_size)?;
        let page_width_in = if landscape { h_in } else { w_in };
        let column_width = page_width_in * 10_000.0 - px_to_fru(left_margin_px + right_margin_px);
        if column_width <= 0.0 {
            return Err(FoxProError::Validation(
                "margins are wider than the page".into(),
            ));
        }

        let mut doc = Self {
            path: path.to_path_buf(),
            table: Table::new_vfp(frx_fields(), enc, 64),
        };

        let mut header = doc.blank(OBJ_HEADER, 53)?;
        let t = &doc.table;
        t.set_num(&mut header, "VPOS", 1.0)?;
        t.set_num(&mut header, "HPOS", px_to_fru(left_margin_px))?;
        t.set_num(&mut header, "WIDTH", column_width)?;
        t.set_text(
            &mut header,
            "EXPR",
            &format!(
                "ORIENTATION={}\r\nPAPERSIZE={paper_code}\r\n",
                if landscape { 1 } else { 0 }
            ),
        )?;
        t.set_text(&mut header, "FONTFACE", font_face)?;
        t.set_num(&mut header, "FONTSIZE", font_size)?;
        t.set_num(&mut header, "RULER", 1.0)?;
        t.set_num(&mut header, "RULERLINES", 1.0)?;
        t.set_bool(&mut header, "GRID", true)?;
        t.set_num(&mut header, "GRIDV", 12.0)?;
        t.set_num(&mut header, "GRIDH", 12.0)?;
        doc.table.rows.push(header);

        for (code, height, group) in bands_px {
            if *height < 0.0 {
                return Err(FoxProError::Validation(
                    "band heights must not be negative".into(),
                ));
            }
            let mut band = doc.blank(OBJ_BAND, *code)?;
            let t = &doc.table;
            t.set_num(&mut band, "HEIGHT", px_to_fru(*height))?;
            if let Some((expr, new_page)) = group
                && *code == BAND_GROUP_HEADER
            {
                t.set_text(&mut band, "EXPR", expr)?;
                t.set_bool(&mut band, "PAGEBREAK", *new_page)?;
            }
            doc.table.rows.push(band);
        }

        let mut font = doc.blank(OBJ_FONT, 0)?;
        doc.table.set_text(&mut font, "FONTFACE", font_face)?;
        doc.table.set_num(&mut font, "FONTSIZE", font_size)?;
        doc.table.rows.push(font);

        let mut de = doc.blank(OBJ_DATAENV, 0)?;
        doc.table.set_text(&mut de, "NAME", "dataenvironment")?;
        doc.table.set_text(
            &mut de,
            "EXPR",
            "Top = 0\r\nLeft = 0\r\nWidth = 0\r\nHeight = 0\r\nDataSource = .NULL.\r\nName = \"Dataenvironment\"\r\n",
        )?;
        doc.table.rows.push(de);
        Ok(doc)
    }

    /// Band list for `create`, in print order.
    pub fn band_layout(
        title: Option<f64>,
        page_header: f64,
        groups: &[(String, f64, f64, bool)],
        detail: f64,
        page_footer: f64,
        summary: Option<f64>,
    ) -> Vec<BandSpec> {
        let mut v = Vec::new();
        if let Some(h) = title {
            v.push((BAND_TITLE, h, None));
        }
        v.push((BAND_PAGE_HEADER, page_header, None));
        for (expr, header, _, new_page) in groups {
            v.push((BAND_GROUP_HEADER, *header, Some((expr.clone(), *new_page))));
        }
        v.push((BAND_DETAIL, detail, None));
        for (expr, _, footer, _) in groups.iter().rev() {
            v.push((BAND_GROUP_FOOTER, *footer, Some((expr.clone(), false))));
        }
        v.push((BAND_PAGE_FOOTER, page_footer, None));
        if let Some(h) = summary {
            v.push((BAND_SUMMARY, h, None));
        }
        v
    }

    fn default_font(&self) -> (String, f64) {
        match self.header_row() {
            Ok(h) => {
                let face = self.text(h, "FONTFACE").trim().to_string();
                let size = self.num(h, "FONTSIZE");
                (
                    if face.is_empty() {
                        "Arial".into()
                    } else {
                        face
                    },
                    if size > 0.0 { size } else { 9.0 },
                )
            }
            Err(_) => ("Arial".into(), 9.0),
        }
    }

    /// Grow band `idx` so it is at least `needed` FRU tall, moving every
    /// object below it down by the same amount.
    fn ensure_band_height(&mut self, idx: usize, needed: f64) -> Result<Option<f64>> {
        let bands = self.bands();
        let band = &bands[idx];
        if needed <= band.height + 0.5 {
            return Ok(None);
        }
        let delta = needed - band.height;
        let band_end = band.start + band.height + BAND_BAR;
        for r in self.object_rows() {
            let v = self.num(r, "VPOS");
            if v >= band_end - 0.5 {
                self.table.set_num_at(r, "VPOS", v + delta)?;
            }
        }
        self.table.set_num_at(band.row, "HEIGHT", needed)?;
        Ok(Some(fru_to_px(needed)))
    }

    fn apply_spec(&mut self, row: usize, spec: &FieldSpec, creating: bool) -> Result<Value> {
        let objtype = self.int(row, "OBJTYPE");
        let mut notes = Vec::new();

        if let Some(expr) = &spec.expression {
            if objtype != OBJ_FIELD {
                return Err(FoxProError::Validation(
                    "expression applies to fields only; use text for labels".into(),
                ));
            }
            if expr.trim().is_empty() || expr.contains('\r') || expr.contains('\n') {
                return Err(FoxProError::Validation(
                    "expression must be a single non-empty line".into(),
                ));
            }
            self.table.set_text_at(row, "EXPR", expr.trim())?;
        }
        if let Some(text) = &spec.text {
            if objtype != OBJ_LABEL {
                return Err(FoxProError::Validation(
                    "text applies to labels only; use expression for fields".into(),
                ));
            }
            self.table.set_text_at(row, "EXPR", &escape_label(text)?)?;
        }
        if let Some(p) = &spec.picture {
            let stored = if p.is_empty() {
                String::new()
            } else {
                vfp_literal(p)?
            };
            self.table.set_text_at(row, "PICTURE", &stored)?;
        }
        if let Some(face) = &spec.font_face {
            self.table.set_text_at(row, "FONTFACE", face)?;
        }
        if let Some(size) = spec.font_size {
            if !(1.0..=127.0).contains(&size) {
                return Err(FoxProError::Validation(
                    "font size must be between 1 and 127".into(),
                ));
            }
            self.table.set_num_at(row, "FONTSIZE", size.round())?;
        }
        let mut style = self.int(row, "FONTSTYLE");
        for (flag, bit) in [(spec.bold, 1), (spec.italic, 2), (spec.underline, 4)] {
            match flag {
                Some(true) => style |= bit,
                Some(false) => style &= !bit,
                None => {}
            }
        }
        self.table.set_num_at(row, "FONTSTYLE", style as f64)?;
        if let Some(total) = &spec.total {
            if objtype != OBJ_FIELD {
                return Err(FoxProError::Validation(
                    "totals apply to fields only".into(),
                ));
            }
            self.table
                .set_num_at(row, "TOTALTYPE", total_code(total)?)?;
            self.table.set_num_at(row, "RESETTOTAL", 1.0)?;
        }
        if let Some(align) = &spec.align {
            let code = match align.to_ascii_lowercase().as_str() {
                "left" => 0.0,
                "right" => 1.0,
                "center" | "centre" => 2.0,
                a => {
                    return Err(FoxProError::InvalidArgument(format!(
                        "unknown alignment {a:?}"
                    )));
                }
            };
            self.table.set_num_at(row, "OFFSET", code)?;
        }
        if let Some(s) = spec.stretch {
            self.table.set_bool_at(row, "STRETCH", s)?;
        }

        // Geometry: x/y are relative to the (possibly new) band.
        for (v, name) in [
            (spec.x, "x"),
            (spec.y, "y"),
            (spec.width, "width"),
            (spec.height, "height"),
        ] {
            if let Some(v) = v
                && !(0.0..=20_000.0).contains(&v)
            {
                return Err(FoxProError::Validation(format!(
                    "{name} = {v} is out of range (0..20000 pixels)"
                )));
            }
        }
        let bands = self.bands();
        if bands.is_empty() {
            return Err(FoxProError::malformed(&self.path, "report has no bands"));
        }
        let current_vpos = self.num(row, "VPOS");
        let current_band = self.band_for(&bands, current_vpos).unwrap_or(0);
        let band_idx = match &spec.band {
            Some(b) => self.find_band(&bands, b)?,
            None if creating => self.find_band(&bands, "detail")?,
            None => current_band,
        };
        let y_fru = match spec.y {
            Some(y) => px_to_fru(y),
            None if band_idx == current_band && !creating => {
                current_vpos - bands[current_band].start
            }
            None => 0.0,
        };
        if let Some(x) = spec.x {
            self.table.set_num_at(row, "HPOS", px_to_fru(x))?;
        }
        if let Some(w) = spec.width {
            self.table.set_num_at(row, "WIDTH", px_to_fru(w))?;
        }
        if let Some(h) = spec.height {
            // Lines are drawn with height 0 (horizontal) or width 0 (vertical).
            self.table.set_num_at(row, "HEIGHT", px_to_fru(h))?;
        }
        let height = self.num(row, "HEIGHT");
        if let Some(new_height) = self.ensure_band_height(band_idx, y_fru + height)? {
            notes.push(format!(
                "band {} grew to {new_height} px to fit the object",
                bands[band_idx].reference
            ));
        }
        // Band starts may have moved when a band above grew.
        let start = self.bands()[band_idx].start;
        self.table
            .set_num_at(row, "VPOS", ((start + y_fru) * 1000.0).round() / 1000.0)?;
        self.table
            .set_num_at(row, "TIMESTAMP", meta::fox_timestamp())?;
        Ok(json!(notes))
    }

    pub fn add_field(&mut self, spec: &FieldSpec) -> Result<Value> {
        let kind = spec
            .kind
            .clone()
            .unwrap_or_else(|| {
                if spec.expression.is_some() {
                    "field"
                } else {
                    "label"
                }
                .to_string()
            })
            .to_ascii_lowercase();
        let objtype = match kind.as_str() {
            "field" => OBJ_FIELD,
            "label" | "text" => OBJ_LABEL,
            "line" => OBJ_LINE,
            "box" | "rectangle" | "shape" => OBJ_BOX,
            k => {
                return Err(FoxProError::InvalidArgument(format!(
                    "unknown report object kind {k:?} (field, label, line, box)"
                )));
            }
        };
        if objtype == OBJ_FIELD
            && spec
                .expression
                .as_deref()
                .is_none_or(|e| e.trim().is_empty())
        {
            return Err(FoxProError::InvalidArgument(
                "fields need an expression".into(),
            ));
        }
        if objtype == OBJ_LABEL && spec.text.as_deref().is_none_or(str::is_empty) {
            return Err(FoxProError::InvalidArgument("labels need text".into()));
        }

        let (face, size) = self.default_font();
        let size = spec.font_size.unwrap_or(size);
        let mut row = self.blank(objtype, 0)?;
        let t = &self.table;
        let default_h = (size * 2.0).round();
        let (w, h) = match objtype {
            OBJ_LABEL => {
                let chars = spec.text.as_deref().map(|s| s.chars().count()).unwrap_or(1) as f64;
                ((chars * size * 0.75).ceil().max(10.0), default_h)
            }
            OBJ_FIELD => (100.0, default_h),
            OBJ_LINE => (100.0, 0.0),
            _ => (100.0, 50.0),
        };
        t.set_num(&mut row, "WIDTH", px_to_fru(spec.width.unwrap_or(w)))?;
        t.set_num(&mut row, "HEIGHT", px_to_fru(spec.height.unwrap_or(h)))?;
        t.set_bool(&mut row, "SUPALWAYS", true)?;
        match objtype {
            OBJ_LABEL | OBJ_FIELD => {
                t.set_text(&mut row, "FONTFACE", &face)?;
                t.set_num(&mut row, "FONTSIZE", size)?;
                t.set_num(&mut row, "MODE", 1.0)?;
                t.set_num(&mut row, "FILLRED", 255.0)?;
                t.set_num(&mut row, "FILLGREEN", 255.0)?;
                t.set_num(&mut row, "FILLBLUE", 255.0)?;
                if objtype == OBJ_FIELD {
                    t.set_text(&mut row, "FILLCHAR", "C")?;
                    t.set_num(&mut row, "RESETTOTAL", 1.0)?;
                }
            }
            _ => {
                t.set_num(&mut row, "PENSIZE", 1.0)?;
                t.set_num(&mut row, "PENPAT", 8.0)?;
                t.set_num(&mut row, "MODE", 1.0)?;
            }
        }

        // Objects are stored after the last band/object record.
        let pos = self
            .windows_rows()
            .filter(|&r| {
                matches!(
                    self.int(r, "OBJTYPE"),
                    OBJ_BAND | OBJ_LABEL | OBJ_LINE | OBJ_BOX | OBJ_FIELD | OBJ_PICTURE
                )
            })
            .max()
            .map(|r| r + 1)
            .unwrap_or(self.table.rows.len());
        self.table.rows.insert(pos, row);
        let notes = self.apply_spec(pos, spec, true)?;
        let bands = self.bands();
        Ok(json!({
            "added": self.field_def(pos, &bands),
            "notes": notes,
        }))
    }

    pub fn update_field(&mut self, id: &str, spec: &FieldSpec) -> Result<Value> {
        let row = self.find_object(id)?;
        if spec.kind.is_some() {
            return Err(FoxProError::InvalidArgument(
                "the kind of an existing object cannot change; remove it and add a new one".into(),
            ));
        }
        let notes = self.apply_spec(row, spec, false)?;
        let bands = self.bands();
        Ok(json!({
            "updated": self.field_def(row, &bands),
            "notes": notes,
        }))
    }

    pub fn remove_field(&mut self, id: &str) -> Result<Value> {
        let row = self.find_object(id)?;
        let bands = self.bands();
        let removed = self.field_def(row, &bands);
        self.table.rows.remove(row);
        Ok(json!({ "removed": removed }))
    }
}

impl DesignerDocument for ReportDocument {
    fn path(&self) -> &Path {
        &self.path
    }

    fn render_text(&self) -> String {
        let Ok(def) = self.definition() else {
            return String::new();
        };
        let mut out = format!(
            "page: {} {:?} left_margin={}\n",
            def.page.orientation, def.page.paper_size, def.page.left_margin
        );
        for b in &def.bands {
            out.push_str(&format!("band {} height={}", b.reference, b.height));
            if let Some(g) = &b.group_expression {
                out.push_str(&format!(" group={g}"));
            }
            out.push('\n');
            for f in def.fields.iter().filter(|f| f.band == b.reference) {
                let content = f
                    .expression
                    .clone()
                    .or_else(|| f.text.as_ref().map(|t| format!("\"{t}\"")))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {} {} {content} at ({}, {}) size {}x{}",
                    f.id, f.kind, f.x, f.y, f.width, f.height
                ));
                if let Some(font) = &f.font {
                    out.push_str(&format!(" font {} {}", font.face, font.size));
                }
                if let Some(p) = &f.picture {
                    out.push_str(&format!(" picture {p}"));
                }
                if let Some(t) = f.total {
                    out.push_str(&format!(" total {t}"));
                }
                out.push('\n');
            }
        }
        out
    }

    fn validate(&self) -> Result<Vec<String>> {
        self.header_row()?;
        let bands = self.bands();
        if !bands.iter().any(|b| b.code == BAND_DETAIL) {
            return Err(FoxProError::Validation(
                "report must have a detail band".into(),
            ));
        }
        let mut warnings = Vec::new();
        let total_height: f64 = bands.iter().map(|b| b.height + BAND_BAR).sum();
        for r in self.object_rows() {
            let objtype = self.int(r, "OBJTYPE");
            let (v, w, h) = (
                self.num(r, "VPOS"),
                self.num(r, "WIDTH"),
                self.num(r, "HEIGHT"),
            );
            if v < 0.0 || w < 0.0 || h < 0.0 {
                return Err(FoxProError::Validation(format!(
                    "object {} has a negative position or size",
                    self.object_id(r)
                )));
            }
            if v > total_height {
                warnings.push(format!(
                    "object {} lies below the last band",
                    self.object_id(r)
                ));
            }
            if objtype == OBJ_FIELD && self.text(r, "EXPR").trim().is_empty() {
                return Err(FoxProError::Validation(format!(
                    "field {} has no expression",
                    self.object_id(r)
                )));
            }
        }
        Ok(warnings)
    }

    fn serialize(&self) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let (dbf, memo) = self.table.to_bytes()?;
        let mut files = vec![(self.path.clone(), dbf)];
        if let Some(memo) = memo {
            let ext = if fsutil::has_extension(&self.path, "lbx") {
                "lbt"
            } else {
                "frt"
            };
            files.push((fsutil::companion(&self.path, ext), memo));
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn new_report(dir: &TempDir) -> ReportDocument {
        let bands = ReportDocument::band_layout(
            None,
            60.0,
            &[("customer_id".into(), 30.0, 30.0, false)],
            25.0,
            40.0,
            Some(30.0),
        );
        ReportDocument::create(
            &dir.path().join("sales.frx"),
            encoding_rs::WINDOWS_874,
            "landscape",
            "a4",
            48.0,
            48.0,
            &bands,
            "Tahoma",
            10.0,
        )
        .unwrap()
    }

    fn reload(doc: &ReportDocument) -> ReportDocument {
        doc.validate().unwrap();
        for (p, b) in doc.serialize().unwrap() {
            std::fs::write(p, b).unwrap();
        }
        ReportDocument::load(doc.path()).unwrap()
    }

    #[test]
    fn create_and_inspect() {
        let dir = TempDir::new().unwrap();
        let doc = reload(&new_report(&dir));
        let def = doc.definition().unwrap();
        assert_eq!(def.page.orientation, "landscape");
        assert_eq!(def.page.paper_size.as_deref(), Some("a4"));
        let refs: Vec<&str> = def.bands.iter().map(|b| b.reference.as_str()).collect();
        assert_eq!(
            refs,
            vec![
                "page_header",
                "group_header:1",
                "detail",
                "group_footer:1",
                "page_footer",
                "summary"
            ]
        );
        assert_eq!(
            def.bands[1].group_expression.as_deref(),
            Some("customer_id")
        );
    }

    #[test]
    fn add_update_remove_fields() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_report(&dir);
        let label = FieldSpec {
            band: Some("page_header".into()),
            text: Some("เลขที่บิล".into()),
            x: Some(20.0),
            y: Some(10.0),
            ..Default::default()
        };
        doc.add_field(&label).unwrap();
        let field = FieldSpec {
            expression: Some("invoice_no".into()),
            x: Some(20.0),
            y: Some(2.0),
            width: Some(100.0),
            ..Default::default()
        };
        let added = doc.add_field(&field).unwrap();
        let id = added["added"]["id"].as_str().unwrap().to_string();

        // Grow the detail band by placing an object below its bottom.
        let tall = FieldSpec {
            expression: Some("amount".into()),
            y: Some(40.0),
            total: Some("sum".into()),
            ..Default::default()
        };
        let res = doc.add_field(&tall).unwrap();
        assert_eq!(res["notes"].as_array().unwrap().len(), 1);

        let doc = reload(&doc);
        let def = doc.definition().unwrap();
        assert_eq!(def.fields.len(), 3);
        let lbl = def.fields.iter().find(|f| f.kind == "label").unwrap();
        assert_eq!(lbl.text.as_deref(), Some("เลขที่บิล"));
        assert_eq!(lbl.band, "page_header");
        assert!((lbl.y - 10.0).abs() < 0.05);
        let inv = def.fields.iter().find(|f| f.id == id).unwrap();
        assert_eq!(inv.band, "detail");
        assert!((inv.y - 2.0).abs() < 0.05);
        let amt = def
            .fields
            .iter()
            .find(|f| f.expression.as_deref() == Some("amount"))
            .unwrap();
        assert_eq!(amt.total, Some("sum"));

        let mut doc = doc;
        let upd = FieldSpec {
            band: Some("group_footer".into()),
            y: Some(5.0),
            bold: Some(true),
            ..Default::default()
        };
        doc.update_field(&id, &upd).unwrap();
        let def = doc.definition().unwrap();
        let inv = def.fields.iter().find(|f| f.id == id).unwrap();
        assert_eq!(inv.band, "group_footer:1");
        assert!(inv.font.as_ref().unwrap().bold);

        doc.remove_field(&id).unwrap();
        assert!(doc.remove_field(&id).is_err());
        assert_eq!(doc.definition().unwrap().fields.len(), 2);
    }

    #[test]
    fn rejects_invalid_input() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_report(&dir);
        assert!(
            doc.add_field(&FieldSpec {
                kind: Some("field".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            doc.add_field(&FieldSpec {
                text: Some("x".into()),
                band: Some("nope".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            doc.add_field(&FieldSpec {
                text: Some("x".into()),
                x: Some(-1.0),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            doc.add_field(&FieldSpec {
                text: Some("日本".into()),
                ..Default::default()
            })
            .is_err()
        );
    }
}
