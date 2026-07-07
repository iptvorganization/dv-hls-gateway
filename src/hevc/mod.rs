//! 视频比特流处理：HEVC/H.264 NAL 类型识别、mp4 length-prefix ↔ Annex-B 转换、AU 组装。

pub mod annexb;
pub mod h264;
pub mod nal;
pub mod sps;
