//! AAC 处理：解析 esds 里的 AudioSpecificConfig，给 raw AAC 帧加 ADTS 头。
//!
//! mp4 里的 AAC 是 raw（无同步头），进 MPEG-TS 必须封成 ADTS（每帧 7 字节头），
//! 否则播放器无法解析采样率/声道。EC-3/AC-3 自带 syncframe，无需此处理。

use super::boxes::{find_box, CONTAINERS};

/// AAC 音频规格（来自 AudioSpecificConfig）。
#[derive(Debug, Clone, Copy)]
pub struct AacConfig {
    /// audioObjectType（AAC-LC=2, HE-AAC=5, HE-AACv2=29）。
    pub object_type: u8,
    /// 采样率索引（0..15）。
    pub freq_index: u8,
    /// 声道配置（1=mono, 2=stereo, ...）。
    pub channel_config: u8,
}

const FREQ_TABLE: [u32; 16] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0,
    0,
];

impl AacConfig {
    pub fn sample_rate(&self) -> u32 {
        *FREQ_TABLE.get(self.freq_index as usize).unwrap_or(&48000)
    }

    /// 从 AudioSpecificConfig 字节解析（至少 2 字节）。
    pub fn parse_asc(asc: &[u8]) -> Option<Self> {
        if asc.len() < 2 {
            return None;
        }
        let v = u16::from_be_bytes([asc[0], asc[1]]);
        let object_type = ((v >> 11) & 0x1F) as u8;
        let freq_index = ((v >> 7) & 0xF) as u8;
        let channel_config = ((v >> 3) & 0xF) as u8;
        Some(Self {
            object_type,
            freq_index,
            channel_config,
        })
    }

    /// 在 init 段里查找 esds → AudioSpecificConfig。
    pub fn find_in_init(init: &[u8]) -> Option<Self> {
        let esds = find_box(init, b"esds", CONTAINERS).or_else(|| scan_anywhere(init, b"esds"))?;
        // esds: version/flags(4) + tags。找 DecoderSpecificInfo tag=0x05。
        let asc = extract_asc_from_esds(esds)?;
        Self::parse_asc(&asc)
    }

    /// ADTS profile 字段 = objectType - 1（AAC-LC=2 → profile=1）。
    fn adts_profile(&self) -> u8 {
        // HE-AAC/HE-AACv2 解码为 AAC-LC 基础流时常用 profile=1；
        // 这里用 (object_type-1) 截断到 2 bit。
        let ot = if self.object_type == 0 {
            2
        } else {
            self.object_type
        };
        (ot - 1) & 0x3
    }

    /// 给一个 raw AAC 帧加 ADTS 头，返回完整 ADTS 帧。
    pub fn wrap_adts(&self, frame: &[u8]) -> Vec<u8> {
        let frame_len = frame.len() + 7;
        let mut h = [0u8; 7];
        // syncword 0xFFF + MPEG-4 + Layer 0 + protection_absent=1
        h[0] = 0xFF;
        h[1] = 0xF1;
        // profile(2) | freq_index(4) | private(1) | channel_config 高1位
        h[2] = (self.adts_profile() << 6)
            | ((self.freq_index & 0xF) << 2)
            | ((self.channel_config >> 2) & 0x1);
        // channel_config 低2位 | ... | frame_length 高2位
        h[3] = ((self.channel_config & 0x3) << 6) | (((frame_len >> 11) & 0x3) as u8);
        h[4] = ((frame_len >> 3) & 0xFF) as u8;
        h[5] = (((frame_len & 0x7) << 5) as u8) | 0x1F; // frame_len 低3位 + buffer_fullness 高5位(全1)
        h[6] = 0xFC; // buffer_fullness 低6位(全1) + num_frames-1=0
        let mut out = Vec::with_capacity(frame_len);
        out.extend_from_slice(&h);
        out.extend_from_slice(frame);
        out
    }
}

fn extract_asc_from_esds(esds: &[u8]) -> Option<Vec<u8>> {
    // 跳过 version/flags(4)，扫描找 tag 0x05 (DecSpecificInfo)，其后是长度(可能多字节)+ASC
    let mut i = 4;
    while i < esds.len() {
        if esds[i] == 0x05 {
            // 解析 expandable size（0x80 续位）
            let mut p = i + 1;
            let mut len = 0usize;
            for _ in 0..4 {
                if p >= esds.len() {
                    return None;
                }
                let b = esds[p];
                len = (len << 7) | (b & 0x7F) as usize;
                p += 1;
                if b & 0x80 == 0 {
                    break;
                }
            }
            if p + len <= esds.len() && len >= 2 {
                return Some(esds[p..p + len].to_vec());
            }
            return None;
        }
        i += 1;
    }
    None
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
    fn parse_real_asc() {
        // 实测 ASC = 0x1190 → AAC-LC, 48000Hz, stereo
        let cfg = AacConfig::parse_asc(&[0x11, 0x90]).unwrap();
        assert_eq!(cfg.object_type, 2);
        assert_eq!(cfg.freq_index, 3);
        assert_eq!(cfg.sample_rate(), 48000);
        assert_eq!(cfg.channel_config, 2);
    }

    #[test]
    fn adts_header_well_formed() {
        let cfg = AacConfig {
            object_type: 2,
            freq_index: 3,
            channel_config: 2,
        };
        let frame = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let adts = cfg.wrap_adts(&frame);
        assert_eq!(adts.len(), 7 + 4);
        // syncword
        assert_eq!(adts[0], 0xFF);
        assert_eq!(adts[1] & 0xF0, 0xF0);
        // frame length 字段还原
        let flen = (((adts[3] & 0x3) as usize) << 11)
            | ((adts[4] as usize) << 3)
            | ((adts[5] as usize) >> 5);
        assert_eq!(flen, 11);
        // freq_index 还原
        assert_eq!((adts[2] >> 2) & 0xF, 3);
        // channel_config 还原
        let ch = ((adts[2] & 1) << 2) | (adts[3] >> 6);
        assert_eq!(ch, 2);
        // 末尾是原始 frame
        assert_eq!(&adts[7..], &frame[..]);
    }

    #[test]
    fn extract_asc() {
        // 构造最小 esds: ver/flags(4) + ...05 02 1190
        let esds = vec![0, 0, 0, 0, 0x05, 0x02, 0x11, 0x90];
        let asc = extract_asc_from_esds(&esds).unwrap();
        assert_eq!(asc, vec![0x11, 0x90]);
    }
}
