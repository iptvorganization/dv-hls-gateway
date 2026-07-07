//! Helpers for extracting common CENC KIDs from PlayReady data URIs.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

pub fn kid_from_playready_data_uri(uri: &str) -> Option<String> {
    let payload = uri.strip_prefix("data:")?;
    let (_, b64) = payload.split_once(',')?;
    let data = STANDARD.decode(b64.trim()).ok()?;
    kid_from_playready_blob(&data)
}

pub fn kid_from_playready_blob(data: &[u8]) -> Option<String> {
    let ascii = String::from_utf8_lossy(data).into_owned();
    let utf16 = utf16le_lossy(data);
    extract_kid_value(&utf16)
        .or_else(|| extract_kid_value(&ascii))
        .and_then(|v| playready_kid_value_to_common_hex(&v))
}

fn utf16le_lossy(data: &[u8]) -> String {
    let units = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]));
    std::char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{fffd}'))
        .collect()
}

fn extract_kid_value(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("<kid") {
        let tag_start = search_from + rel;
        if lower[tag_start..].starts_with("</kid") {
            search_from = tag_start + 5;
            continue;
        }
        let tag_end = lower[tag_start..].find('>').map(|p| tag_start + p)?;
        let tag = &text[tag_start..=tag_end];
        if let Some(value) = attr_value_case_insensitive(tag, "VALUE") {
            return Some(value);
        }
        let content_start = tag_end + 1;
        let content_end = lower[content_start..]
            .find("</kid>")
            .map(|p| content_start + p)?;
        let value = text[content_start..content_end].trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
        search_from = tag_end + 1;
    }
    None
}

fn attr_value_case_insensitive(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{}=", name.to_ascii_lowercase());
    let pos = lower.find(&needle)?;
    let value_start = pos + needle.len();
    let bytes = tag.as_bytes();
    let quote = *bytes.get(value_start)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &tag[value_start + 1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].trim().to_string())
}

fn playready_kid_value_to_common_hex(value: &str) -> Option<String> {
    let value = value.trim();
    let raw = if value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        hex::decode(value).ok()?
    } else {
        STANDARD.decode(value).ok()?
    };
    if raw.len() != 16 {
        return None;
    }

    let common = [
        raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10],
        raw[11], raw[12], raw[13], raw[14], raw[15],
    ];
    Some(hex::encode(common))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_playready_guid_to_common_kid() {
        assert_eq!(
            playready_kid_value_to_common_hex("724Zm6/f3kyfZ8FOH2OUyg==").unwrap(),
            "9b196eefdfaf4cde9f67c14e1f6394ca"
        );
    }

    #[test]
    fn extracts_utf16_kid_from_data_uri() {
        let xml = r#"<WRMHEADER><DATA><KID>724Zm6/f3kyfZ8FOH2OUyg==</KID></DATA></WRMHEADER>"#;
        let utf16: Vec<u8> = xml.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let uri = format!("data:text/plain;base64,{}", STANDARD.encode(utf16));
        assert_eq!(
            kid_from_playready_data_uri(&uri).unwrap(),
            "9b196eefdfaf4cde9f67c14e1f6394ca"
        );
    }
}
