//! `dvcC` / `dvvC` box 解析 → Dolby Vision 配置。
//!
//! 实测 dvcC payload (24字节，有效前5): `01 00 0a 35 00`
//!   ver_major=1 ver_minor=0 profile=5 level=6 rpu=1 el=0 bl=1 compat=0
//!
//! 位布局（dvcC，与 TS dolby_vision_video_descriptor 的 payload 相同）：
//! ```text
//! byte0: dv_version_major
//! byte1: dv_version_minor
//! byte2: dv_profile(7) | dv_level高1位
//! byte3: dv_level低5位(<<3) | rpu(1) | el(1) | bl(1)
//! byte4: dv_bl_signal_compatibility_id(4) | reserved(4)
//! ```

use super::boxes::{find_box, CONTAINERS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoviConfig {
    pub version_major: u8,
    pub version_minor: u8,
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    pub bl_compatibility_id: u8,
}

impl DoviConfig {
    /// 从 dvcC/dvvC payload（至少 5 字节）解析。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 5 {
            return None;
        }
        let version_major = payload[0];
        let version_minor = payload[1];
        let val = ((payload[2] as u32) << 16) | ((payload[3] as u32) << 8) | (payload[4] as u32);
        let profile = ((val >> 17) & 0x7F) as u8;
        let level = ((val >> 11) & 0x3F) as u8;
        let rpu_present = (val >> 10) & 1 == 1;
        let el_present = (val >> 9) & 1 == 1;
        let bl_present = (val >> 8) & 1 == 1;
        let bl_compatibility_id = ((val >> 4) & 0xF) as u8;
        Some(Self {
            version_major,
            version_minor,
            profile,
            level,
            rpu_present,
            el_present,
            bl_present,
            bl_compatibility_id,
        })
    }

    /// 在 init 段数据里查找 dvcC（或 dvvC）并解析。
    pub fn find_in_init(init: &[u8]) -> Option<Self> {
        // dvcC 在 stsd → (encv/dvh1/dvhe/hvc1) 内部，sample entry 不是标准容器，
        // 故直接全局扫描 dvcC/dvvC box 头。
        if let Some(p) =
            find_box(init, b"dvcC", CONTAINERS).or_else(|| scan_anywhere(init, b"dvcC"))
        {
            return Self::parse(p);
        }
        if let Some(p) =
            find_box(init, b"dvvC", CONTAINERS).or_else(|| scan_anywhere(init, b"dvvC"))
        {
            return Self::parse(p);
        }
        None
    }

    /// HLS CODECS 属性里的视频 codec 字符串，例如 "dvh1.05.06"。
    pub fn codec_string(&self) -> String {
        format!("dvh1.{:02}.{:02}", self.profile, self.level)
    }
}

/// sample entry 内的 box 无法靠标准容器递归找到，这里在原始字节里暴力扫描 box 头。
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
    fn parse_real_dvcc() {
        // 实测 dvcC payload 前5字节
        let payload = [0x01, 0x00, 0x0a, 0x35, 0x00];
        let cfg = DoviConfig::parse(&payload).unwrap();
        assert_eq!(cfg.version_major, 1);
        assert_eq!(cfg.version_minor, 0);
        assert_eq!(cfg.profile, 5);
        assert_eq!(cfg.level, 6);
        assert!(cfg.rpu_present);
        assert!(!cfg.el_present);
        assert!(cfg.bl_present);
        assert_eq!(cfg.bl_compatibility_id, 0);
        assert_eq!(cfg.codec_string(), "dvh1.05.06");
    }
}
