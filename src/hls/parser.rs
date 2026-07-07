//! Minimal HLS m3u8 parser: master variants, media segments, and AES-128 keys.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum HlsPlaylist {
    Master(HlsMaster),
    Media(HlsMediaPlaylist),
}

#[derive(Debug, Clone)]
pub struct HlsMaster {
    pub variants: Vec<HlsVariant>,
    pub audio: Vec<HlsRendition>,
    pub subtitles: Vec<HlsRendition>,
}

#[derive(Debug, Clone)]
pub struct HlsVariant {
    pub id: String,
    pub uri: String,
    pub audio_group: Option<String>,
    pub subtitles_group: Option<String>,
    pub bandwidth: u64,
    pub codecs: String,
    pub width: u32,
    pub height: u32,
    pub video_range: String,
}

#[derive(Debug, Clone)]
pub struct HlsRendition {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub uri: String,
    pub language: String,
    pub channels: String,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct HlsMediaPlaylist {
    pub uri: String,
    pub target_duration: u64,
    pub media_sequence: u64,
    pub end_list: bool,
    pub has_map: bool,
    pub map_uri: Option<String>,
    pub segments: Vec<HlsSegment>,
}

impl HlsMediaPlaylist {
    pub fn is_live(&self) -> bool {
        !self.end_list
    }
}

#[derive(Debug, Clone)]
pub struct HlsSegment {
    pub sequence: u64,
    pub uri: String,
    pub duration: f64,
    pub key: HlsSegmentKey,
    pub discontinuity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsSegmentKey {
    None,
    Aes128 {
        uri: String,
        iv: Option<[u8; 16]>,
    },
    SampleAesCtr {
        key_format: String,
        uri: Option<String>,
    },
    SampleAes {
        key_format: String,
        uri: Option<String>,
        iv: Option<[u8; 16]>,
    },
    Unsupported {
        method: String,
        key_format: String,
    },
}

pub fn looks_like_hls(text: &str) -> bool {
    text.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with("#EXTM3U")
}

pub fn parse_playlist(content: &str, playlist_url: &str) -> crate::Result<HlsPlaylist> {
    if !looks_like_hls(content) {
        return Err(anyhow::anyhow!("not an HLS playlist"));
    }

    let mut variants = Vec::new();
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut pending_stream_inf: Option<HashMap<String, String>> = None;
    let mut has_stream_inf = false;

    let mut target_duration = 6u64;
    let mut media_sequence = 0u64;
    let mut next_sequence = 0u64;
    let mut end_list = false;
    let mut has_map = false;
    let mut map_uri = None;
    let mut pending_extinf: Option<f64> = None;
    let mut current_key = HlsSegmentKey::None;
    let mut pending_discontinuity = false;
    let mut segments = Vec::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(attrs) = pending_stream_inf.take() {
            if line.starts_with('#') {
                pending_stream_inf = Some(attrs);
                continue;
            }
            let id = format!("hls-{}", variants.len());
            variants.push(build_variant(id, attrs, playlist_url, line)?);
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            has_stream_inf = true;
            pending_stream_inf = Some(parse_attr_list(rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attr_list(rest);
            if attrs.get("TYPE").map(|v| v == "AUDIO").unwrap_or(false) {
                if let (Some(group_id), Some(uri)) = (attrs.get("GROUP-ID"), attrs.get("URI")) {
                    let id = format!("hls-a-{}", audio.len());
                    audio.push(HlsRendition {
                        id,
                        group_id: group_id.clone(),
                        name: attrs.get("NAME").cloned().unwrap_or_default(),
                        uri: resolve_url(playlist_url, uri)?,
                        language: attrs.get("LANGUAGE").cloned().unwrap_or_default(),
                        channels: attrs.get("CHANNELS").cloned().unwrap_or_default(),
                        is_default: attrs.get("DEFAULT").map(|v| v == "YES").unwrap_or(false),
                    });
                }
            } else if attrs.get("TYPE").map(|v| v == "SUBTITLES").unwrap_or(false) {
                if let (Some(group_id), Some(uri)) = (attrs.get("GROUP-ID"), attrs.get("URI")) {
                    let id = format!("hls-s-{}", subtitles.len());
                    subtitles.push(HlsRendition {
                        id,
                        group_id: group_id.clone(),
                        name: attrs.get("NAME").cloned().unwrap_or_default(),
                        uri: resolve_url(playlist_url, uri)?,
                        language: attrs.get("LANGUAGE").cloned().unwrap_or_default(),
                        channels: String::new(),
                        is_default: attrs.get("DEFAULT").map(|v| v == "YES").unwrap_or(false),
                    });
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse().unwrap_or(target_duration);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest.trim().parse().unwrap_or(0);
            next_sequence = media_sequence;
            continue;
        }
        if line.starts_with("#EXT-X-ENDLIST") {
            end_list = true;
            continue;
        }
        if line.starts_with("#EXT-X-DISCONTINUITY") {
            pending_discontinuity = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            has_map = true;
            let attrs = parse_attr_list(rest);
            if let Some(uri) = attrs.get("URI") {
                map_uri = Some(resolve_url(playlist_url, uri)?);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            current_key = parse_key(rest, playlist_url)?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0);
            pending_extinf = Some(dur);
            continue;
        }

        if line.starts_with('#') {
            continue;
        }

        if let Some(duration) = pending_extinf.take() {
            segments.push(HlsSegment {
                sequence: next_sequence,
                uri: resolve_url(playlist_url, line)?,
                duration,
                key: current_key.clone(),
                discontinuity: std::mem::take(&mut pending_discontinuity),
            });
            next_sequence += 1;
        }
    }

    if has_stream_inf {
        Ok(HlsPlaylist::Master(HlsMaster {
            variants,
            audio,
            subtitles,
        }))
    } else {
        Ok(HlsPlaylist::Media(HlsMediaPlaylist {
            uri: playlist_url.to_string(),
            target_duration,
            media_sequence,
            end_list,
            has_map,
            map_uri,
            segments,
        }))
    }
}

fn build_variant(
    id: String,
    attrs: HashMap<String, String>,
    playlist_url: &str,
    uri: &str,
) -> crate::Result<HlsVariant> {
    let (width, height) = attrs
        .get("RESOLUTION")
        .and_then(|r| r.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((0, 0));

    Ok(HlsVariant {
        id,
        uri: resolve_url(playlist_url, uri)?,
        audio_group: attrs.get("AUDIO").cloned(),
        subtitles_group: attrs.get("SUBTITLES").cloned(),
        bandwidth: attrs
            .get("BANDWIDTH")
            .or_else(|| attrs.get("AVERAGE-BANDWIDTH"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        codecs: attrs.get("CODECS").cloned().unwrap_or_default(),
        width,
        height,
        video_range: attrs
            .get("VIDEO-RANGE")
            .cloned()
            .unwrap_or_else(|| "SDR".to_string()),
    })
}

fn parse_key(rest: &str, playlist_url: &str) -> crate::Result<HlsSegmentKey> {
    let attrs = parse_attr_list(rest);
    let method = attrs
        .get("METHOD")
        .map(|s| s.trim().to_ascii_uppercase())
        .unwrap_or_else(|| "NONE".to_string());
    if method == "NONE" {
        return Ok(HlsSegmentKey::None);
    }

    let key_format = attrs
        .get("KEYFORMAT")
        .cloned()
        .unwrap_or_else(|| "identity".to_string());
    if method == "SAMPLE-AES-CTR" {
        let uri = attrs
            .get("URI")
            .map(|u| resolve_key_uri(playlist_url, u))
            .transpose()?;
        return Ok(HlsSegmentKey::SampleAesCtr { key_format, uri });
    }
    if method == "SAMPLE-AES" {
        let iv = attrs.get("IV").map(|v| parse_iv(v)).transpose()?;
        let uri = attrs
            .get("URI")
            .map(|u| resolve_key_uri(playlist_url, u))
            .transpose()?;
        return Ok(HlsSegmentKey::SampleAes {
            key_format,
            uri,
            iv,
        });
    }
    if method != "AES-128" || key_format != "identity" {
        return Ok(HlsSegmentKey::Unsupported { method, key_format });
    }

    let uri = attrs
        .get("URI")
        .ok_or_else(|| anyhow::anyhow!("HLS AES-128 key is missing URI"))?;
    let iv = attrs.get("IV").map(|v| parse_iv(v)).transpose()?;
    Ok(HlsSegmentKey::Aes128 {
        uri: resolve_url(playlist_url, uri)?,
        iv,
    })
}

fn parse_attr_list(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut start = 0usize;
    let mut in_quote = false;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quote = !in_quote,
            b',' if !in_quote => {
                parse_attr_pair(&s[start..i], &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    parse_attr_pair(&s[start..], &mut out);
    out
}

fn parse_attr_pair(pair: &str, out: &mut HashMap<String, String>) {
    let Some((k, v)) = pair.split_once('=') else {
        return;
    };
    let key = k.trim().to_ascii_uppercase();
    let mut val = v.trim().to_string();
    if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
        val = val[1..val.len() - 1].to_string();
    }
    out.insert(key, val);
}

fn parse_iv(s: &str) -> crate::Result<[u8; 16]> {
    let hex = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() % 2 != 0 {
        return Err(anyhow::anyhow!("invalid HLS IV hex length"));
    }
    let bytes = hex::decode(hex).map_err(|e| anyhow::anyhow!("invalid HLS IV: {e}"))?;
    if bytes.len() > 16 {
        return Err(anyhow::anyhow!("HLS IV is longer than 16 bytes"));
    }
    let mut iv = [0u8; 16];
    iv[16 - bytes.len()..].copy_from_slice(&bytes);
    Ok(iv)
}

fn resolve_url(base: &str, path: &str) -> crate::Result<String> {
    if path.starts_with("http://") || path.starts_with("https://") {
        return Ok(path.to_string());
    }
    let base = url::Url::parse(base).map_err(|e| anyhow::anyhow!("invalid HLS URL {base}: {e}"))?;
    Ok(base
        .join(path)
        .map_err(|e| anyhow::anyhow!("invalid HLS relative URL {path}: {e}"))?
        .to_string())
}

fn resolve_key_uri(base: &str, uri: &str) -> crate::Result<String> {
    if uri.starts_with("data:") || uri.contains("://") {
        return Ok(uri.to_string());
    }
    resolve_url(base, uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_master_variants() {
        let text = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.64001f,mp4a.40.2"
v/720/main.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5500000,RESOLUTION=1920x1080,VIDEO-RANGE=PQ,CODECS="hvc1.2.4.L153.B0,ec-3"
/live/1080.m3u8
"#;
        let HlsPlaylist::Master(m) = parse_playlist(text, "https://cdn/a/master.m3u8").unwrap()
        else {
            panic!("master expected");
        };
        assert_eq!(m.variants.len(), 2);
        assert_eq!(m.variants[0].id, "hls-0");
        assert_eq!(m.variants[0].uri, "https://cdn/a/v/720/main.m3u8");
        assert_eq!(m.variants[1].uri, "https://cdn/live/1080.m3u8");
        assert_eq!(m.variants[1].video_range, "PQ");
    }

    #[test]
    fn parses_media_aes128_key_and_segments() {
        let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:42
#EXT-X-KEY:METHOD=AES-128,URI="keys/k.bin",IV=0x0000000000000000000000000000abcd
#EXTINF:5.944,
seg42.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:6.0,
seg43.ts
#EXT-X-ENDLIST
"#;
        let HlsPlaylist::Media(m) = parse_playlist(text, "https://cdn/live/index.m3u8").unwrap()
        else {
            panic!("media expected");
        };
        assert_eq!(m.target_duration, 6);
        assert_eq!(m.media_sequence, 42);
        assert!(!m.is_live());
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.segments[0].sequence, 42);
        assert_eq!(m.segments[0].uri, "https://cdn/live/seg42.ts");
        assert_eq!(m.segments[1].key, HlsSegmentKey::None);
        match &m.segments[0].key {
            HlsSegmentKey::Aes128 { uri, iv } => {
                assert_eq!(uri, "https://cdn/live/keys/k.bin");
                assert_eq!(iv.unwrap()[14..], [0xab, 0xcd]);
            }
            other => panic!("bad key: {other:?}"),
        }
    }

    #[test]
    fn unsupported_keyformat_is_recorded() {
        let text = r#"#EXTM3U
#EXT-X-KEY:METHOD=SAMPLE-AES,KEYFORMAT="com.apple.streamingkeydelivery",URI="skd://x"
#EXTINF:4,
s.ts
"#;
        let HlsPlaylist::Media(m) = parse_playlist(text, "https://cdn/p.m3u8").unwrap() else {
            panic!("media expected");
        };
        assert!(matches!(m.segments[0].key, HlsSegmentKey::SampleAes { .. }));
    }

    #[test]
    fn parses_audio_renditions_and_fmp4_map() {
        let master = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="eac3",NAME="English",LANGUAGE="en",CHANNELS="6",DEFAULT=YES,URI="a/224.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000,CODECS="hvc1,ec-3",AUDIO="eac3"
v/hi.m3u8
"#;
        let HlsPlaylist::Master(m) = parse_playlist(master, "https://cdn/master.m3u8").unwrap()
        else {
            panic!("master expected");
        };
        assert_eq!(m.audio.len(), 1);
        assert_eq!(m.audio[0].uri, "https://cdn/a/224.m3u8");
        assert_eq!(m.variants[0].audio_group.as_deref(), Some("eac3"));

        let media = r#"#EXTM3U
#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,KEYFORMAT="com.microsoft.playready",URI="data:text/plain;base64,x"
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2.0,
0001.m4s
"#;
        let HlsPlaylist::Media(m) = parse_playlist(media, "https://cdn/v/hi.m3u8").unwrap() else {
            panic!("media expected");
        };
        assert_eq!(m.map_uri.as_deref(), Some("https://cdn/v/init.mp4"));
        match &m.segments[0].key {
            HlsSegmentKey::SampleAesCtr { uri, key_format } => {
                assert_eq!(key_format, "com.microsoft.playready");
                assert_eq!(uri.as_deref(), Some("data:text/plain;base64,x"));
            }
            other => panic!("bad key: {other:?}"),
        }
    }

    #[test]
    fn marks_segment_after_discontinuity() {
        let media = r#"#EXTM3U
#EXT-X-MEDIA-SEQUENCE:7
#EXTINF:2.0,
0007.m4s
#EXT-X-DISCONTINUITY
#EXTINF:2.0,
0008.m4s
"#;
        let HlsPlaylist::Media(m) = parse_playlist(media, "https://cdn/v/main.m3u8").unwrap()
        else {
            panic!("media expected");
        };
        assert!(!m.segments[0].discontinuity);
        assert!(m.segments[1].discontinuity);
    }
}
