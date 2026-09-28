//! The METHODS memo of SCX/VCX records:
//!
//! ```text
//! PROCEDURE Click
//! MESSAGEBOX('Saved')
//! ENDPROC
//! PROCEDURE Page1.Activate
//! ...
//! ENDPROC
//! ```
//!
//! Text outside PROCEDURE/ENDPROC blocks is preserved verbatim.

use crate::error::{FoxProError, Result};

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Method { name: String, code: String },
    Raw(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MethodList {
    blocks: Vec<Block>,
}

fn procedure_name(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t
        .get(..10)
        .filter(|p| p.eq_ignore_ascii_case("PROCEDURE "))
        .map(|_| &t[10..])?;
    rest.split_whitespace().next()
}

fn is_endproc(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 7 && t[..7].eq_ignore_ascii_case("ENDPROC")
}

impl MethodList {
    pub fn parse(text: &str) -> Self {
        let mut blocks = Vec::new();
        let mut current: Option<(String, Vec<&str>)> = None;
        let body = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        if body.is_empty() {
            return Self::default();
        }
        for line in body.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
            match &mut current {
                Some((_, lines)) => {
                    if is_endproc(line) {
                        let (name, lines) = current.take().unwrap_or_default();
                        blocks.push(Block::Method {
                            name,
                            code: lines.join("\r\n"),
                        });
                    } else {
                        lines.push(line);
                    }
                }
                None => {
                    if let Some(name) = procedure_name(line) {
                        current = Some((name.to_string(), Vec::new()));
                    } else {
                        blocks.push(Block::Raw(line.to_string()));
                    }
                }
            }
        }
        if let Some((name, lines)) = current {
            blocks.push(Block::Method {
                name,
                code: lines.join("\r\n"),
            });
        }
        Self { blocks }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            match b {
                Block::Method { name, code } => {
                    out.push_str("PROCEDURE ");
                    out.push_str(name);
                    out.push_str("\r\n");
                    if !code.is_empty() {
                        out.push_str(code);
                        out.push_str("\r\n");
                    }
                    out.push_str("ENDPROC\r\n");
                }
                Block::Raw(r) => {
                    out.push_str(r);
                    out.push_str("\r\n");
                }
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Method { name, .. } => Some(name.as_str()),
                Block::Raw(_) => None,
            })
            .collect()
    }

    pub fn methods(&self) -> impl Iterator<Item = (&str, &str)> {
        self.blocks.iter().filter_map(|b| match b {
            Block::Method { name, code } => Some((name.as_str(), code.as_str())),
            Block::Raw(_) => None,
        })
    }

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.methods()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, c)| c)
    }

    /// Add or replace one method, leaving all others untouched.
    /// Returns the previous code, if any.
    pub fn set(&mut self, name: &str, code: &str) -> Result<Option<String>> {
        let code = normalize_code(code)?;
        for b in &mut self.blocks {
            if let Block::Method { name: n, code: c } = b
                && n.eq_ignore_ascii_case(name)
            {
                return Ok(Some(std::mem::replace(c, code)));
            }
        }
        self.blocks.push(Block::Method {
            name: name.to_string(),
            code,
        });
        Ok(None)
    }

    pub fn remove(&mut self, name: &str) -> Option<String> {
        let idx = self.blocks.iter().position(
            |b| matches!(b, Block::Method { name: n, .. } if n.eq_ignore_ascii_case(name)),
        )?;
        match self.blocks.remove(idx) {
            Block::Method { code, .. } => Some(code),
            Block::Raw(_) => None,
        }
    }

    pub fn remove_member(&mut self, member: &str) {
        let prefix = format!("{}.", member.to_ascii_lowercase());
        self.blocks.retain(
            |b| !matches!(b, Block::Method { name, .. } if name.to_ascii_lowercase().starts_with(&prefix)),
        );
    }
}

/// Normalize line endings to CRLF and reject code that would break the
/// PROCEDURE/ENDPROC framing of the memo.
fn normalize_code(code: &str) -> Result<String> {
    let unified = code.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = unified.trim_end_matches('\n');
    for line in trimmed.lines() {
        if procedure_name(line).is_some() || is_endproc(line) {
            return Err(FoxProError::Validation(
                "method code must not contain PROCEDURE or ENDPROC lines; pass only the body"
                    .into(),
            ));
        }
    }
    Ok(trimmed.replace('\n', "\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "PROCEDURE Click\r\nMESSAGEBOX('a')\r\nENDPROC\r\nPROCEDURE Init\r\nDODEFAULT()\r\nENDPROC\r\n";

    #[test]
    fn roundtrip() {
        assert_eq!(MethodList::parse(SAMPLE).to_text(), SAMPLE);
    }

    #[test]
    fn set_preserves_other_methods() {
        let mut m = MethodList::parse(SAMPLE);
        let old = m.set("click", "MESSAGEBOX('Saved')\nRETURN").unwrap();
        assert_eq!(old.as_deref(), Some("MESSAGEBOX('a')"));
        assert_eq!(m.get("Init"), Some("DODEFAULT()"));
        assert_eq!(m.get("Click"), Some("MESSAGEBOX('Saved')\r\nRETURN"));
        m.set("Destroy", "").unwrap();
        assert_eq!(m.names(), vec!["Click", "Init", "Destroy"]);
    }

    #[test]
    fn rejects_framing_lines() {
        let mut m = MethodList::default();
        assert!(m.set("Click", "x = 1\nENDPROC\nPROCEDURE Evil").is_err());
    }
}
