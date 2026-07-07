//! PMT 内的描述符。重点是 Dolby Vision 的 dolby_vision_video_descriptor (tag 0xB0)。

use super::bitwriter::BitWriter;
use crate::mp4::DoviConfig;

pub const DOVI_DESCRIPTOR_TAG: u8 = 0xB0;

/// 构造 dolby_vision_video_descriptor（含 tag + length）。
///
/// 实测：Profile5/Level6/rpu1/el0/bl1/compat0 → `B0 05 01 00 0A 35 00`，
/// 其 5 字节 payload 与 dvcC 前 5 字节位布局相同。
/// 这里逐字段用 BitWriter 写，便于支持 Profile 8 等其它配置。
pub fn dolby_vision_descriptor(cfg: &DoviConfig) -> Vec<u8> {
    let mut payload = BitWriter::new();
    payload.u8(cfg.version_major);
    payload.u8(cfg.version_minor);
    payload.bits(cfg.profile as u32, 7);
    payload.bits(cfg.level as u32, 6);
    payload.bit(cfg.rpu_present);
    payload.bit(cfg.el_present);
    payload.bit(cfg.bl_present);
    payload.bits(cfg.bl_compatibility_id as u32, 4);
    payload.align(false); // reserved 补 0 到字节边界
    let payload = payload.into_bytes();

    let mut out = Vec::with_capacity(2 + payload.len());
    out.push(DOVI_DESCRIPTOR_TAG);
    out.push(payload.len() as u8);
    out.extend_from_slice(&payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_p5() -> DoviConfig {
        DoviConfig {
            version_major: 1,
            version_minor: 0,
            profile: 5,
            level: 6,
            rpu_present: true,
            el_present: false,
            bl_present: true,
            bl_compatibility_id: 0,
        }
    }

    #[test]
    fn dovi_descriptor_matches_real_bytes() {
        let d = dolby_vision_descriptor(&cfg_p5());
        assert_eq!(d, vec![0xB0, 0x05, 0x01, 0x00, 0x0A, 0x35, 0x00]);
    }

    #[test]
    fn dovi_descriptor_roundtrip() {
        let cfg = cfg_p5();
        let d = dolby_vision_descriptor(&cfg);
        // 反解 payload
        let p = &d[2..];
        let val = ((p[2] as u32) << 16) | ((p[3] as u32) << 8) | (p[4] as u32);
        assert_eq!((val >> 17) & 0x7F, cfg.profile as u32);
        assert_eq!((val >> 11) & 0x3F, cfg.level as u32);
        assert_eq!((val >> 10) & 1, 1);
        assert_eq!((val >> 8) & 1, 1);
    }
}
