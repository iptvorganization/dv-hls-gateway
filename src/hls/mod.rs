//! HLS playlist parsing for AES-128 TS inputs.

pub mod parser;

pub use parser::{
    looks_like_hls, parse_playlist, HlsMaster, HlsMediaPlaylist, HlsPlaylist, HlsRendition,
    HlsSegment, HlsSegmentKey, HlsVariant,
};
