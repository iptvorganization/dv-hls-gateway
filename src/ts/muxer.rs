//! TS muxer：协调 PAT/PMT 注入、AU→PES→TS 打包、PCR、音视频交错，输出一个 TS 段的字节。

use crate::clock::{wrap33, ClockState};
use crate::hevc::annexb::AccessUnit;
use crate::mp4::{DoviConfig, ParamSets};

use super::descriptor::dolby_vision_descriptor;
use super::packet::{pack_pes, pack_section, ContinuityCounters};
use super::pat::build_pat_section;
use super::pes::build_pes;
use super::pmt::build_pmt_section;
use super::{
    AudioCodec, VideoCodec, PID_AUDIO, PID_PAT, PID_PMT, PID_VIDEO, STREAM_ID_AUDIO,
    STREAM_ID_VIDEO,
};

const AUDIO_LEADS_VIDEO_TOLERANCE_90K: u64 = 90_000 / 4;

/// 一个音频访问单元（已解密的帧字节 + 时间戳，源 timescale）。
#[derive(Debug, Clone)]
pub struct AudioUnit {
    pub data: Vec<u8>,
    pub pts: u64,
}

/// TS muxer 配置。
pub struct TsMuxer {
    param_sets: Vec<Vec<u8>>,
    video_codec: VideoCodec,
    dovi: Option<DoviConfig>,
    audio_codec: Option<AudioCodec>,
    pub audio_timescale: u32,
    clock: ClockState,
    cc: ContinuityCounters,
}

impl TsMuxer {
    pub fn new(
        params: &ParamSets,
        video_codec: VideoCodec,
        dovi: Option<DoviConfig>,
        audio_codec: Option<AudioCodec>,
        audio_timescale: u32,
        clock: ClockState,
    ) -> Self {
        Self {
            param_sets: params.flat(),
            video_codec,
            dovi,
            audio_codec,
            audio_timescale,
            clock,
            cc: ContinuityCounters::new(),
        }
    }

    /// 验证 DOVI descriptor 是否能正确生成（调试用）。
    pub fn dovi_descriptor_bytes(&self) -> Option<Vec<u8>> {
        self.dovi.as_ref().map(dolby_vision_descriptor)
    }

    /// 写一个 TS 段：先 PAT/PMT，再按时间戳交错写入音视频 AU。
    /// 返回该段的完整 TS 字节。
    pub fn mux_segment(&mut self, video: &[AccessUnit], audio: &[AudioUnit]) -> Vec<u8> {
        let mut out = Vec::new();

        // 每段开头重发 PAT/PMT，保证段可独立解码
        let pat = build_pat_section(PID_PMT);
        out.extend_from_slice(&pack_section(PID_PAT, &pat, &mut self.cc));
        let audio_pmt = self.audio_codec.map(|c| (PID_AUDIO, c));
        let pmt = build_pmt_section(PID_VIDEO, self.video_codec, audio_pmt, self.dovi.as_ref());
        out.extend_from_slice(&pack_section(PID_PMT, &pmt, &mut self.cc));

        let mut vi = 0usize;
        let mut ai = 0usize;
        let audio_offset_90k = self.segment_audio_offset_90k(video, audio);

        while vi < video.len() || ai < audio.len() {
            let write_video = match (video.get(vi), audio.get(ai)) {
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(v), Some(a)) => {
                    // 先写首个视频 AU，确保段开头尽快出现 PCR/RAI；之后按 90k 时间戳交错。
                    vi == 0
                        || self.clock.pts_dts(v.dts, v.cts_offset).1
                            <= self.audio_pts90(a, audio_offset_90k)
                }
                (None, None) => break,
            };

            if write_video {
                let au = &video[vi];
                let (pts90, dts90) = self.clock.pts_dts(au.dts, au.cts_offset);
                let mut es = Vec::new();
                au.write_annexb(&mut es, &self.param_sets);
                let pes = build_pes(STREAM_ID_VIDEO, pts90, Some(dts90), &es);
                // PCR 放每个视频 AU 的首包（密度足够，~20-42ms/帧）；首 AU 带 RAI。
                // PCR 略小于 DTS（提前 ~开播 buffer 的一部分），保证 PCR < DTS。
                let pcr = Some(dts90.saturating_sub(self.clock.base_offset_90k.min(dts90)));
                let rai = vi == 0 || au.is_irap;
                pack_pes(PID_VIDEO, &pes, pcr, rai, &mut self.cc, &mut out);
                vi += 1;
            } else {
                let au = &audio[ai];
                let pts90 = self.audio_pts90(au, audio_offset_90k);
                let pes = build_pes(STREAM_ID_AUDIO, pts90, None, &au.data);
                pack_pes(PID_AUDIO, &pes, None, false, &mut self.cc, &mut out);
                ai += 1;
            }
        }

        out
    }

    fn audio_pts90(&self, au: &AudioUnit, segment_offset_90k: u64) -> u64 {
        wrap33(
            self.clock
                .audio_pts(au.pts, self.audio_timescale)
                .saturating_add(segment_offset_90k),
        )
    }

    fn segment_audio_offset_90k(&self, video: &[AccessUnit], audio: &[AudioUnit]) -> u64 {
        let (Some(first_video), Some(first_audio)) = (video.first(), audio.first()) else {
            return 0;
        };
        let first_video_pts = self
            .clock
            .pts_dts(first_video.dts, first_video.cts_offset)
            .0;
        let first_audio_pts = self.clock.audio_pts(first_audio.pts, self.audio_timescale);
        if first_audio_pts + AUDIO_LEADS_VIDEO_TOLERANCE_90K < first_video_pts {
            first_video_pts - first_audio_pts
        } else {
            0
        }
    }
}
