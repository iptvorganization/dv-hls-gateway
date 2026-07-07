//! `hvcC` (HEVCDecoderConfigurationRecord) 解析 → 提取 VPS/SPS/PPS 裸 NAL。
//!
//! hvcC 布局（关键尾部）：
//! ```text
//! configurationVersion(1)
//! ... 21 字节固定头 ...
//! numOfArrays(1)  @ offset 22
//! 每个 array:
//!   array_completeness(1)|reserved(1)|NAL_unit_type(6)  (1 byte)
//!   numNalus (2)
//!   每个 nalu: nalUnitLength(2) + nalUnit bytes
//! ```

use super::boxes::{find_box, CONTAINERS};

/// 从 init 段提取的参数集（裸 NAL，不含起始码/长度前缀）。
#[derive(Debug, Clone, Default)]
pub struct ParamSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParamSets {
    /// 按 VPS→SPS→PPS 顺序铺平成一个列表，供 Annex-B 注入。
    pub fn flat(&self) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        v.extend(self.vps.iter().cloned());
        v.extend(self.sps.iter().cloned());
        v.extend(self.pps.iter().cloned());
        v
    }

    pub fn is_empty(&self) -> bool {
        self.vps.is_empty() && self.sps.is_empty() && self.pps.is_empty()
    }

    /// 解析 hvcC payload。
    pub fn parse_hvcc(payload: &[u8]) -> Option<Self> {
        if payload.len() < 23 {
            return None;
        }
        let num_arrays = payload[22] as usize;
        let mut p = 23;
        let mut ps = ParamSets::default();
        for _ in 0..num_arrays {
            if p + 3 > payload.len() {
                break;
            }
            let nal_type = payload[p] & 0x3F;
            let num_nalus = u16::from_be_bytes([payload[p + 1], payload[p + 2]]) as usize;
            p += 3;
            for _ in 0..num_nalus {
                if p + 2 > payload.len() {
                    return Some(ps);
                }
                let len = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
                p += 2;
                if p + len > payload.len() {
                    return Some(ps);
                }
                let nal = payload[p..p + len].to_vec();
                p += len;
                match nal_type {
                    32 => ps.vps.push(nal),
                    33 => ps.sps.push(nal),
                    34 => ps.pps.push(nal),
                    _ => {}
                }
            }
        }
        Some(ps)
    }

    /// 在 init 段里查找 hvcC 并解析。
    pub fn find_in_init(init: &[u8]) -> Option<Self> {
        let p = find_box(init, b"hvcC", CONTAINERS).or_else(|| scan_anywhere(init, b"hvcC"))?;
        Self::parse_hvcc(p)
    }
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
