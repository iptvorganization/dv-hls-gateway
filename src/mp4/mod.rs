//! 手写 mp4/ISO-BMFF box 解析，只解析本项目需要的 box：
//! init 段：`hvcC`(VPS/SPS/PPS)、`dvcC`(DV配置)、`tenc`(KID/IV策略)
//! media 段：`moof`→`traf`→{`tfhd`,`trun`,`saiz`,`saio`,`senc`}、`mdat`

pub mod aac;
pub mod avcc;
pub mod boxes;
pub mod dvcc;
pub mod hvcc;
pub mod mdhd;
pub mod sample;
pub mod tenc;

pub use dvcc::DoviConfig;
pub use hvcc::ParamSets;
pub use tenc::TrackEncryption;
