//! HEVC NAL 单元类型（ISO/IEC 23008-2）。
//!
//! NAL header 2 字节：`forbidden(1) | nal_unit_type(6) | layer_id(6) | tid_plus1(3)`。
//! type = `(byte0 >> 1) & 0x3F`。

/// 取 HEVC NAL 单元类型。
#[inline]
pub fn nal_type(first_byte: u8) -> u8 {
    (first_byte >> 1) & 0x3F
}

// 关心的 NAL 类型常量
pub const TRAIL_N: u8 = 0;
pub const TRAIL_R: u8 = 1;
pub const BLA_W_LP: u8 = 16;
pub const IDR_W_RADL: u8 = 19;
pub const IDR_N_LP: u8 = 20;
pub const CRA_NUT: u8 = 21;
pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;
pub const AUD: u8 = 35;
pub const SEI_PREFIX: u8 = 39;
pub const SEI_SUFFIX: u8 = 40;
/// Dolby Vision RPU（unspecified 62），必须原样透传。
pub const DV_RPU: u8 = 62;
/// Dolby Vision EL（unspecified 63）。Profile 5/8 单层不出现。
pub const DV_EL: u8 = 63;

/// 是否为 IRAP（随机接入点，即关键帧 / IDR/CRA/BLA）。
/// HLS 段必须以这种帧开头。
#[inline]
pub fn is_irap(t: u8) -> bool {
    (16..=23).contains(&t)
}

/// 是否为 VCL（视频编码层 slice）NAL：0..=31。
#[inline]
pub fn is_vcl(t: u8) -> bool {
    t <= 31
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_real_aud() {
        // 实测 AUD = 46 01 10 → type 35
        assert_eq!(nal_type(0x46), AUD);
    }

    #[test]
    fn parse_rpu() {
        // RPU NAL header 第一字节: type 62 → (62<<1)=0x7C, |forbidden0 = 0x7C
        assert_eq!(nal_type(0x7C), DV_RPU);
    }

    #[test]
    fn irap_classification() {
        assert!(is_irap(IDR_N_LP));
        assert!(is_irap(IDR_W_RADL));
        assert!(is_irap(CRA_NUT));
        assert!(!is_irap(TRAIL_R));
        assert!(!is_irap(DV_RPU));
    }
}
