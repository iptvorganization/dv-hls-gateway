//! 时基换算：DASH timescale (本流 24000) → MPEG-TS 90kHz 时钟。
//!
//! 关键正确性（实测踩坑后修正）：
//! - B 帧的 composition offset (cts) 可为负，导致源 `pts = dts + cts < dts`。
//!   但 MPEG-TS 要求每帧 `pts >= dts`。
//! - 解法：对整条流的 cts 统一加上 `cts_shift = -min(cts)`（≥0），把 composition
//!   timeline 平移，使所有 `cts' = cts - min_cts >= 0`，于是 `pts' = dts + cts' >= dts`。
//! - 音频 PTS 也跟随同一个 composition shift，否则视频被整体推迟后音频会相对提前。
//! - 再对 PTS/DTS 同加 `base_offset`（含一点起播 buffer），保证 dts ≥ 0、PCR < 首 DTS。
//! - 换算用绝对值 `ts * 90000 / timescale` 整数除，每帧独立算，误差 < 1 tick 不累积。

pub const PTS_MODULO: u64 = 1 << 33;

#[inline]
pub fn rescale_90k(ts: u64, timescale: u32) -> u64 {
    ((ts as u128 * 90_000) / timescale as u128) as u64
}

#[inline]
pub fn rescale_90k_signed(ts: i64, timescale: u32) -> i64 {
    (ts as i128 * 90_000 / timescale as i128) as i64
}

#[inline]
pub fn wrap33(v: u64) -> u64 {
    v % PTS_MODULO
}

#[derive(Debug, Clone, Copy)]
pub struct ClockState {
    /// composition timeline 平移量（源 timescale），= -min(cts)，≥ 0。
    pub cts_shift: u64,
    /// 加到 PTS/DTS 上的起始偏移（90kHz），保证 dts ≥ 0 + 起播 buffer。
    pub base_offset_90k: u64,
    pub timescale: u32,
}

impl ClockState {
    /// `min_cts` 是整条流里最小的 composition offset（源 timescale，可负）。
    /// `start_buffer_90k` 起播提前量（如 1 秒 = 90000）。
    pub fn new(min_cts: i64, timescale: u32, start_buffer_90k: u64) -> Self {
        let cts_shift = if min_cts < 0 { (-min_cts) as u64 } else { 0 };
        Self {
            cts_shift,
            base_offset_90k: start_buffer_90k,
            timescale,
        }
    }

    /// 由 dts(源 timescale) 和 cts_offset(源 timescale, 有符号) 计算 (pts90, dts90)。
    /// 保证 pts90 >= dts90 且都 ≥ 0。
    #[inline]
    pub fn pts_dts(&self, dts_ts: u64, cts_offset: i64) -> (u64, u64) {
        let dts90 = rescale_90k(dts_ts, self.timescale) + self.base_offset_90k;
        // pts = dts + (cts + cts_shift)，其中 cts + cts_shift >= 0
        let shifted_cts = cts_offset + self.cts_shift as i64;
        let pts90 = (dts90 as i64 + rescale_90k_signed(shifted_cts, self.timescale)) as u64;
        (wrap33(pts90), wrap33(dts90))
    }

    /// 音频只有 pts。
    #[inline]
    pub fn audio_pts(&self, pts_ts: u64, timescale: u32) -> u64 {
        let shift90 = rescale_90k(self.cts_shift, self.timescale);
        wrap33(rescale_90k(pts_ts, timescale) + shift90 + self.base_offset_90k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescale_basic() {
        assert_eq!(rescale_90k(24000, 24000), 90000);
        assert_eq!(rescale_90k(1001, 24000), 3753);
        assert_eq!(rescale_90k(10010, 24000), 37537);
    }

    #[test]
    fn bframe_pts_ge_dts() {
        // 实测前几帧 (dts, cts): (0,0)(1001,4004)(2002,1001)(3003,-2002)(4004,-2002)(5005,-1001)
        // min_cts = -2002 → cts_shift = 2002
        let clk = ClockState::new(-2002, 24000, 90000);
        let cases = [
            (0u64, 0i64),
            (1001, 4004),
            (2002, 1001),
            (3003, -2002),
            (4004, -2002),
            (5005, -1001),
        ];
        for (dts, cts) in cases {
            let (pts90, dts90) = clk.pts_dts(dts, cts);
            assert!(
                pts90 >= dts90,
                "pts {pts90} < dts {dts90} for dts_ts={dts} cts={cts}"
            );
        }
    }

    #[test]
    fn audio_pts_follows_video_composition_shift() {
        let clk = ClockState::new(-2002, 24000, 90000);
        assert_eq!(clk.audio_pts(0, 48000), 90000 + rescale_90k(2002, 24000));
    }

    #[test]
    fn wrap_33bit() {
        assert_eq!(wrap33(PTS_MODULO), 0);
        assert_eq!(wrap33(PTS_MODULO + 5), 5);
    }
}
