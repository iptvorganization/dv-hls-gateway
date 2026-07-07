//! MPD (DASH) 解析与时间线展开。

pub mod parser;

pub use parser::{parse_mpd, MpdInfo, Representation, TrackKind};
