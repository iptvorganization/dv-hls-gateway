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

/// 一个音频访问单元（帧字节 + 源 timescale 时间戳）。AC-4 帧需已带同步头。
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
    preserve_audio_lead: bool,
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
            preserve_audio_lead: false,
        }
    }

    /// Disable the drift heuristic for an audio and video pair that share
    /// a single normalized source timeline (e.g. HLS in-band fMP4).
    pub(crate) fn with_preserved_audio_lead(mut self, preserve_audio_lead: bool) -> Self {
        self.preserve_audio_lead = preserve_audio_lead;
        self
    }

    /// 验证 DOVI descriptor 是否能正确生成（调试用）。
    pub fn dovi_descriptor_bytes(&self) -> Option<Vec<u8>> {
        self.dovi.as_ref().map(dolby_vision_descriptor)
    }

    /// Video presentation origin for a segment-local WebVTT timestamp map.
    pub fn segment_pts_90k(&self, video: &[AccessUnit]) -> Option<u64> {
        video
            .first()
            .map(|au| self.clock.pts_dts(au.dts, au.cts_offset).0)
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
        let audio_stream_id = self
            .audio_codec
            .map_or(STREAM_ID_AUDIO, AudioCodec::pes_stream_id);

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
                let pes = build_pes(audio_stream_id, pts90, None, &au.data);
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
        if self.preserve_audio_lead {
            return 0;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts::{SYNC_BYTE, TS_PACKET_SIZE};

    #[test]
    fn common_source_clock_preserves_lead_over_legacy_drift_threshold() {
        let muxer = TsMuxer::new(
            &ParamSets::default(),
            VideoCodec::H264,
            None,
            Some(AudioCodec::Ac4),
            48_000,
            ClockState::new(0, 1_000, 90_000),
        );
        let video = [AccessUnit::from_sample(b"", 400, 0, VideoCodec::H264)];
        let audio = [AudioUnit {
            data: vec![],
            pts: 0,
        }];
        assert_eq!(muxer.segment_audio_offset_90k(&video, &audio), 36_000);
        assert_eq!(
            muxer
                .with_preserved_audio_lead(true)
                .segment_audio_offset_90k(&video, &audio),
            0
        );
    }

    #[test]
    fn mux_ac4_private_pes_preserves_syncframes_and_pts() {
        let clock = ClockState::new(0, 90_000, 90_000);
        let mut muxer = TsMuxer::new(
            &ParamSets::default(),
            VideoCodec::H264,
            None,
            Some(AudioCodec::Ac4),
            48_000,
            clock,
        );
        // Synthetic transport frames, not decoder fixtures: one spans several TS packets.
        let mut large = vec![0xac, 0x40, 1, 0];
        large.extend(0..=255u8);
        let audio = [
            AudioUnit {
                data: large,
                pts: 0,
            },
            AudioUnit {
                data: vec![0xac, 0x40, 0, 2, 0x88, 0x33],
                pts: 1024,
            },
        ];
        let ts = muxer.mux_segment(&[], &audio);
        assert_eq!(ts.len() % TS_PACKET_SIZE, 0);
        let mut packets: Vec<Vec<u8>> = Vec::new();
        for ts_packet in ts.chunks_exact(TS_PACKET_SIZE) {
            assert_eq!(ts_packet[0], SYNC_BYTE);
            let pid = u16::from_be_bytes([ts_packet[1] & 0x1f, ts_packet[2]]);
            if pid != PID_AUDIO {
                continue;
            }
            if ts_packet[1] & 0x40 != 0 {
                packets.push(Vec::new());
            }
            let offset = if ts_packet[3] & 0x20 != 0 {
                5 + usize::from(ts_packet[4])
            } else {
                4
            };
            packets
                .last_mut()
                .unwrap()
                .extend_from_slice(&ts_packet[offset..]);
        }
        assert_eq!(packets.len(), 2);
        for (i, pes) in packets.iter().enumerate() {
            assert_eq!(&pes[..4], &[0, 0, 1, 0xbd]);
            assert_eq!(
                usize::from(u16::from_be_bytes([pes[4], pes[5]])),
                pes.len() - 6
            );
            assert_eq!((pes[6], pes[7], pes[8]), (0x84, 0x80, 5)); // aligned PTS-only
            let pts = (u64::from(pes[9] & 0x0e) << 29)
                | (u64::from(pes[10]) << 22)
                | (u64::from(pes[11] & 0xfe) << 14)
                | (u64::from(pes[12]) << 7)
                | u64::from(pes[13] >> 1);
            assert_eq!(pts, [90_000, 91_920][i]);
            assert_eq!(&pes[14..], &audio[i].data);
        }
    }
}
