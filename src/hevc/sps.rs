//! 从 SPS 提取 VUI 的 colour transfer_characteristics，用于判定 SDR/HDR10(PQ)/HLG。
//!
//! transfer_characteristics 值（H.273）：1=BT.709(SDR), 16=PQ(SMPTE2084/HDR10), 18=HLG。
//!
//! 只解析到 VUI 的 colour_description 为止；用最小指数哥伦布解码器。
//! HEVC SPS 与 H.264 SPS 的头部字段不同，分别处理。

use crate::ts::VideoRange;

/// 去 emulation-prevention（0x000003 → 0x0000）。
fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    let mut i = 0;
    while i < nal.len() {
        let b = nal[i];
        if zeros >= 2 && b == 0x03 && i + 1 < nal.len() && nal[i + 1] <= 0x03 {
            // 跳过 emulation byte
            zeros = 0;
            i += 1;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        i += 1;
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}
impl<'a> BitReader<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { data: d, bit: 0 }
    }
    fn u1(&mut self) -> u32 {
        let byte = self.bit / 8;
        if byte >= self.data.len() {
            return 0;
        }
        let off = 7 - (self.bit % 8);
        self.bit += 1;
        ((self.data[byte] >> off) & 1) as u32
    }
    fn u(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.u1();
        }
        v
    }
    /// 无符号指数哥伦布 ue(v)。
    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.u1() == 0 && zeros < 32 {
            zeros += 1;
        }
        if zeros == 0 {
            return 0;
        }
        (1 << zeros) - 1 + self.u(zeros)
    }
    /// 有符号 se(v)。
    fn se(&mut self) -> i32 {
        let k = self.ue();
        let sign = if k & 1 == 1 { 1 } else { -1 };
        sign * ((k + 1) / 2) as i32
    }
}

fn tc_to_range(tc: u32) -> VideoRange {
    match tc {
        16 => VideoRange::Pq,  // SMPTE ST 2084
        18 => VideoRange::Hlg, // ARIB STD-B67
        _ => VideoRange::Sdr,
    }
}

/// 解析 H.264 SPS（裸 NAL，含 1 字节 header）→ VideoRange。
pub fn h264_sps_range(nal: &[u8]) -> Option<VideoRange> {
    if nal.len() < 4 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[1..]); // 去 NAL header
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u(8);
    let _constraint = r.u(8);
    let _level = r.u(8);
    let _sps_id = r.ue();
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = r.ue();
        if chroma == 3 {
            r.u1();
        }
        r.ue(); // bit_depth_luma
        r.ue(); // bit_depth_chroma
        r.u1(); // qpprime
        let scaling = r.u1();
        if scaling == 1 {
            // 跳过 scaling lists（简化：保守放弃精确解析，回退 SDR）
            return Some(VideoRange::Sdr);
        }
    }
    r.ue(); // log2_max_frame_num
    let poc_type = r.ue();
    if poc_type == 0 {
        r.ue();
    } else if poc_type == 1 {
        r.u1();
        r.se();
        r.se();
        let n = r.ue();
        for _ in 0..n {
            r.se();
        }
    }
    r.ue(); // max_num_ref_frames
    r.u1(); // gaps_in_frame_num
    r.ue(); // pic_width_in_mbs
    r.ue(); // pic_height_in_map_units
    let frame_mbs_only = r.u1();
    if frame_mbs_only == 0 {
        r.u1();
    }
    r.u1(); // direct_8x8
    let cropping = r.u1();
    if cropping == 1 {
        r.ue();
        r.ue();
        r.ue();
        r.ue();
    }
    let vui_present = r.u1();
    if vui_present == 0 {
        return Some(VideoRange::Sdr);
    }
    // VUI
    let aspect = r.u1();
    if aspect == 1 {
        let idc = r.u(8);
        if idc == 255 {
            r.u(16);
            r.u(16);
        }
    }
    let overscan = r.u1();
    if overscan == 1 {
        r.u1();
    }
    let video_signal = r.u1();
    if video_signal == 1 {
        r.u(3); // video_format
        r.u1(); // full_range
        let colour_desc = r.u1();
        if colour_desc == 1 {
            r.u(8); // colour_primaries
            let tc = r.u(8); // transfer_characteristics
            r.u(8); // matrix
            return Some(tc_to_range(tc));
        }
    }
    Some(VideoRange::Sdr)
}

