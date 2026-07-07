//! DV-HLS-Gateway: 把 MPD/M3U8 输入实时转成 HLS-TS。
//!
//! 全程只做必要处理 + 容器转换（零转码），DV RPU (NAL type 62) 原样透传。

pub mod clock;
pub mod config;
pub mod crypto;
pub mod fetch;
pub mod hevc;
pub mod hls;
pub mod mp4;
pub mod mpd;
pub mod segment;
pub mod server;
pub mod subtitle;
pub mod task;
pub mod ts;

/// 全项目通用结果类型。
pub type Result<T> = anyhow::Result<T>;
