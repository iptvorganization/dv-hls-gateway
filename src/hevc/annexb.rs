//! mp4 (length-prefixed) NALU → Annex-B (start-code) 转换与 AU 组装。
//! 同时支持 HEVC（含 DV RPU 透传）和 H.264。
//!
//! 关键规则（实测校准）：
//! - 每个 NAL 前加 4 字节起始码 `00 00 00 01`。
//! - IRAP/IDR 帧在 AUD 之后注入参数集（HEVC: VPS/SPS/PPS；H264: SPS/PPS），保证段可独立解码。
//! - DV RPU (HEVC NAL 62) 原样透传，不做任何 RBSP 处理。
//! - 输入样本已含 AUD 则不重复添加。

use super::{h264, nal};
use crate::ts::VideoCodec;

const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// 取某 codec 的 NAL 类型。
#[inline]
fn nal_type(codec: VideoCodec, first_byte: u8) -> u8 {
    match codec {
        VideoCodec::Hevc => nal::nal_type(first_byte),
        VideoCodec::H264 => h264::nal_type(first_byte),
    }
}

#[inline]
fn is_irap(codec: VideoCodec, t: u8) -> bool {
    match codec {
        VideoCodec::Hevc => nal::is_irap(t),
        VideoCodec::H264 => h264::is_irap(t),
    }
}

#[inline]
fn is_aud(codec: VideoCodec, t: u8) -> bool {
    match codec {
        VideoCodec::Hevc => t == nal::AUD,  // 35
        VideoCodec::H264 => t == h264::AUD, // 9
    }
}

/// 解析后的一个访问单元（一帧）。
#[derive(Debug, Clone)]
pub struct AccessUnit {
    pub nals: Vec<Vec<u8>>,
    pub is_irap: bool,
    pub dts: u64,
    pub cts_offset: i64,
    pub codec: VideoCodec,
}

/// 把 mp4 样本数据（4字节大端长度前缀 + NAL，重复）拆成裸 NAL 列表。
pub fn split_length_prefixed(sample: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= sample.len() {
        let len =
            u32::from_be_bytes([sample[p], sample[p + 1], sample[p + 2], sample[p + 3]]) as usize;
        p += 4;
        if len == 0 || p + len > sample.len() {
            break;
        }
        out.push(&sample[p..p + len]);
        p += len;
    }
    out
}

impl AccessUnit {
    pub fn from_sample(sample: &[u8], dts: u64, cts_offset: i64, codec: VideoCodec) -> Self {
        let nals: Vec<Vec<u8>> = split_length_prefixed(sample)
            .into_iter()
            .map(|n| n.to_vec())
            .collect();
        let is_irap = nals
            .iter()
            .any(|n| !n.is_empty() && is_irap(codec, nal_type(codec, n[0])));
        Self {
            nals,
            is_irap,
            dts,
            cts_offset,
            codec,
        }
    }

    /// 写成 Annex-B 字节流追加到 `out`；`param_sets` 仅 IRAP 帧注入。
    pub fn write_annexb(&self, out: &mut Vec<u8>, param_sets: &[Vec<u8>]) {
        let codec = self.codec;
        let mut injected = false;
        let mut first_is_aud = false;
        if let Some(first) = self.nals.iter().find(|n| !n.is_empty()) {
            first_is_aud = is_aud(codec, nal_type(codec, first[0]));
        }
        for (i, nalu) in self.nals.iter().enumerate() {
            if nalu.is_empty() {
                continue;
            }
            let t = nal_type(codec, nalu[0]);

            // 注入点：若首 NAL 是 AUD，则在它之后；否则在第一个 NAL 之前。
            if self.is_irap && !injected && !(first_is_aud && i == 0) {
                for ps in param_sets {
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(ps);
                }
                injected = true;
            }

            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nalu);
            let _ = t;
        }
        // 兜底：极端情况下仍未注入
        if self.is_irap && !injected && !param_sets.is_empty() {
            let mut prefix = Vec::new();
            for ps in param_sets {
                prefix.extend_from_slice(&START_CODE);
                prefix.extend_from_slice(ps);
            }
            prefix.extend_from_slice(out);
            *out = prefix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hevc_nalu(t: u8, payload: &[u8]) -> Vec<u8> {
        let hdr0 = (t << 1) & 0x7E;
        let mut v = vec![hdr0, 0x01];
        v.extend_from_slice(payload);
        v
    }

    fn length_prefixed(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut s = Vec::new();
        for n in nals {
            s.extend_from_slice(&(n.len() as u32).to_be_bytes());
            s.extend_from_slice(n);
        }
        s
    }

    #[test]
    fn split_roundtrip() {
        let nals = vec![
            hevc_nalu(nal::AUD, &[0x10]),
            hevc_nalu(nal::TRAIL_R, &[1, 2, 3]),
        ];
        let sample = length_prefixed(&nals);
        let got = split_length_prefixed(&sample);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], nals[0].as_slice());
    }

    #[test]
    fn hevc_irap_injects_after_aud() {
        let aud = hevc_nalu(nal::AUD, &[0x10]);
        let idr = hevc_nalu(nal::IDR_N_LP, &[0xaa]);
        let rpu = hevc_nalu(nal::DV_RPU, &[0xbb; 4]);
        let sample = length_prefixed(&[aud.clone(), idr.clone(), rpu.clone()]);
        let au = AccessUnit::from_sample(&sample, 0, 0, VideoCodec::Hevc);
        assert!(au.is_irap);
        let vps = hevc_nalu(nal::VPS, &[1]);
        let sps = hevc_nalu(nal::SPS, &[2]);
        let pps = hevc_nalu(nal::PPS, &[3]);
        let mut out = Vec::new();
        au.write_annexb(&mut out, &[vps.clone(), sps.clone(), pps.clone()]);
        let sc = &START_CODE[..];
        let mut expect = Vec::new();
        for n in [&aud, &vps, &sps, &pps, &idr, &rpu] {
            expect.extend_from_slice(sc);
            expect.extend_from_slice(n);
        }
        assert_eq!(out, expect);
    }

    #[test]
    fn rpu_passthrough_untouched() {
        let rpu_payload = [0x00, 0x00, 0x01, 0xff];
        let rpu = hevc_nalu(nal::DV_RPU, &rpu_payload);
        let sample = length_prefixed(&[hevc_nalu(nal::TRAIL_R, &[9]), rpu.clone()]);
        let au = AccessUnit::from_sample(&sample, 0, 0, VideoCodec::Hevc);
        let mut out = Vec::new();
        au.write_annexb(&mut out, &[]);
        let needle = rpu.as_slice();
        assert!(out.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn h264_irap_injection() {
        // H264: AUD(9) + IDR(5)
        let aud = vec![0x09u8, 0x10];
        let idr = vec![0x65u8, 0xaa]; // type 5
        let sample = length_prefixed(&[aud.clone(), idr.clone()]);
        let au = AccessUnit::from_sample(&sample, 0, 0, VideoCodec::H264);
        assert!(au.is_irap);
        let sps = vec![0x67u8, 0x42];
        let pps = vec![0x68u8, 0xce];
        let mut out = Vec::new();
        au.write_annexb(&mut out, &[sps.clone(), pps.clone()]);
        // 期望: AUD, SPS, PPS, IDR
        let sc = &START_CODE[..];
        let mut expect = Vec::new();
        for n in [&aud, &sps, &pps, &idr] {
            expect.extend_from_slice(sc);
            expect.extend_from_slice(n);
        }
        assert_eq!(out, expect);
    }
}
