//! Catalog of supported control types and their designer defaults.
//!
//! Most controls are stored as their own SCX record. Pages, option buttons,
//! command-group buttons and grid columns are *members*: they have no record
//! of their own; their properties live in the container's PROPERTIES memo as
//! `Page1.Caption = "..."`, and the container's count property (PageCount,
//! ButtonCount, ColumnCount) determines how many exist.

pub struct ControlSpec {
    pub type_name: &'static str,
    pub base_class: &'static str,
    pub name_prefix: &'static str,
    /// Raw property values applied before user properties.
    pub defaults: &'static [(&'static str, &'static str)],
    /// Set `Caption = "<name>"` by default.
    pub caption: bool,
}

const SPECS: &[ControlSpec] = &[
    ControlSpec {
        type_name: "Label",
        base_class: "label",
        name_prefix: "Label",
        defaults: &[("BackStyle", "0"), ("Height", "17"), ("Width", "60")],
        caption: true,
    },
    ControlSpec {
        type_name: "TextBox",
        base_class: "textbox",
        name_prefix: "Text",
        defaults: &[("Height", "23"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "EditBox",
        base_class: "editbox",
        name_prefix: "Edit",
        defaults: &[("Height", "53"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "CommandButton",
        base_class: "commandbutton",
        name_prefix: "Command",
        defaults: &[("Height", "27"), ("Width", "84")],
        caption: true,
    },
    ControlSpec {
        type_name: "CheckBox",
        base_class: "checkbox",
        name_prefix: "Check",
        defaults: &[("Height", "17"), ("Width", "80"), ("Value", "0")],
        caption: true,
    },
    ControlSpec {
        type_name: "OptionGroup",
        base_class: "optiongroup",
        name_prefix: "Optiongroup",
        defaults: &[("Value", "1"), ("Height", "46"), ("Width", "71")],
        caption: false,
    },
    ControlSpec {
        type_name: "CommandGroup",
        base_class: "commandgroup",
        name_prefix: "Commandgroup",
        defaults: &[("Value", "1"), ("Height", "66"), ("Width", "94")],
        caption: false,
    },
    ControlSpec {
        type_name: "ComboBox",
        base_class: "combobox",
        name_prefix: "Combo",
        defaults: &[("Height", "24"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "ListBox",
        base_class: "listbox",
        name_prefix: "List",
        defaults: &[("Height", "170"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "Spinner",
        base_class: "spinner",
        name_prefix: "Spinner",
        defaults: &[("Height", "24"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "Grid",
        base_class: "grid",
        name_prefix: "Grid",
        defaults: &[("Height", "200"), ("Width", "320")],
        caption: false,
    },
    ControlSpec {
        type_name: "Image",
        base_class: "image",
        name_prefix: "Image",
        defaults: &[("Height", "100"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "Shape",
        base_class: "shape",
        name_prefix: "Shape",
        defaults: &[("Height", "50"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "Line",
        base_class: "line",
        name_prefix: "Line",
        defaults: &[("Height", "0"), ("Width", "100")],
        caption: false,
    },
    ControlSpec {
        type_name: "Container",
        base_class: "container",
        name_prefix: "Container",
        defaults: &[("Height", "100"), ("Width", "200")],
        caption: false,
    },
    ControlSpec {
        type_name: "PageFrame",
        base_class: "pageframe",
        name_prefix: "Pageframe",
        defaults: &[("ErasePage", ".T."), ("Height", "170"), ("Width", "240")],
        caption: false,
    },
    ControlSpec {
        type_name: "Timer",
        base_class: "timer",
        name_prefix: "Timer",
        defaults: &[("Height", "23"), ("Width", "23")],
        caption: false,
    },
];

/// Member object kinds and the container that holds them.
pub struct MemberSpec {
    pub type_name: &'static str,
    pub container_base: &'static str,
    pub count_prop: &'static str,
    pub name_prefix: &'static str,
    pub default_count: usize,
}

pub const MEMBERS: &[MemberSpec] = &[
    MemberSpec {
        type_name: "Page",
        container_base: "pageframe",
        count_prop: "PageCount",
        name_prefix: "Page",
        default_count: 2,
    },
    MemberSpec {
        type_name: "OptionButton",
        container_base: "optiongroup",
        count_prop: "ButtonCount",
        name_prefix: "Option",
        default_count: 2,
    },
    MemberSpec {
        type_name: "CommandButton",
        container_base: "commandgroup",
        count_prop: "ButtonCount",
        name_prefix: "Command",
        default_count: 2,
    },
    MemberSpec {
        type_name: "Column",
        container_base: "grid",
        count_prop: "ColumnCount",
        name_prefix: "Column",
        default_count: 2,
    },
];

pub fn lookup(type_name: &str) -> Option<&'static ControlSpec> {
    let t = type_name.trim();
    SPECS
        .iter()
        .find(|s| s.type_name.eq_ignore_ascii_case(t) || s.base_class.eq_ignore_ascii_case(t))
}

pub fn member_for_container(base_class: &str) -> Option<&'static MemberSpec> {
    MEMBERS
        .iter()
        .find(|m| m.container_base.eq_ignore_ascii_case(base_class))
}

pub fn member_by_type(type_name: &str, container_base: &str) -> Option<&'static MemberSpec> {
    MEMBERS.iter().find(|m| {
        m.type_name.eq_ignore_ascii_case(type_name.trim())
            && m.container_base.eq_ignore_ascii_case(container_base)
    })
}

pub fn supported_types() -> Vec<&'static str> {
    let mut v: Vec<&str> = SPECS.iter().map(|s| s.type_name).collect();
    v.extend(["Page", "OptionButton", "Column"]);
    v
}

/// Display name for a base class ("commandbutton" → "CommandButton").
pub fn display_type(base_class: &str) -> String {
    let extra = [
        ("form", "Form"),
        ("formset", "FormSet"),
        ("dataenvironment", "DataEnvironment"),
        ("cursor", "Cursor"),
        ("relation", "Relation"),
        ("header", "Header"),
        ("column", "Column"),
        ("page", "Page"),
        ("optionbutton", "OptionButton"),
        ("custom", "Custom"),
        ("olecontrol", "OleControl"),
        ("oleboundcontrol", "OleBoundControl"),
        ("hyperlink", "Hyperlink"),
        ("toolbar", "Toolbar"),
        ("separator", "Separator"),
        ("session", "Session"),
    ];
    SPECS
        .iter()
        .find(|s| s.base_class.eq_ignore_ascii_case(base_class))
        .map(|s| s.type_name.to_string())
        .or_else(|| {
            extra
                .iter()
                .find(|(b, _)| b.eq_ignore_ascii_case(base_class))
                .map(|(_, d)| d.to_string())
        })
        .unwrap_or_else(|| base_class.to_string())
}

/// Default member properties for member number `n` (1-based), in the order
/// the VFP designer writes them. `Name` is always last.
pub fn member_defaults(spec: &MemberSpec, n: usize) -> Vec<(&'static str, String)> {
    let name = format!("{}{n}", spec.name_prefix);
    let mut props: Vec<(&'static str, String)> = match spec.container_base {
        "pageframe" => vec![("Caption", format!("\"{name}\""))],
        "optiongroup" => vec![
            ("Caption", format!("\"{name}\"")),
            ("Value", if n == 1 { "1" } else { "0" }.to_string()),
            ("Height", "17".into()),
            ("Left", "5".into()),
            ("Top", (5 + (n - 1) * 23).to_string()),
            ("Width", "61".into()),
        ],
        "commandgroup" => vec![
            ("Top", (5 + (n - 1) * 32).to_string()),
            ("Left", "5".into()),
            ("Height", "27".into()),
            ("Width", "84".into()),
            ("Caption", format!("\"{name}\"")),
        ],
        _ => vec![],
    };
    props.push(("Name", format!("\"{name}\"")));
    props
}

/// Base events and methods of VFP classes. Methods outside this list on the
/// form need a declaration in the form's RESERVED3 memo.
pub const NATIVE_METHODS: &[&str] = &[
    "activate",
    "activatecell",
    "addcolumn",
    "additem",
    "addlistitem",
    "addobject",
    "addproperty",
    "afterclosetables",
    "afterdock",
    "afterrowcolchange",
    "autofit",
    "beforedock",
    "beforeopentables",
    "beforerowcolchange",
    "box",
    "circle",
    "clear",
    "click",
    "cloneobject",
    "closetables",
    "dblclick",
    "deactivate",
    "deletecolumn",
    "deleted",
    "destroy",
    "docmd",
    "dock",
    "doscroll",
    "doverb",
    "downclick",
    "drag",
    "dragdrop",
    "dragover",
    "draw",
    "dropdown",
    "error",
    "errormessage",
    "eval",
    "gotfocus",
    "gridhittest",
    "hide",
    "indextoitemid",
    "init",
    "interactivechange",
    "itemidtoindex",
    "keypress",
    "line",
    "load",
    "lostfocus",
    "message",
    "middleclick",
    "mousedown",
    "mouseenter",
    "mouseleave",
    "mousemove",
    "mouseup",
    "mousewheel",
    "move",
    "moved",
    "newobject",
    "opentables",
    "paint",
    "point",
    "print",
    "programmaticchange",
    "pset",
    "queryunload",
    "rangehigh",
    "rangelow",
    "readactivate",
    "readdeactivate",
    "readexpression",
    "readmethod",
    "readshow",
    "readvalid",
    "readwhen",
    "refresh",
    "release",
    "removeitem",
    "removelistitem",
    "removeobject",
    "requery",
    "reset",
    "resetoresult",
    "resize",
    "rightclick",
    "saveas",
    "saveasclass",
    "scrolled",
    "setall",
    "setfocus",
    "setviewport",
    "show",
    "showwhatsthis",
    "textheight",
    "textwidth",
    "timer",
    "uienable",
    "undock",
    "unload",
    "upclick",
    "valid",
    "when",
    "writeexpression",
    "writemethod",
    "zorder",
];

pub fn is_native_method(name: &str) -> bool {
    let base = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
    NATIVE_METHODS.contains(&base.as_str())
}
