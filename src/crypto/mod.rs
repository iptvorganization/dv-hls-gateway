//! CENC (AES-128-CTR) 解密。
//!
//! - 视频：subsample 加密——每样本只解密 encrypted 段，clear 段（含 NAL 头）原样保留。
//!   counter = IV(补到16字节) ，跨 subsample 连续递增（按 16 字节块）。
//! - 音频：整样本加密，IV 来自 per-sample senc 或 constant IV（tenc）。
//!
//! 用 `aes` + `ctr` crate，启用 AES-NI（aarch64 上是 ARMv8 Crypto Extensions）。

pub mod cenc;
pub mod hls;
pub mod playready;

pub use cenc::Decryptor;
