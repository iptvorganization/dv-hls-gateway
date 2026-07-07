//! MPEG-2 系统层用的 CRC-32（poly 0x04C11DB7, init 0xFFFFFFFF, 不反转）。
//! 用于 PAT/PMT section 末尾的 CRC_32 字段。

const POLY: u32 = 0x04C1_1DB7;

/// 计算 MPEG CRC-32。
pub fn mpeg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // ffmpeg 生成的真实 PAT section（不含 CRC），PMT PID=0x1000 → F000。
        // 已用 ffmpeg 输出的 TS 实测：该 section 的 CRC = 0x2AB104B2。
        let data = [
            0x00u8, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        let crc = mpeg_crc32(&data);
        assert_eq!(crc, 0x2AB1_04B2, "got {:08X}", crc);
    }
}
