//! PMT (Program Map Table) section 构造，含 视频 + 音频 ES，DV 时视频挂 DOVI descriptor。

use super::descriptor::dolby_vision_descriptor;
use super::pat::finish_section;
use super::{AudioCodec, VideoCodec, PID_VIDEO};
use crate::mp4::DoviConfig;

/// 构造 PMT section。
/// - video_pid 同时是 PCR_PID。
/// - video_codec 决定 stream_type（HEVC 0x24 / H264 0x1B）。
/// - dovi: 若 Some（DV5/DV8），则在视频 ES 上挂 DOVI descriptor。
/// - audio: (pid, codec)；None 表示无音频。
pub fn build_pmt_section(
    video_pid: u16,
    video_codec: VideoCodec,
    audio: Option<(u16, AudioCodec)>,
    dovi: Option<&DoviConfig>,
) -> Vec<u8> {
    let mut s = Vec::new();
    s.push(0x02); // table_id = PMT
    s.push(0xB0); // section_syntax_indicator=1, 占位
    s.push(0x00); // section_length 低位（回填）
    s.extend_from_slice(&0x0001u16.to_be_bytes()); // program_number
    s.push(0xC1); // reserved + version0 + current_next=1
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    s.extend_from_slice(&(0xE000 | (video_pid & 0x1FFF)).to_be_bytes()); // reserved=111 + PCR_PID
    s.extend_from_slice(&0xF000u16.to_be_bytes()); // reserved=1111 + program_info_length=0

    // ── ES loop 1: 视频 ──
    let mut video_desc = Vec::new();
    if let Some(cfg) = dovi {
        // DV (profile 5/8) 才挂 DOVI descriptor；SDR/HDR10/HLG 不挂（信息在码流 VUI/SEI）。
        video_desc.extend_from_slice(&dolby_vision_descriptor(cfg));
    }
    push_es(&mut s, video_codec.stream_type(), video_pid, &video_desc);

    // ── ES loop 2: audio ──
    if let Some((apid, codec)) = audio {
        push_es(&mut s, codec.stream_type(), apid, &[]);
    }

    finish_section(&mut s);
    s
}

fn push_es(s: &mut Vec<u8>, stream_type: u8, pid: u16, descriptors: &[u8]) {
    s.push(stream_type);
    s.extend_from_slice(&(0xE000 | (pid & 0x1FFF)).to_be_bytes()); // reserved=111 + elementary_PID
    let es_info_len = descriptors.len() as u16;
    s.extend_from_slice(&(0xF000 | (es_info_len & 0x0FFF)).to_be_bytes()); // reserved=1111 + ES_info_length
    s.extend_from_slice(descriptors);
}

/// 默认视频 PCR PID。
pub const DEFAULT_PCR_PID: u16 = PID_VIDEO;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts::crc32::mpeg_crc32;
    use crate::ts::{PID_AUDIO, PID_VIDEO};

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
    fn pmt_contains_dovi_and_crc_ok() {
        let pmt = build_pmt_section(
            PID_VIDEO,
            VideoCodec::Hevc,
            Some((PID_AUDIO, AudioCodec::Ec3)),
            Some(&cfg_p5()),
        );
        assert_eq!(pmt[0], 0x02);
        // 含 DOVI descriptor 字节序列
        let needle = [0xB0u8, 0x05, 0x01, 0x00, 0x0A, 0x35, 0x00];
        assert!(
            pmt.windows(needle.len()).any(|w| w == needle),
            "DOVI desc missing"
        );
        // 含 HEVC stream_type 0x24 和 EC-3 0x87
        assert!(pmt.contains(&0x24));
        assert!(pmt.contains(&0x87));
        // CRC 自洽
        let crc = mpeg_crc32(&pmt[..pmt.len() - 4]);
        let stored = u32::from_be_bytes([
            pmt[pmt.len() - 4],
            pmt[pmt.len() - 3],
            pmt[pmt.len() - 2],
            pmt[pmt.len() - 1],
        ]);
        assert_eq!(crc, stored);
    }

    #[test]
    fn pmt_audio_stream_type_follows_codec() {
        let cases = [
            (AudioCodec::Ac3, 0x81),
            (AudioCodec::Ec3, 0x87),
            (AudioCodec::AacAdts, 0x0F),
        ];

        for (codec, expected_stream_type) in cases {
            let pmt =
                build_pmt_section(PID_VIDEO, VideoCodec::Hevc, Some((PID_AUDIO, codec)), None);
            let types = es_stream_types(&pmt);
            assert_eq!(types, vec![0x24, expected_stream_type]);
        }
    }

    fn es_stream_types(pmt: &[u8]) -> Vec<u8> {
        let section_len = (((pmt[1] & 0x0f) as usize) << 8) | pmt[2] as usize;
        let section_end = 3 + section_len - 4;
        let program_info_len = (((pmt[10] & 0x0f) as usize) << 8) | pmt[11] as usize;
        let mut pos = 12 + program_info_len;
        let mut types = Vec::new();
        while pos + 5 <= section_end {
            types.push(pmt[pos]);
            let es_info_len = (((pmt[pos + 3] & 0x0f) as usize) << 8) | pmt[pos + 4] as usize;
            pos += 5 + es_info_len;
        }
        types
    }
}
