//! MPEG-TS muxer：把已解密的 HEVC(DV) + AC-3/EC-3/AAC 访问单元封装成 188 字节 TS 包。

pub mod bitwriter;
pub mod crc32;
pub mod descriptor;
pub mod muxer;
pub mod packet;
pub mod pat;
pub mod pes;
pub mod pmt;

// ── TS 全局常量（计划已定）─────────────────────────────────
pub const TS_PACKET_SIZE: usize = 188;
pub const SYNC_BYTE: u8 = 0x47;

pub const PID_PAT: u16 = 0x0000;
pub const PID_PMT: u16 = 0x1000;
pub const PID_VIDEO: u16 = 0x0100; // 同时是 PCR_PID
pub const PID_AUDIO: u16 = 0x0101;
pub const PID_NULL: u16 = 0x1FFF;

// stream_type
pub const STREAM_TYPE_HEVC: u8 = 0x24;
pub const STREAM_TYPE_H264: u8 = 0x1B;
pub const STREAM_TYPE_AC3: u8 = 0x81;
pub const STREAM_TYPE_EC3: u8 = 0x87;
pub const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;

// PES stream_id
pub const STREAM_ID_VIDEO: u8 = 0xE0;
pub const STREAM_ID_AUDIO: u8 = 0xC0;

/// 视频编码类型（决定 PMT stream_type 与 NAL 处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Hevc,
    H264,
}

impl VideoCodec {
    pub fn stream_type(self) -> u8 {
        match self {
            VideoCodec::Hevc => STREAM_TYPE_HEVC,
            VideoCodec::H264 => STREAM_TYPE_H264,
        }
    }
    /// 从 MPD codecs 字符串判定。
    pub fn from_codecs(codecs: &str) -> Self {
        let c = codecs.to_lowercase();
        if c.starts_with("avc") || c.starts_with("h264") {
            VideoCodec::H264
        } else {
            VideoCodec::Hevc // dvh1/dvhe/hvc1/hev1
        }
    }
}

/// 视频动态范围/色域（用于 HLS m3u8 的 VIDEO-RANGE）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRange {
    Sdr,
    /// HDR10 / DV (PQ, SMPTE ST 2084)
    Pq,
    /// HLG (ARIB STD-B67)
    Hlg,
}

impl VideoRange {
    /// HLS #EXT-X-STREAM-INF 的 VIDEO-RANGE 值。
    pub fn hls_str(self) -> &'static str {
        match self {
            VideoRange::Sdr => "SDR",
            VideoRange::Pq => "PQ",
            VideoRange::Hlg => "HLG",
        }
    }
}

/// 音频编码类型（决定 PMT stream_type）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Ac3,
    Ec3,
    AacAdts,
}

impl AudioCodec {
    pub fn stream_type(self) -> u8 {
        match self {
            AudioCodec::Ac3 => STREAM_TYPE_AC3,
            AudioCodec::Ec3 => STREAM_TYPE_EC3,
            AudioCodec::AacAdts => STREAM_TYPE_AAC_ADTS,
        }
    }
}
