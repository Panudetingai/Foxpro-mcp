//! Visual FoxPro forms (.scx/.sct).
//!
//! An SCX file is a VFP table with one record per object (data environment,
//! cursors, the form, every control). This module maps those records to a
//! JSON-friendly [`FormDefinition`] and implements safe edit operations.
//! Records that are not touched by an operation are written back byte for
//! byte (including compiled OBJCODE and OLE data).

pub mod controls;
pub mod methods;
pub mod props;

use crate::dbf::{FIELD_BINARY, FieldDef, Table};
use crate::designer::DesignerDocument;
use crate::encoding;
use crate::error::{FoxProError, Result};
use crate::fsutil;
use crate::meta;
use controls::{ControlSpec, MemberSpec};
use methods::MethodList;
use props::PropList;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const REQUIRED_FIELDS: &[&str] = &[
    "PLATFORM",
    "UNIQUEID",
    "TIMESTAMP",
    "CLASS",
    "CLASSLOC",
    "BASECLASS",
    "OBJNAME",
    "PARENT",
    "PROPERTIES",
    "METHODS",
];

/// The SCX table structure written by Visual FoxPro 9.
pub fn scx_fields() -> Vec<FieldDef> {
    let mut fields = vec![
        FieldDef::new("PLATFORM", 'C', 8, 0),
        FieldDef::new("UNIQUEID", 'C', 10, 0),
        FieldDef::new("TIMESTAMP", 'N', 10, 0),
    ];
    for name in [
        "CLASS",
        "CLASSLOC",
        "BASECLASS",
        "OBJNAME",
        "PARENT",
        "PROPERTIES",
        "PROTECTED",
        "METHODS",
    ] {
        fields.push(FieldDef::new(name, 'M', 4, 0));
    }
    for name in ["OBJCODE", "OLE", "OLE2"] {
        fields.push(FieldDef::new(name, 'M', 4, 0).with_flags(FIELD_BINARY));
    }
    for i in 1..=8 {
        fields.push(FieldDef::new(&format!("RESERVED{i}"), 'M', 4, 0));
    }
    fields.push(FieldDef::new("USER", 'M', 4, 0));
    fields
}

