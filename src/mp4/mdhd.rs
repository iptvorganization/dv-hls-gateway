//! `mdhd` media header helpers.

use super::boxes::{find_box, CONTAINERS};

/// Read media timescale from the first `mdhd` box in an init segment.
pub fn timescale_from_init(init: &[u8]) -> Option<u32> {
    let mdhd = find_box(init, b"mdhd", CONTAINERS).or_else(|| scan_anywhere(init, b"mdhd"))?;
    parse_mdhd_timescale(mdhd)
}

fn parse_mdhd_timescale(payload: &[u8]) -> Option<u32> {
    let version = *payload.first()?;
    let offset = match version {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    if payload.len() < offset + 4 {
        return None;
    }
    Some(u32::from_be_bytes([
        payload[offset],
        payload[offset + 1],
        payload[offset + 2],
        payload[offset + 3],
    ]))
}

fn scan_anywhere<'a>(data: &'a [u8], target: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + 8 <= data.len() {
        if &data[i + 4..i + 8] == target {
            let size =
                u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
            if size >= 8 && i + size <= data.len() {
                return Some(&data[i + 8..i + size]);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mdhd_v0_timescale() {
        let mut p = vec![0u8; 32];
        p[12..16].copy_from_slice(&48_000u32.to_be_bytes());
        assert_eq!(parse_mdhd_timescale(&p), Some(48_000));
    }

    #[test]
    fn parses_mdhd_v1_timescale() {
        let mut p = vec![0u8; 40];
        p[0] = 1;
        p[20..24].copy_from_slice(&90_000u32.to_be_bytes());
        assert_eq!(parse_mdhd_timescale(&p), Some(90_000));
    }
}
