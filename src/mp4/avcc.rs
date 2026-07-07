//! `avcC` (AVCDecoderConfigurationRecord) 解析 → 提取 H.264 SPS/PPS 裸 NAL。
//!
//! 布局：
//! ```text
//! configurationVersion(1) AVCProfileIndication(1) profile_compatibility(1) AVCLevelIndication(1)
//! reserved(6bits)+lengthSizeMinusOne(2bits) (1)
//! reserved(3bits)+numOfSPS(5bits) (1)
//!   每个 SPS: length(2) + nalu
//! numOfPPS(1)
//!   每个 PPS: length(2) + nalu
//! ```

use super::boxes::{find_box, CONTAINERS};
use super::hvcc::ParamSets;

/// 解析 avcC payload → SPS/PPS（放进 ParamSets 的 sps/pps，vps 留空）。
pub fn parse_avcc(payload: &[u8]) -> Option<ParamSets> {
    if payload.len() < 6 {
        return None;
    }
    let mut p = 5; // 跳过 version/profile/compat/level/lengthSize
    let num_sps = (payload[p] & 0x1F) as usize;
    p += 1;
    let mut ps = ParamSets::default();
    for _ in 0..num_sps {
        if p + 2 > payload.len() {
            return Some(ps);
        }
        let len = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
        p += 2;
        if p + len > payload.len() {
            return Some(ps);
        }
        ps.sps.push(payload[p..p + len].to_vec());
        p += len;
    }
    if p >= payload.len() {
        return Some(ps);
    }
    let num_pps = payload[p] as usize;
    p += 1;
    for _ in 0..num_pps {
        if p + 2 > payload.len() {
            return Some(ps);
        }
        let len = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
        p += 2;
        if p + len > payload.len() {
            return Some(ps);
        }
        ps.pps.push(payload[p..p + len].to_vec());
        p += len;
    }
    Some(ps)
}

/// 在 init 段里查找 avcC 并解析。
pub fn find_avcc_in_init(init: &[u8]) -> Option<ParamSets> {
    let p = find_box(init, b"avcC", CONTAINERS).or_else(|| scan_anywhere(init, b"avcC"))?;
    parse_avcc(p)
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

/// H.264 的 ParamSets 注入顺序：SPS 在前 PPS 在后（无 VPS）。flat() 已是此序。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_avcc() {
        // version,profile,compat,level,lengthSize, numSPS=1, [len=2,67 42], numPPS=1, [len=2,68 ce]
        let payload = [
            0x01, 0x42, 0x00, 0x1f, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x42, 0x01, 0x00, 0x02, 0x68,
            0xce,
        ];
        let ps = parse_avcc(&payload).unwrap();
        assert_eq!(ps.sps.len(), 1);
        assert_eq!(ps.pps.len(), 1);
        assert_eq!(ps.sps[0], vec![0x67, 0x42]);
        assert_eq!(ps.pps[0], vec![0x68, 0xce]);
        assert!(ps.vps.is_empty());
    }
}
