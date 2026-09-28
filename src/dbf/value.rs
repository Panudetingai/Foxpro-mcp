//! Typed field values.

use super::FieldDef;
use crate::error::{FoxProError, Result};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use encoding_rs::Encoding;
use serde_json::{Value, json};

/// A decoded field or expression value.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// `None` is the empty date `{}`.
    Date(Option<NaiveDate>),
    DateTime(Option<NaiveDateTime>),
    Binary(Vec<u8>),
}

impl Val {
    pub fn type_name(&self) -> &'static str {
        match self {
            Val::Null => "null",
            Val::Bool(_) => "logical",
            Val::Num(_) => "numeric",
            Val::Str(_) => "character",
            Val::Date(_) => "date",
            Val::DateTime(_) => "datetime",
            Val::Binary(_) => "binary",
        }
    }

    /// JSON representation. Character values have trailing blanks removed
    /// when `trim` is set (DBF character fields are space padded).
    pub fn to_json(&self, trim: bool) -> Value {
        match self {
            Val::Null => Value::Null,
            Val::Bool(b) => json!(b),
            Val::Num(n) => num_json(*n),
            Val::Str(s) if trim => json!(s.trim_end()),
            Val::Str(s) => json!(s),
            Val::Date(Some(d)) => json!(d.format("%Y-%m-%d").to_string()),
            Val::DateTime(Some(dt)) => json!(dt.format("%Y-%m-%dT%H:%M:%S").to_string()),
            Val::Date(None) | Val::DateTime(None) => Value::Null,
            Val::Binary(b) => json!({ "binary_bytes": b.len() }),
        }
    }
}

fn num_json(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
        json!(n as i64)
    } else if n.is_finite() {
        json!(n)
    } else {
        Value::Null
    }
}

/// Julian day number of 1970-01-01.
const JDN_UNIX_EPOCH: i64 = 2_440_588;

fn date_from_jdn(jdn: i64) -> Option<NaiveDate> {
    if jdn <= 0 {
        return None;
    }
    NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(Duration::days(jdn - JDN_UNIX_EPOCH))
}

fn parse_ascii_num(raw: &[u8]) -> Val {
    let s = String::from_utf8_lossy(raw);
    let s = s.trim().trim_matches('\0');
    if s.is_empty() {
        return Val::Num(0.0);
    }
    match s.parse::<f64>() {
        Ok(n) => Val::Num(n),
        // Overflowed numeric fields are stored as asterisks.
        Err(_) => Val::Str(s.to_string()),
    }
}

