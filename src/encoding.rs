//! Text encoding helpers shared by source, form, report and table code.
//!
//! Visual FoxPro files are almost always stored in a Windows ANSI code page
//! (Windows-1252 for western projects, Windows-874 for Thai projects), while
//! modern tools produce UTF-8. These helpers detect the encoding of existing
//! bytes and refuse to write text that cannot be represented losslessly.

use crate::error::{FoxProError, Result};
use encoding_rs::Encoding;

pub const UTF8: &str = "utf-8";
#[cfg(test)]
pub const WINDOWS_1252: &str = "windows-1252";
#[cfg(test)]
pub const WINDOWS_874: &str = "windows-874";

/// Decode bytes, returning `(encoding_label, text)`.
///
/// `preferred` forces a specific encoding. Otherwise UTF-8 is tried first and
/// then Windows-1252 / Windows-874 are scored against each other.
pub fn decode_bytes(bytes: &[u8], preferred: Option<&str>) -> (String, String) {
    if let Some(enc) = preferred.and_then(|l| Encoding::for_label(l.as_bytes())) {
        let (text, _) = enc.decode_without_bom_handling(bytes);
        return (enc.name().to_lowercase(), text.into_owned());
    }

    if let Ok(text) = std::str::from_utf8(bytes) {
        return (UTF8.to_string(), text.to_string());
    }

    let enc = detect_ansi(bytes);
    let (text, _) = enc.decode_without_bom_handling(bytes);
    (enc.name().to_lowercase(), text.into_owned())
}

/// Choose between Windows-1252 and Windows-874 for non-UTF-8 bytes.
pub fn detect_ansi(bytes: &[u8]) -> &'static Encoding {
    let win1252_invalid = bytes
        .iter()
        .filter(|&&b| matches!(b, 0x81 | 0x8D | 0x8F | 0x90 | 0x9D))
        .count();
    let win874_invalid = bytes
        .iter()
        .filter(|&&b| matches!(b, 0x80 | 0x85 | 0x91..=0x97))
        .count();

    if win874_invalid < win1252_invalid {
        return encoding_rs::WINDOWS_874;
    }
    if win1252_invalid < win874_invalid {
        return encoding_rs::WINDOWS_1252;
    }
    // Tie: Thai text uses the 0xA1..0xFB range heavily; western text rarely
    // produces long runs of those bytes.
    let high = bytes.iter().filter(|&&b| b >= 0xA1).count();
    let thai_consonants = bytes
        .iter()
        .filter(|&&b| (0xA1..=0xCE).contains(&b))
        .count();
    if high > 0 && thai_consonants * 2 >= high {
        encoding_rs::WINDOWS_874
    } else {
        encoding_rs::WINDOWS_1252
    }
}

pub fn lookup(label: &str) -> Result<&'static Encoding> {
    Encoding::for_label(label.trim().as_bytes())
        .ok_or_else(|| FoxProError::Encoding(format!("Unsupported encoding: {label}")))
}

/// Encode text, failing instead of silently substituting characters that the
/// target encoding cannot represent.
#[cfg(test)]
pub fn encode_string(text: &str, label: &str) -> Result<Vec<u8>> {
    encode_with(text, lookup(label)?)
}

pub fn encode_with(text: &str, enc: &'static Encoding) -> Result<Vec<u8>> {
    if enc == encoding_rs::UTF_8 {
        return Ok(text.as_bytes().to_vec());
    }
    let (bytes, _, had_errors) = enc.encode(text);
    if had_errors {
        let bad: String = text
            .chars()
            .filter(|c| {
                let mut buf = [0u8; 4];
                enc.encode(c.encode_utf8(&mut buf)).2
            })
            .take(10)
            .collect();
        return Err(FoxProError::Encoding(format!(
            "text contains characters that cannot be stored as {}: {:?}",
            enc.name(),
            bad
        )));
    }
    Ok(bytes.into_owned())
}

pub fn has_thai(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}'))
}

/// Best ANSI encoding for new FoxPro content: Windows-874 when the text
/// contains Thai characters, Windows-1252 otherwise.
pub fn ansi_for_text(text: &str) -> &'static Encoding {
    if has_thai(text) {
        encoding_rs::WINDOWS_874
    } else {
        encoding_rs::WINDOWS_1252
    }
}

/// Map a DBF code page mark (header byte 29) to an encoding.
pub fn from_codepage_mark(mark: u8) -> Option<&'static Encoding> {
    let enc = match mark {
        0x03 | 0x57 => encoding_rs::WINDOWS_1252,
        0x7C => encoding_rs::WINDOWS_874,
        0x7B => encoding_rs::SHIFT_JIS,
        0x7A => encoding_rs::GBK,
        0x79 => encoding_rs::EUC_KR,
        0x78 => encoding_rs::BIG5,
        0x7D => encoding_rs::WINDOWS_1255,
        0x7E => encoding_rs::WINDOWS_1256,
        0xC8 => encoding_rs::WINDOWS_1250,
        0xC9 => encoding_rs::WINDOWS_1251,
        0xCA => encoding_rs::WINDOWS_1254,
        0xCB => encoding_rs::WINDOWS_1253,
        0x65 => encoding_rs::IBM866,
        _ => return None,
    };
    Some(enc)
}

/// Map an encoding to the DBF code page mark written into new tables.
pub fn to_codepage_mark(enc: &'static Encoding) -> u8 {
    match enc.name() {
        "windows-874" => 0x7C,
        "windows-1252" => 0x03,
        "Shift_JIS" => 0x7B,
        "GBK" => 0x7A,
        "EUC-KR" => 0x79,
        "Big5" => 0x78,
        "windows-1255" => 0x7D,
        "windows-1256" => 0x7E,
        "windows-1250" => 0xC8,
        "windows-1251" => 0xC9,
        "windows-1254" => 0xCA,
        "windows-1253" => 0xCB,
        "IBM866" => 0x65,
        _ => 0x00,
    }
}

pub fn codepage_name(mark: u8) -> String {
    match mark {
        0x00 => "none".to_string(),
        0x01 => "437 (DOS US)".to_string(),
        0x02 => "850 (DOS Multilingual)".to_string(),
        m => from_codepage_mark(m)
            .map(|e| e.name().to_lowercase())
            .unwrap_or_else(|| format!("unknown (0x{m:02X})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_rejects_unmappable_characters() {
        let err = encode_string("สวัสดี", WINDOWS_1252).unwrap_err();
        assert!(matches!(err, FoxProError::Encoding(_)));
    }

    #[test]
    fn thai_roundtrip_windows_874() {
        let bytes = encode_string("บันทึก", WINDOWS_874).unwrap();
        let (label, text) = decode_bytes(&bytes, None);
        assert_eq!(label, WINDOWS_874);
        assert_eq!(text, "บันทึก");
    }

    #[test]
    fn western_ansi_detected_as_1252() {
        let bytes = encode_string("Café crème", WINDOWS_1252).unwrap();
        let (label, text) = decode_bytes(&bytes, None);
        assert_eq!(label, WINDOWS_1252);
        assert_eq!(text, "Café crème");
    }
}
