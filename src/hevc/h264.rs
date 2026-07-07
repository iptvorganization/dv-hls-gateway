//! H.264/AVC NAL 单元类型（ISO/IEC 14496-10）。
//!
//! NAL header 1 字节：`forbidden(1) | nal_ref_idc(2) | nal_unit_type(5)`。
//! type = `byte0 & 0x1F`。

#[inline]
pub fn nal_type(first_byte: u8) -> u8 {
    first_byte & 0x1F
}

pub const NON_IDR_SLICE: u8 = 1;
pub const IDR_SLICE: u8 = 5;
pub const SEI: u8 = 6;
pub const SPS: u8 = 7;
pub const PPS: u8 = 8;
pub const AUD: u8 = 9;

/// IDR slice 即随机接入点。
#[inline]
pub fn is_irap(t: u8) -> bool {
    t == IDR_SLICE
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn types() {
        // SPS NAL header 常见 0x67 (ref_idc=3,type=7)
        assert_eq!(nal_type(0x67), SPS);
        assert_eq!(nal_type(0x68), PPS);
        assert_eq!(nal_type(0x65), IDR_SLICE);
        assert!(is_irap(IDR_SLICE));
        assert!(!is_irap(NON_IDR_SLICE));
    }
}