// ---------------------------------------------------------------------------
// Domain model returned to agents
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct MethodDefinition {
    pub name: String,
    pub lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ControlDefinition {
    pub name: String,
    /// Path relative to the form, e.g. `Pageframe1.Page1.txtName`.
    pub path: String,
    #[serde(rename = "type")]
    pub control_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_library: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub member: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodDefinition>,
}

#[derive(Debug, Serialize)]
pub struct CursorDefinition {
    pub name: String,
    pub alias: Option<String>,
    pub cursor_source: Option<String>,
    pub database: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FormDefinition {
    pub file: String,
    pub encoding: String,
    pub form: ControlDefinition,
    pub data_environment: Vec<CursorDefinition>,
    pub controls: Vec<ControlDefinition>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_forms: Vec<String>,
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Obj {
    row: usize,
    name: String,
    parent: String,
    base: String,
    class: String,
}

impl Obj {
    fn full(&self) -> String {
        if self.parent.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.parent, self.name)
        }
    }
}

#[derive(Debug, Clone)]
enum Target {
    Record(Obj),
    Member { owner: Obj, member: String },
}

impl Target {
    fn full(&self) -> String {
        match self {
            Target::Record(o) => o.full(),
            Target::Member { owner, member } => format!("{}.{member}", owner.full()),
        }
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn starts_with_path(path: &str, prefix: &str) -> bool {
    path.len() > prefix.len()
        && path.as_bytes()[prefix.len()] == b'.'
        && path[..prefix.len()].eq_ignore_ascii_case(prefix)
}

pub struct FormDocument {
    path: PathBuf,
    table: Table,
    touched: HashSet<String>,
}

impl FormDocument {
    pub fn load(path: &Path) -> Result<Self> {
        if !fsutil::has_extension(path, "scx") {
            return Err(FoxProError::InvalidArgument(format!(
                "{} is not a form (.scx) file",
                path.display()
            )));
        }
        let table = Table::load(path, None)?;
        for f in REQUIRED_FIELDS {
            if table.field_index(f).is_err() {
                return Err(FoxProError::malformed(
                    path,
                    format!("not a VFP form: missing column {f}"),
                ));
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            table,
            touched: HashSet::new(),
        })
    }

    /// A new form with a data environment, like VFP's CREATE FORM.
    pub fn create(
        path: &Path,
        name: &str,
        props_in: &Map<String, Value>,
        enc: &'static encoding_rs::Encoding,
    ) -> Result<Self> {
        if !meta::is_valid_name(name) {
            return Err(FoxProError::Validation(format!(
                "invalid form name {name:?}"
            )));
        }
        let mut doc = Self {
            path: path.to_path_buf(),
            table: Table::new_vfp(scx_fields(), enc, 64),
            touched: HashSet::new(),
        };

        let header = doc.new_row("COMMENT", "Screen")?;
        doc.table.rows.push(header);
        let last = doc.table.rows.len() - 1;
        doc.table
            .set_text_at(last, "RESERVED1", "VERSION =   3.00")?;

        let de_props = "Top = 0\r\nLeft = 0\r\nWidth = 0\r\nHeight = 0\r\nDataSource = .NULL.\r\nName = \"Dataenvironment\"\r\n";
        let de = doc.object_row("dataenvironment", "Dataenvironment", "", de_props)?;
        doc.table.rows.push(de);
        let last = doc.table.rows.len() - 1;
        doc.table.set_text_at(last, "RESERVED2", "1")?;
        doc.table.set_text_at(last, "RESERVED4", "1")?;

        let mut props = PropList::default();
        for (k, v) in [
            ("Top", "0"),
            ("Left", "0"),
            ("Height", "250"),
            ("Width", "375"),
            ("DoCreate", ".T."),
        ] {
            props.set_raw(k, v.to_string());
        }
        props.set_raw("Caption", props::encode_value("Caption", &json!(name))?);
        for (k, v) in props_in {
            if eq(k, "Name") {
                continue;
            }
            props.set(k, v)?;
        }
        props.set_raw("Name", props::encode_value("Name", &json!(name))?);
        let form = doc.object_row("form", name, "", &props.to_text())?;
        doc.table.rows.push(form);
        doc.touched.insert(name.to_ascii_lowercase());

        let trailer = doc.new_row("COMMENT", "RESERVED")?;
        doc.table.rows.push(trailer);
        Ok(doc)
    }

    pub fn encoding_name(&self) -> String {
        self.table.encoding.name().to_lowercase()
    }

    fn new_row(&self, platform: &str, unique_id: &str) -> Result<crate::dbf::Row> {
        let mut row = self.table.blank_row();
        self.table.set_text(&mut row, "PLATFORM", platform)?;
        self.table.set_text(&mut row, "UNIQUEID", unique_id)?;
        self.table.set_num(&mut row, "TIMESTAMP", 0.0)?;
        Ok(row)
    }

    fn object_row(
        &self,
        base: &str,
        name: &str,
        parent: &str,
        props: &str,
    ) -> Result<crate::dbf::Row> {
        let mut row = self.new_row("WINDOWS", &meta::unique_id())?;
        self.table
            .set_num(&mut row, "TIMESTAMP", meta::fox_timestamp())?;
        self.table.set_text(&mut row, "CLASS", base)?;
        self.table.set_text(&mut row, "BASECLASS", base)?;
        self.table.set_text(&mut row, "OBJNAME", name)?;
        self.table.set_text(&mut row, "PARENT", parent)?;
        self.table.set_text(&mut row, "PROPERTIES", props)?;
        Ok(row)
    }

    fn objects(&self) -> Vec<Obj> {
        self.table
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.deleted)
            .filter(|(_, r)| eq(self.table.get_text(r, "PLATFORM").trim(), "WINDOWS"))
            .map(|(i, r)| Obj {
                row: i,
                name: self.table.get_text(r, "OBJNAME").trim().to_string(),
                parent: self.table.get_text(r, "PARENT").trim().to_string(),
                base: self
                    .table
                    .get_text(r, "BASECLASS")
                    .trim()
                    .to_ascii_lowercase(),
                class: self.table.get_text(r, "CLASS").trim().to_string(),
            })
            .filter(|o| !o.name.is_empty())
            .collect()
    }

    fn form_obj(&self) -> Result<Obj> {
        self.objects()
            .into_iter()
            .find(|o| o.base == "form")
            .ok_or_else(|| FoxProError::malformed(&self.path, "the file contains no form object"))
    }

    fn props(&self, row: usize) -> PropList {
        PropList::parse(&self.table.get_text(&self.table.rows[row], "PROPERTIES"))
    }

    fn methods(&self, row: usize) -> MethodList {
        MethodList::parse(&self.table.get_text(&self.table.rows[row], "METHODS"))
    }

    fn mark_modified(&mut self, row: usize) -> Result<()> {
        self.table
            .set_num_at(row, "TIMESTAMP", meta::fox_timestamp())?;
        // Stale compiled code would keep running the old methods; VFP
        // recompiles forms with empty OBJCODE (COMPILE FORM / BUILD RECOMPILE).
        self.table.set_memo_at(row, "OBJCODE", Vec::new())?;
        let name = self.table.get_text(&self.table.rows[row], "OBJNAME");
        let parent = self.table.get_text(&self.table.rows[row], "PARENT");
        let full = if parent.trim().is_empty() {
            name.trim().to_string()
        } else {
            format!("{}.{}", parent.trim(), name.trim())
        };
        self.touched.insert(full.to_ascii_lowercase());
        Ok(())
    }

    fn write_props(&mut self, row: usize, props: &PropList) -> Result<()> {
        self.table
            .set_text_at(row, "PROPERTIES", &props.to_text())?;
        self.mark_modified(row)
    }

    fn write_methods(&mut self, row: usize, methods: &MethodList) -> Result<()> {
        self.table.set_text_at(row, "METHODS", &methods.to_text())?;
        self.mark_modified(row)
    }

    /// All addressable objects: records plus member objects (pages, ...).
    fn targets(&self) -> Vec<Target> {
        let mut out = Vec::new();
        for o in self.objects() {
            if controls::member_for_container(&o.base).is_some() {
                for m in self.props(o.row).members() {
                    out.push(Target::Member {
                        owner: o.clone(),
                        member: m,
                    });
                }
            }
            out.push(Target::Record(o));
        }
        out
    }

    /// Resolve an object reference: full path, path relative to the form,
    /// or a bare name when it is unique.
    fn resolve(&self, reference: &str) -> Result<Target> {
        let form = self.form_obj()?;
        let mut r = reference.trim();
        for prefix in ["thisform.", "THISFORM.", "ThisForm."] {
            r = r.strip_prefix(prefix).unwrap_or(r);
        }
        if r.is_empty() || eq(r, "thisform") || eq(r, "form") || eq(r, &form.full()) {
            return Ok(Target::Record(form));
        }
        let relative = format!("{}.{r}", form.full());
        let targets = self.targets();

        let exact: Vec<&Target> = targets
            .iter()
            .filter(|t| eq(&t.full(), r) || eq(&t.full(), &relative))
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0].clone());
        }
        if !r.contains('.') {
            let by_name: Vec<&Target> = targets
                .iter()
                .filter(|t| match t {
                    Target::Record(o) => eq(&o.name, r),
                    Target::Member { member, .. } => eq(member, r),
                })
                .collect();
            match by_name.len() {
                1 => return Ok(by_name[0].clone()),
                0 => {}
                _ => {
                    return Err(FoxProError::Conflict(format!(
                        "{r:?} is ambiguous; use one of: {}",
                        by_name
                            .iter()
                            .map(|t| self.relative_path(&t.full()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }
        Err(FoxProError::NotFound(format!(
            "object {reference:?} does not exist in {}",
            self.path.display()
        )))
    }

    fn relative_path(&self, full: &str) -> String {
        match self.form_obj() {
            Ok(f) if starts_with_path(full, &f.full()) => full[f.full().len() + 1..].to_string(),
            _ => full.to_string(),
        }
    }

    /// Names already used directly under `parent_full`.
    fn sibling_names(&self, parent_full: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .objects()
            .into_iter()
            .filter(|o| eq(&o.parent, parent_full))
            .map(|o| o.name)
            .collect();
        for t in self.targets() {
            if let Target::Member { owner, member } = t
                && eq(&owner.full(), parent_full)
            {
                names.push(member);
            }
        }
        names
    }

    fn unique_name(&self, prefix: &str, parent_full: &str) -> String {
        let used = self.sibling_names(parent_full);
        (1..)
            .map(|n| format!("{prefix}{n}"))
            .find(|n| !used.iter().any(|u| eq(u, n)))
            .unwrap_or_else(|| prefix.to_string())
    }

    /// Row index after the last record of `full`'s subtree. Member paths
    /// (pages, columns) have no record, so the owner's subtree is used.
    fn insert_position(&self, full: &str) -> usize {
        let objects = self.objects();
        let mut path = full.to_string();
        loop {
            let last = objects
                .iter()
                .filter(|o| eq(&o.full(), &path) || starts_with_path(&o.full(), &path))
                .map(|o| o.row)
                .max();
            if let Some(r) = last {
                return r + 1;
            }
            match path.rfind('.') {
                Some(i) => path.truncate(i),
                None => break,
            }
        }
        // Before the trailing COMMENT records VFP keeps at the end.
        let mut pos = self.table.rows.len();
        while pos > 0
            && eq(
                self.table
                    .get_text(&self.table.rows[pos - 1], "PLATFORM")
                    .trim(),
                "COMMENT",
            )
        {
            pos -= 1;
        }
        pos
    }

    // -- read ---------------------------------------------------------------

    fn control_def(&self, t: &Target, include_code: bool) -> ControlDefinition {
        // Record methods are unqualified; member methods are "Page1.Click".
        let method_defs = |m: &MethodList, member: Option<&str>| -> Vec<MethodDefinition> {
            m.methods()
                .filter_map(|(name, code)| {
                    let shown = match (member, name.split_once('.')) {
                        (Some(p), Some((owner, rest))) if eq(owner, p) => rest,
                        (None, None) => name,
                        _ => return None,
                    };
                    Some(MethodDefinition {
                        name: shown.to_string(),
                        lines: code.lines().count(),
                        code: include_code.then(|| code.replace("\r\n", "\n")),
                    })
                })
                .collect()
        };
        match t {
            Target::Record(o) => {
                let p = self.props(o.row);
                let row = &self.table.rows[o.row];
                let classloc = self.table.get_text(row, "CLASSLOC");
                let classloc = classloc.trim();
                ControlDefinition {
                    name: o.name.clone(),
                    path: self.relative_path(&o.full()),
                    control_type: controls::display_type(&o.base),
                    parent: self.relative_path(&o.parent),
                    class: (!eq(&o.class, &o.base)).then(|| o.class.clone()),
                    class_library: (!classloc.is_empty()).then(|| classloc.to_string()),
                    member: false,
                    left: p.num("Left"),
                    top: p.num("Top"),
                    width: p.num("Width"),
                    height: p.num("Height"),
                    caption: p.string("Caption"),
                    properties: p.to_json(),
                    methods: method_defs(&self.methods(o.row), None),
                }
            }
            Target::Member { owner, member } => {
                let p = self.props(owner.row);
                let mp = p.member_props(member);
                let num = |k: &str| mp.get(k).and_then(Value::as_f64);
                let member_type = controls::member_for_container(&owner.base)
                    .map(|m| m.type_name)
                    .unwrap_or("Member");
                ControlDefinition {
                    name: member.clone(),
                    path: self.relative_path(&format!("{}.{member}", owner.full())),
                    control_type: member_type.to_string(),
                    parent: self.relative_path(&owner.full()),
                    class: None,
                    class_library: None,
                    member: true,
                    left: num("Left"),
                    top: num("Top"),
                    width: num("Width"),
                    height: num("Height"),
                    caption: mp
                        .get("Caption")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    methods: method_defs(&self.methods(owner.row), Some(member)),
                    properties: mp,
                }
            }
        }
    }

    pub fn definition(&self, include_code: bool) -> Result<FormDefinition> {
        let form = self.form_obj()?;
        let form_full = form.full();
        let mut controls = Vec::new();
        let mut cursors = Vec::new();
        let mut other_forms = Vec::new();
        for t in self.targets() {
            let full = t.full();
            match &t {
                Target::Record(o) if o.row == form.row => continue,
                Target::Record(o) if o.base == "cursor" => {
                    let p = self.props(o.row);
                    cursors.push(CursorDefinition {
                        name: o.name.clone(),
                        alias: p.string("Alias"),
                        cursor_source: p.string("CursorSource"),
                        database: p.string("Database"),
                    });
                    continue;
                }
                Target::Record(o) if o.base == "form" => {
                    other_forms.push(o.full());
                    continue;
                }
                _ => {}
            }
            if starts_with_path(&full, &form_full) {
                controls.push(self.control_def(&t, include_code));
            }
        }
        Ok(FormDefinition {
            file: self.path.to_string_lossy().into_owned(),
            encoding: self.encoding_name(),
            form: self.control_def(&Target::Record(form), include_code),
            data_environment: cursors,
            controls,
            other_forms,
        })
    }

    // -- edit ---------------------------------------------------------------

    fn build_props(
        &self,
        spec_defaults: &[(&str, &str)],
        caption: Option<&str>,
        user: &Map<String, Value>,
        name: &str,
    ) -> Result<PropList> {
        let mut p = PropList::default();
        for (k, v) in spec_defaults {
            p.set_raw(k, v.to_string());
        }
        if let Some(c) = caption {
            p.set_raw("Caption", props::encode_value("Caption", &json!(c))?);
        }
        for (k, v) in user {
            if eq(k, "Name") {
                continue;
            }
            p.set(k, v)?;
        }
        p.set_raw("Name", props::encode_value("Name", &json!(name))?);
        Ok(p)
    }

    /// Add a control. `parent` defaults to the form.
    pub fn add_control(
        &mut self,
        control_type: &str,
        name: Option<&str>,
        parent: Option<&str>,
        properties: &Map<String, Value>,
        methods: &Map<String, Value>,
    ) -> Result<Value> {
        let parent_target = self.resolve(parent.unwrap_or(""))?;

        // Members (Page in PageFrame, OptionButton in OptionGroup, ...).
        if let Target::Record(owner) = &parent_target
            && let Some(member_spec) = controls::member_by_type(control_type, &owner.base)
        {
            return self.add_member(owner.clone(), member_spec, name, properties, methods);
        }

        let spec: &ControlSpec = controls::lookup(control_type).ok_or_else(|| {
            let hint = if controls::MEMBERS
                .iter()
                .any(|m| eq(m.type_name, control_type))
            {
                format!(
                    "; {control_type} can only be added to a {}",
                    controls::MEMBERS
                        .iter()
                        .filter(|m| eq(m.type_name, control_type))
                        .map(|m| controls::display_type(m.container_base))
                        .collect::<Vec<_>>()
                        .join(" or ")
                )
            } else {
                String::new()
            };
            FoxProError::InvalidArgument(format!(
                "unsupported control type {control_type:?}{hint}. Supported: {}",
                controls::supported_types().join(", ")
            ))
        })?;

        // Where can controls live? The form, containers, pages and columns.
        let parent_full = parent_target.full();
        match &parent_target {
            Target::Record(o) if o.base == "form" || o.base == "container" => {}
            Target::Member { owner, .. } if owner.base == "pageframe" || owner.base == "grid" => {}
            other => {
                let what = match other {
                    Target::Record(o) => controls::display_type(&o.base),
                    Target::Member { owner, .. } => {
                        format!("{} member", controls::display_type(&owner.base))
                    }
                };
                return Err(FoxProError::Validation(format!(
                    "{} ({what}) cannot contain controls; use the form, a Container, a Page or a grid Column",
                    self.relative_path(&parent_full)
                )));
            }
        }

        let name = match name {
            Some(n) => n.trim().to_string(),
            None => self.unique_name(spec.name_prefix, &parent_full),
        };
        if !meta::is_valid_name(&name) {
            return Err(FoxProError::Validation(format!(
                "invalid control name {name:?}: use letters, digits and underscores, starting with a letter"
            )));
        }
        if self
            .sibling_names(&parent_full)
            .iter()
            .any(|n| eq(n, &name))
        {
            return Err(FoxProError::Conflict(format!(
                "{} already contains an object named {name}",
                self.relative_path(&parent_full)
            )));
        }

        let caption = spec.caption.then_some(name.as_str());
        let mut p = self.build_props(spec.defaults, caption, properties, &name)?;

        // Compound controls get their member objects up front.
        let mut member_records = Vec::new();
        if let Some(member_spec) = controls::member_for_container(spec.base_class) {
            let count = match properties
                .iter()
                .find(|(k, _)| eq(k, member_spec.count_prop))
                .map(|(_, v)| v)
            {
                Some(v) => v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                    .filter(|n| *n <= 99)
                    .ok_or_else(|| {
                        FoxProError::Validation(format!(
                            "{} must be an integer between 0 and 99",
                            member_spec.count_prop
                        ))
                    })? as usize,
                None => member_spec.default_count,
            };
            p.set_raw(member_spec.count_prop, count.to_string());
            for n in 1..=count {
                for (k, v) in controls::member_defaults(member_spec, n) {
                    let key = format!("{}{n}.{k}", member_spec.name_prefix);
                    if !p.contains(&key) {
                        p.set_raw(&key, v);
                    }
                }
                if member_spec.container_base == "grid" {
                    member_records.push(format!("{}{n}", member_spec.name_prefix));
                }
            }
            // Re-apply explicit member properties such as "Page1.Caption".
            for (k, v) in properties.iter().filter(|(k, _)| k.contains('.')) {
                p.set(k, v)?;
            }
        }
        props::validate_geometry(&p, &name)?;

        let mut m = MethodList::default();
        for (method, code) in methods {
            let code = code.as_str().ok_or_else(|| {
                FoxProError::InvalidArgument(format!("method {method} code must be a string"))
            })?;
            m.set(method, code)?;
        }

        let pos = self.insert_position(&parent_full);
        let mut row = self.object_row(spec.base_class, &name, &parent_full, &p.to_text())?;
        if !m.is_empty() {
            self.table.set_text(&mut row, "METHODS", &m.to_text())?;
        }
        self.table.rows.insert(pos, row);
        let full = format!("{parent_full}.{name}");
        self.touched.insert(full.to_ascii_lowercase());

        for column in member_records {
            self.add_column_records(&full, &column)?;
        }

        Ok(json!({
            "added": self.relative_path(&full),
            "type": spec.type_name,
            "properties": p.to_json(),
        }))
    }

    /// Header and TextBox records that belong to a grid column.
    fn add_column_records(&mut self, grid_full: &str, column: &str) -> Result<()> {
        let parent = format!("{grid_full}.{column}");
        let pos = self.insert_position(grid_full);
        let header = self.object_row(
            "header",
            "Header1",
            &parent,
            "Caption = \"Header1\"\r\nName = \"Header1\"\r\n",
        )?;
        let text = self.object_row(
            "textbox",
            "Text1",
            &parent,
            "BorderStyle = 0\r\nMargin = 0\r\nForeColor = 0,0,0\r\nBackColor = 255,255,255\r\nName = \"Text1\"\r\n",
        )?;
        self.table.rows.insert(pos, header);
        self.table.rows.insert(pos + 1, text);
        Ok(())
    }

    fn add_member(
        &mut self,
        owner: Obj,
        spec: &MemberSpec,
        name: Option<&str>,
        properties: &Map<String, Value>,
        methods: &Map<String, Value>,
    ) -> Result<Value> {
        let mut p = self.props(owner.row);
        let existing = p.members().len();
        let count = p
            .num(spec.count_prop)
            .map(|n| n.max(0.0) as usize)
            .unwrap_or(existing)
            .max(existing);
        let n = count + 1;
        let member = format!("{}{n}", spec.name_prefix);
        if let Some(requested) = name
            && !eq(requested.trim(), &member)
        {
            return Err(FoxProError::Validation(format!(
                "{} objects are numbered by VFP; the new one will be named {member} (rename is not supported for member objects)",
                spec.type_name
            )));
        }
        p.set_raw(spec.count_prop, n.to_string());
        for (k, v) in controls::member_defaults(spec, n) {
            p.set_raw(&format!("{member}.{k}"), v);
        }
        for (k, v) in properties {
            if eq(k, "Name") {
                continue;
            }
            p.set(&format!("{member}.{k}"), v)?;
        }
        props::validate_geometry(&p, &owner.name)?;
        self.write_props(owner.row, &p)?;

        if !methods.is_empty() {
            let mut m = self.methods(owner.row);
            for (method, code) in methods {
                let code = code.as_str().ok_or_else(|| {
                    FoxProError::InvalidArgument(format!("method {method} code must be a string"))
                })?;
                m.set(&format!("{member}.{method}"), code)?;
            }
            self.write_methods(owner.row, &m)?;
        }
        if spec.container_base == "grid" {
            self.add_column_records(&owner.full(), &member)?;
        }

        let full = format!("{}.{member}", owner.full());
        Ok(json!({
            "added": self.relative_path(&full),
            "type": spec.type_name,
            "member_of": self.relative_path(&owner.full()),
            "properties": p.member_props(&member),
        }))
    }

    pub fn update_control(
        &mut self,
        object: &str,
        properties: &Map<String, Value>,
        remove: &[String],
    ) -> Result<Value> {
        let target = self.resolve(object)?;
        match target {
            Target::Record(o) => {
                let mut p = self.props(o.row);
                let mut rename = None;
                for (k, v) in properties {
                    if eq(k, "Name") {
                        let new = v.as_str().ok_or_else(|| {
                            FoxProError::InvalidArgument("Name must be a string".into())
                        })?;
                        if !eq(new, &o.name) || new != o.name {
                            rename = Some(new.trim().to_string());
                        }
                        continue;
                    }
                    p.set(k, v)?;
                }
                for k in remove {
                    if eq(k, "Name") {
                        return Err(FoxProError::Validation("Name cannot be removed".into()));
                    }
                    p.remove(k);
                }
                props::validate_geometry(&p, &o.name)?;
                self.write_props(o.row, &p)?;
                let mut result = json!({
                    "updated": self.relative_path(&o.full()),
                    "properties": p.to_json(),
                });
                if let Some(new) = rename {
                    let renamed = self.rename(&o, &new)?;
                    result["renamed_to"] = json!(renamed);
                    result["note"] = json!(
                        "code that refers to the old name (e.g. THISFORM.oldname) is not updated automatically; use foxpro.search_code"
                    );
                }
                Ok(result)
            }
            Target::Member { owner, member } => {
                let mut p = self.props(owner.row);
                for (k, v) in properties {
                    if eq(k, "Name") {
                        return Err(FoxProError::Validation(
                            "member objects (pages, option buttons, columns) cannot be renamed"
                                .into(),
                        ));
                    }
                    p.set(&format!("{member}.{k}"), v)?;
                }
                for k in remove {
                    if eq(k, "Name") {
                        return Err(FoxProError::Validation("Name cannot be removed".into()));
                    }
                    p.remove(&format!("{member}.{k}"));
                }
                props::validate_geometry(&p, &owner.name)?;
                self.write_props(owner.row, &p)?;
                Ok(json!({
                    "updated": self.relative_path(&format!("{}.{member}", owner.full())),
                    "properties": p.member_props(&member),
                }))
            }
        }
    }

    fn rename(&mut self, o: &Obj, new: &str) -> Result<String> {
        if !meta::is_valid_name(new) {
            return Err(FoxProError::Validation(format!("invalid name {new:?}")));
        }
        if self
            .sibling_names(&o.parent)
            .iter()
            .any(|n| eq(n, new) && !eq(n, &o.name))
        {
            return Err(FoxProError::Conflict(format!(
                "an object named {new} already exists next to {}",
                o.name
            )));
        }
        let old_full = o.full();
        let new_full = if o.parent.is_empty() {
            new.to_string()
        } else {
            format!("{}.{new}", o.parent)
        };

        self.table.set_text_at(o.row, "OBJNAME", new)?;
        let mut p = self.props(o.row);
        p.set_raw("Name", props::encode_value("Name", &json!(new))?);
        self.write_props(o.row, &p)?;

        for child in self.objects() {
            let parent = &child.parent;
            let updated = if eq(parent, &old_full) {
                Some(new_full.clone())
            } else if starts_with_path(parent, &old_full) {
                Some(format!("{new_full}{}", &parent[old_full.len()..]))
            } else {
                None
            };
            if let Some(updated) = updated {
                self.table.set_text_at(child.row, "PARENT", &updated)?;
                self.table
                    .set_num_at(child.row, "TIMESTAMP", meta::fox_timestamp())?;
            }
        }
        Ok(self.relative_path(&new_full))
    }

    pub fn remove_control(&mut self, object: &str) -> Result<Value> {
        let target = self.resolve(object)?;
        match target {
            Target::Record(o) => {
                if matches!(o.base.as_str(), "form" | "dataenvironment" | "formset") {
                    return Err(FoxProError::Validation(format!(
                        "{} ({}) cannot be removed",
                        o.name,
                        controls::display_type(&o.base)
                    )));
                }
                let full = o.full();
                let doomed: Vec<Obj> = self
                    .objects()
                    .into_iter()
                    .filter(|x| {
                        x.row == o.row || eq(&x.parent, &full) || starts_with_path(&x.parent, &full)
                    })
                    .collect();
                let removed: Vec<String> = doomed
                    .iter()
                    .map(|x| self.relative_path(&x.full()))
                    .collect();
                let rows: HashSet<usize> = doomed.iter().map(|x| x.row).collect();
                let mut i = 0;
                self.table.rows.retain(|_| {
                    let keep = !rows.contains(&i);
                    i += 1;
                    keep
                });
                Ok(json!({ "removed": removed }))
            }
            Target::Member { owner, member } => {
                let spec = controls::member_for_container(&owner.base)
                    .ok_or_else(|| FoxProError::Internal("member without container spec".into()))?;
                let mut p = self.props(owner.row);
                let members = p.members();
                let last = members.last().cloned().unwrap_or_default();
                if !eq(&last, &member) {
                    return Err(FoxProError::Validation(format!(
                        "only the last {} ({last}) can be removed; VFP numbers member objects sequentially",
                        spec.type_name
                    )));
                }
                let count = p
                    .num(spec.count_prop)
                    .map(|n| n as usize)
                    .unwrap_or(members.len());
                p.set_raw(spec.count_prop, count.saturating_sub(1).to_string());
                p.remove_member(&member);
                self.write_props(owner.row, &p)?;
                let mut m = self.methods(owner.row);
                m.remove_member(&member);
                self.write_methods(owner.row, &m)?;

                let member_full = format!("{}.{member}", owner.full());
                let doomed: HashSet<usize> = self
                    .objects()
                    .into_iter()
                    .filter(|x| {
                        eq(&x.parent, &member_full) || starts_with_path(&x.parent, &member_full)
                    })
                    .map(|x| x.row)
                    .collect();
                let mut i = 0;
                self.table.rows.retain(|_| {
                    let keep = !doomed.contains(&i);
                    i += 1;
                    keep
                });
                Ok(json!({
                    "removed": [self.relative_path(&member_full)],
                    "removed_children": doomed.len(),
                }))
            }
        }
    }

    /// Add, replace or remove one method; other methods are preserved.
    pub fn update_method(
        &mut self,
        object: &str,
        method: &str,
        code: Option<&str>,
    ) -> Result<Value> {
        let method = method.trim();
        if !meta::is_valid_name(method) {
            return Err(FoxProError::InvalidArgument(format!(
                "invalid method name {method:?}"
            )));
        }
        let target = self.resolve(object)?;
        let (row, stored_name, path, is_form) = match &target {
            Target::Record(o) => (o.row, method.to_string(), o.full(), o.base == "form"),
            Target::Member { owner, member } => (
                owner.row,
                format!("{member}.{method}"),
                format!("{}.{member}", owner.full()),
                false,
            ),
        };
        let mut m = self.methods(row);
        let (action, previous) = match code {
            Some(code) => {
                let previous = m.set(&stored_name, code)?;
                (
                    if previous.is_some() {
                        "replaced"
                    } else {
                        "added"
                    },
                    previous,
                )
            }
            None => {
                let previous = m.remove(&stored_name).ok_or_else(|| {
                    FoxProError::NotFound(format!(
                        "{} has no method {method}",
                        self.relative_path(&path)
                    ))
                })?;
                ("removed", Some(previous))
            }
        };
        self.write_methods(row, &m)?;

        let mut warnings = Vec::new();
        if code.is_some() && !controls::is_native_method(method) {
            if is_form {
                // Custom form methods must be declared in RESERVED3.
                let decl = self.table.get_text(&self.table.rows[row], "RESERVED3");
                let exists = decl.lines().any(|l| {
                    l.trim()
                        .strip_prefix('*')
                        .and_then(|r| r.split_whitespace().next())
                        .is_some_and(|n| eq(n, method))
                });
                if !exists {
                    let mut decl = decl;
                    if !decl.is_empty() && !decl.ends_with('\n') {
                        decl.push_str("\r\n");
                    }
                    decl.push_str(&format!("*{} \r\n", method.to_ascii_lowercase()));
                    self.table.set_text_at(row, "RESERVED3", &decl)?;
                }
            } else {
                warnings.push(format!(
                    "{method} is not a built-in event/method of this control; VFP only allows custom methods on the form or in classes"
                ));
            }
        }

        let mut result = json!({
            "object": self.relative_path(&path),
            "method": method,
            "action": action,
            "other_methods": m.names().into_iter().filter(|n| !eq(n, &stored_name)).collect::<Vec<_>>(),
        });
        if let Some(prev) = previous {
            result["previous_lines"] = json!(prev.lines().count());
        }
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        Ok(result)
    }
}

impl DesignerDocument for FormDocument {
    fn path(&self) -> &Path {
        &self.path
    }

    fn render_text(&self) -> String {
        let mut out = String::new();
        for o in self.objects() {
            out.push_str(&format!(
                "[{}] ({})\n",
                o.full(),
                controls::display_type(&o.base)
            ));
            for (k, v) in self.props(o.row).pairs() {
                out.push_str(&format!("  {k} = {v}\n"));
            }
            for (name, code) in self.methods(o.row).methods() {
                out.push_str(&format!("  PROCEDURE {name}\n"));
                for line in code.lines() {
                    out.push_str(&format!("    {line}\n"));
                }
                out.push_str("  ENDPROC\n");
            }
        }
        out
    }

    fn validate(&self) -> Result<Vec<String>> {
        let objects = self.objects();
        let mut warnings = Vec::new();
        if !objects.iter().any(|o| o.base == "form") {
            return Err(FoxProError::Validation("the form object is missing".into()));
        }

        let mut paths: HashSet<String> = HashSet::new();
        for o in &objects {
            if !meta::is_valid_name(&o.name) {
                warnings.push(format!(
                    "object name {:?} is not a valid identifier",
                    o.name
                ));
            }
            if !paths.insert(o.full().to_ascii_lowercase()) {
                return Err(FoxProError::Validation(format!(
                    "duplicate object {}",
                    o.full()
                )));
            }
        }
        let members: HashSet<String> = self
            .targets()
            .iter()
            .filter(|t| matches!(t, Target::Member { .. }))
            .map(|t| t.full().to_ascii_lowercase())
            .collect();
        for o in &objects {
            if o.parent.is_empty() {
                continue;
            }
            let p = o.parent.to_ascii_lowercase();
            if !paths.contains(&p) && !members.contains(&p) {
                return Err(FoxProError::Validation(format!(
                    "{} refers to a parent {} that does not exist",
                    o.name, o.parent
                )));
            }
        }
        for o in objects
            .iter()
            .filter(|o| self.touched.contains(&o.full().to_ascii_lowercase()))
        {
            let p = self.props(o.row);
            props::validate_geometry(&p, &o.full())?;
            match p.string("Name") {
                Some(n) if eq(&n, &o.name) => {}
                _ => {
                    return Err(FoxProError::Validation(format!(
                        "{}: Name property must match the object name",
                        o.full()
                    )));
                }
            }
        }
        Ok(warnings)
    }

    fn serialize(&self) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let (dbf, memo) = self.table.to_bytes()?;
        let mut files = vec![(self.path.clone(), dbf)];
        if let Some(memo) = memo {
            files.push((fsutil::companion(&self.path, "sct"), memo));
        }
        Ok(files)
    }
}

/// Encoding for a new form: explicit label, or Windows-874 when any caption
/// is Thai, otherwise Windows-1252.
pub fn encoding_for_new(
    label: Option<&str>,
    sample: &str,
) -> Result<&'static encoding_rs::Encoding> {
    match label {
        Some(l) => encoding::lookup(l),
        None => Ok(encoding::ansi_for_text(sample)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn new_form(dir: &TempDir) -> FormDocument {
        let path = dir.path().join("customer.scx");
        let mut props = Map::new();
        props.insert("Caption".into(), json!("ลูกค้า"));
        props.insert("Width".into(), json!(800));
        FormDocument::create(&path, "frmCustomer", &props, encoding_rs::WINDOWS_874).unwrap()
    }

    fn save_and_reload(doc: &FormDocument) -> FormDocument {
        doc.validate().unwrap();
        for (p, bytes) in doc.serialize().unwrap() {
            std::fs::write(p, bytes).unwrap();
        }
        FormDocument::load(doc.path()).unwrap()
    }

    #[test]
    fn create_form_roundtrip() {
        let dir = TempDir::new().unwrap();
        let doc = save_and_reload(&new_form(&dir));
        let def = doc.definition(false).unwrap();
        assert_eq!(def.form.name, "frmCustomer");
        assert_eq!(def.form.caption.as_deref(), Some("ลูกค้า"));
        assert_eq!(def.form.width, Some(800.0));
        assert_eq!(def.encoding, "windows-874");
        assert!(def.controls.is_empty());
    }

    #[test]
    fn add_update_remove_controls() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_form(&dir);
        let mut props = Map::new();
        props.insert("Left".into(), json!(10));
        props.insert("Caption".into(), json!("บันทึก"));
        doc.add_control("CommandButton", Some("cmdSave"), None, &props, &Map::new())
            .unwrap();
        doc.add_control("TextBox", Some("txtName"), None, &Map::new(), &Map::new())
            .unwrap();
        let err = doc
            .add_control("TextBox", Some("txtName"), None, &Map::new(), &Map::new())
            .unwrap_err();
        assert!(matches!(err, FoxProError::Conflict(_)));

        let mut upd = Map::new();
        upd.insert("Top".into(), json!(40));
        upd.insert("Visible".into(), json!(false));
        doc.update_control("txtName", &upd, &[]).unwrap();
        doc.update_method("cmdSave", "Click", Some("MESSAGEBOX('Saved')"))
            .unwrap();
        doc.update_method("cmdSave", "Init", Some("RETURN .T."))
            .unwrap();
        doc.update_method("cmdSave", "Click", Some("THISFORM.Release()"))
            .unwrap();

        let doc2 = save_and_reload(&doc);
        let def = doc2.definition(true).unwrap();
        assert_eq!(def.controls.len(), 2);
        let cmd = def.controls.iter().find(|c| c.name == "cmdSave").unwrap();
        assert_eq!(cmd.caption.as_deref(), Some("บันทึก"));
        assert_eq!(cmd.methods.len(), 2);
        assert_eq!(cmd.methods[0].code.as_deref(), Some("THISFORM.Release()"));
        let txt = def.controls.iter().find(|c| c.name == "txtName").unwrap();
        assert_eq!(txt.top, Some(40.0));
        assert_eq!(txt.properties["Visible"], json!(false));

        let mut doc3 = doc2;
        doc3.remove_control("txtName").unwrap();
        assert!(doc3.remove_control("txtName").is_err());
        assert!(doc3.remove_control("frmCustomer").is_err());
        assert_eq!(doc3.definition(false).unwrap().controls.len(), 1);
    }

    #[test]
    fn pageframe_pages_and_nested_controls() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_form(&dir);
        doc.add_control("PageFrame", Some("pgf"), None, &Map::new(), &Map::new())
            .unwrap();
        doc.add_control("Page", None, Some("pgf"), &Map::new(), &Map::new())
            .unwrap();
        doc.add_control(
            "Label",
            Some("lbl"),
            Some("pgf.Page3"),
            &Map::new(),
            &Map::new(),
        )
        .unwrap();
        let mut caption = Map::new();
        caption.insert("Caption".into(), json!("General"));
        doc.update_control("pgf.Page1", &caption, &[]).unwrap();

        let doc = save_and_reload(&doc);
        let def = doc.definition(false).unwrap();
        let pages: Vec<_> = def
            .controls
            .iter()
            .filter(|c| c.control_type == "Page")
            .collect();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].caption.as_deref(), Some("General"));
        let lbl = def.controls.iter().find(|c| c.name == "lbl").unwrap();
        assert_eq!(lbl.parent, "pgf.Page3");

        let mut doc = doc;
        assert!(doc.remove_control("pgf.Page1").is_err());
        doc.remove_control("pgf.Page3").unwrap();
        let def = doc.definition(false).unwrap();
        assert!(def.controls.iter().all(|c| c.name != "lbl"));
    }

    #[test]
    fn rename_updates_children() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_form(&dir);
        doc.add_control("Container", Some("cnt"), None, &Map::new(), &Map::new())
            .unwrap();
        doc.add_control(
            "TextBox",
            Some("txt"),
            Some("cnt"),
            &Map::new(),
            &Map::new(),
        )
        .unwrap();
        let mut upd = Map::new();
        upd.insert("Name".into(), json!("cntMain"));
        doc.update_control("cnt", &upd, &[]).unwrap();
        let doc = save_and_reload(&doc);
        let def = doc.definition(false).unwrap();
        let txt = def.controls.iter().find(|c| c.name == "txt").unwrap();
        assert_eq!(txt.path, "cntMain.txt");
    }

    #[test]
    fn grid_creates_column_records() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_form(&dir);
        let mut props = Map::new();
        props.insert("ColumnCount".into(), json!(3));
        doc.add_control("Grid", Some("grd"), None, &props, &Map::new())
            .unwrap();
        let def = save_and_reload(&doc).definition(false).unwrap();
        let headers = def
            .controls
            .iter()
            .filter(|c| c.control_type == "Header")
            .count();
        assert_eq!(headers, 3);
    }

    #[test]
    fn custom_form_method_is_declared() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_form(&dir);
        doc.update_method("", "SaveData", Some("RETURN .T."))
            .unwrap();
        let form = doc.form_obj().unwrap();
        let decl = doc.table.get_text(&doc.table.rows[form.row], "RESERVED3");
        assert!(decl.contains("*savedata"));
    }

    #[test]
    fn rejects_bad_input() {
        let dir = TempDir::new().unwrap();
        let mut doc = new_form(&dir);
        assert!(
            doc.add_control("Widget", None, None, &Map::new(), &Map::new())
                .is_err()
        );
        assert!(
            doc.add_control("TextBox", Some("1bad"), None, &Map::new(), &Map::new())
                .is_err()
        );
        let mut props = Map::new();
        props.insert("Width".into(), json!(-5));
        assert!(
            doc.add_control("TextBox", None, None, &props, &Map::new())
                .is_err()
        );
        let mut props = Map::new();
        props.insert("Caption".into(), json!("日本語"));
        let err = doc
            .add_control("Label", None, None, &props, &Map::new())
            .unwrap_err();
        assert!(matches!(err, FoxProError::Encoding(_)));
        assert!(doc.update_method("missing", "Click", Some("x")).is_err());
    }
}