/// Decode a non-memo field.
pub fn decode_fixed(
    field: &FieldDef,
    raw: &[u8],
    enc: &'static Encoding,
    short: bool,
) -> Result<Val> {
    let fixed = |n: usize| -> Result<&[u8]> {
        raw.get(..n).ok_or_else(|| {
            FoxProError::Validation(format!("field {} is shorter than {n} bytes", field.name))
        })
    };
    let v = match field.kind {
        'C' => {
            if field.is_binary() {
                Val::Binary(raw.to_vec())
            } else {
                let (t, _) = enc.decode_without_bom_handling(raw);
                Val::Str(t.into_owned())
            }
        }
        'V' | 'Q' => {
            let data = if short {
                let len = *raw.last().unwrap_or(&0) as usize;
                &raw[..len.min(raw.len().saturating_sub(1))]
            } else {
                raw
            };
            if field.kind == 'Q' || field.is_binary() {
                Val::Binary(data.to_vec())
            } else {
                let (t, _) = enc.decode_without_bom_handling(data);
                Val::Str(t.into_owned())
            }
        }
        'N' | 'F' => parse_ascii_num(raw),
        'I' => {
            let b = fixed(4)?;
            Val::Num(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64)
        }
        'B' => {
            let b = fixed(8)?;
            Val::Num(f64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
        }
        'Y' => {
            let b = fixed(8)?;
            Val::Num(i64::from_le_bytes(b.try_into().unwrap_or([0; 8])) as f64 / 10_000.0)
        }
        'D' => {
            let s = String::from_utf8_lossy(raw);
            let s = s.trim();
            if s.is_empty() || s.chars().all(|c| c == '0') {
                Val::Date(None)
            } else {
                Val::Date(NaiveDate::parse_from_str(s, "%Y%m%d").ok())
            }
        }
        'T' => {
            let b = fixed(8)?;
            let jdn = i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64;
            let ms = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as i64;
            Val::DateTime(
                date_from_jdn(jdn)
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    // VFP stores milliseconds; round to whole seconds.
                    .map(|dt| dt + Duration::seconds((ms + 500) / 1000)),
            )
        }
        'L' => match raw.first() {
            Some(b'T' | b't' | b'Y' | b'y') => Val::Bool(true),
            Some(b'F' | b'f' | b'N' | b'n') => Val::Bool(false),
            _ => Val::Null,
        },
        '0' => Val::Binary(raw.to_vec()),
        other => {
            return Err(FoxProError::Unsupported(format!(
                "field {} has unsupported type '{other}'",
                field.name
            )));
        }
    };
    Ok(v)
}

/// Bytes of an empty value for a fixed-size field.
pub fn blank(field: &FieldDef) -> Vec<u8> {
    let len = field.length as usize;
    match field.kind {
        'I' | 'B' | 'Y' | 'T' | '0' | 'Q' => vec![0u8; len],
        'L' => vec![b'F'; len],
        _ => vec![b' '; len],
    }
}

/// Encode a number into an N/F field, rejecting values that do not fit.
pub fn encode_numeric(field: &FieldDef, value: f64) -> Result<Vec<u8>> {
    if !matches!(field.kind, 'N' | 'F') {
        return Err(FoxProError::Validation(format!(
            "field {} is not a numeric field",
            field.name
        )));
    }
    if !value.is_finite() {
        return Err(FoxProError::Validation(format!(
            "field {}: value must be a finite number",
            field.name
        )));
    }
    let len = field.length as usize;
    let dec = field.decimals as usize;
    // Like VFP, give up decimal places before refusing a value that is too
    // wide (FRX positions such as N(9,3) routinely exceed 99999.999).
    for d in (0..=dec).rev() {
        let text = format!("{value:>len$.d$}");
        if text.len() <= len {
            return Ok(text.into_bytes());
        }
    }
    Err(FoxProError::Validation(format!(
        "value {value} does not fit numeric field {} (N({len},{dec}))",
        field.name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_datetime_and_currency() {
        let f = FieldDef::new("T", 'T', 8, 0);
        let mut raw = (2_460_000i32).to_le_bytes().to_vec(); // 2023-02-24
        raw.extend_from_slice(&(3_600_000u32).to_le_bytes());
        let v = decode_fixed(&f, &raw, encoding_rs::WINDOWS_1252, false).unwrap();
        assert_eq!(v.to_json(true), json!("2023-02-24T01:00:00"));

        let f = FieldDef::new("Y", 'Y', 8, 4);
        let raw = 12_345_600i64.to_le_bytes();
        let v = decode_fixed(&f, &raw, encoding_rs::WINDOWS_1252, false).unwrap();
        assert_eq!(v, Val::Num(1234.56));
    }

    #[test]
    fn decodes_empty_date_and_logical() {
        let f = FieldDef::new("D", 'D', 8, 0);
        let v = decode_fixed(&f, b"        ", encoding_rs::WINDOWS_1252, false).unwrap();
        assert_eq!(v, Val::Date(None));
        let f = FieldDef::new("L", 'L', 1, 0);
        let v = decode_fixed(&f, b"?", encoding_rs::WINDOWS_1252, false).unwrap();
        assert_eq!(v, Val::Null);
    }
}