/// 解析 HEVC SPS（裸 NAL，含 2 字节 header）→ VideoRange。
/// 解析到 VUI colour_description。需跳过 profile_tier_level 与若干字段。
pub fn hevc_sps_range(nal: &[u8]) -> Option<VideoRange> {
    if nal.len() < 4 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..]); // 去 2 字节 NAL header
    let mut r = BitReader::new(&rbsp);
    r.u(4); // sps_video_parameter_set_id
    let max_sub_layers = r.u(3);
    r.u1(); // temporal_id_nesting
            // profile_tier_level(1, max_sub_layers)
    skip_ptl(&mut r, max_sub_layers);
    r.ue(); // sps_seq_parameter_set_id
    let chroma = r.ue();
    if chroma == 3 {
        r.u1();
    }
    r.ue(); // pic_width
    r.ue(); // pic_height
    let conf_win = r.u1();
    if conf_win == 1 {
        r.ue();
        r.ue();
        r.ue();
        r.ue();
    }
    r.ue(); // bit_depth_luma
    r.ue(); // bit_depth_chroma
    r.ue(); // log2_max_pic_order_cnt
    let sub_layer_ordering = r.u1();
    let start = if sub_layer_ordering == 1 {
        0
    } else {
        max_sub_layers
    };
    for _ in start..=max_sub_layers {
        r.ue();
        r.ue();
        r.ue();
    }
    r.ue(); // log2_min_luma_coding_block_size
    r.ue();
    r.ue();
    r.ue();
    r.ue();
    r.ue();
    let scaling = r.u1();
    if scaling == 1 {
        return Some(VideoRange::Sdr); // 跳过 scaling list 太复杂，回退
    }
    r.u1(); // amp_enabled
    r.u1(); // sample_adaptive_offset
    let pcm = r.u1();
    if pcm == 1 {
        r.u(4);
        r.u(4);
        r.ue();
        r.ue();
        r.u1();
    }
    let num_short_term = r.ue();
    if num_short_term > 0 && num_short_term < 65 {
        // 跳过 st_ref_pic_set 太复杂 → 回退（多数 HDR 流 VUI 仍可达，但保守起见）
        return Some(VideoRange::Sdr);
    }
    let long_term = r.u1();
    if long_term == 1 {
        let n = r.ue();
        for _ in 0..n {
            r.ue();
            r.u1();
        }
    }
    r.u1(); // temporal_mvp
    r.u1(); // strong_intra_smoothing
    let vui = r.u1();
    if vui == 0 {
        return Some(VideoRange::Sdr);
    }
    let aspect = r.u1();
    if aspect == 1 {
        let idc = r.u(8);
        if idc == 255 {
            r.u(16);
            r.u(16);
        }
    }
    let overscan = r.u1();
    if overscan == 1 {
        r.u1();
    }
    let video_signal = r.u1();
    if video_signal == 1 {
        r.u(3);
        r.u1();
        let colour_desc = r.u1();
        if colour_desc == 1 {
            r.u(8);
            let tc = r.u(8);
            r.u(8);
            return Some(tc_to_range(tc));
        }
    }
    Some(VideoRange::Sdr)
}

/// 跳过 HEVC profile_tier_level。
fn skip_ptl(r: &mut BitReader, max_sub_layers: u32) {
    // general: 2+1+5 + 32 + 4 + 43 + 1 = 88 bits + 8 (general_level_idc)
    r.u(2);
    r.u1();
    r.u(5);
    r.u(32);
    r.u(4);
    r.u(32);
    r.u(11);
    r.u(8); // general_level_idc
    if max_sub_layers == 0 {
        return;
    }
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..(max_sub_layers as usize).min(8) {
        profile_present[i] = r.u1() == 1;
        level_present[i] = r.u1() == 1;
    }
    if max_sub_layers > 0 {
        for _ in max_sub_layers..8 {
            r.u(2);
        }
    }
    for i in 0..(max_sub_layers as usize).min(8) {
        if profile_present[i] {
            r.u(2);
            r.u1();
            r.u(5);
            r.u(32);
            r.u(4);
            r.u(43);
            r.u1();
        }
        if level_present[i] {
            r.u(8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ue_decode() {
        // bits 1 -> ue=0 ; 010 -> ue=1 ; 011 -> ue=2
        let mut r = BitReader::new(&[0b1_010_011_0]);
        assert_eq!(r.ue(), 0);
        assert_eq!(r.ue(), 1);
        assert_eq!(r.ue(), 2);
    }
    #[test]
    fn unescape() {
        assert_eq!(
            unescape_rbsp(&[0x00, 0x00, 0x03, 0x01]),
            vec![0x00, 0x00, 0x01]
        );
    }
}
