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
    pub has_session_key: bool,
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
    /// Empty for an audio rendition carried inside the variant's media.
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
    /// Any non-NONE EXT-X-KEY, including keys protecting init maps.
    pub has_encryption: bool,
    /// The current pipeline only keeps one init map per playlist.
    pub has_multiple_maps: bool,
    /// Whole-resource downloads cannot represent byte-range segments/maps.
    pub has_byte_ranges: bool,
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
    /// Timeline epoch signaled by the Media Playlist.
    pub discontinuity_sequence: u64,
    /// Start in Unix seconds, from PROGRAM-DATE-TIME or inferred within its epoch.
    pub program_date_time: Option<f64>,
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
    let mut has_session_key = false;

    let mut target_duration = 6u64;
    let mut media_sequence = 0u64;
    let mut next_sequence = 0u64;
    let mut end_list = false;
    let mut has_map = false;
    let mut map_uri = None;
    let mut last_map_attrs = None;
    let mut has_multiple_maps = false;
    let mut has_byte_ranges = false;
    let mut has_encryption = false;
    let mut pending_extinf: Option<f64> = None;
    let mut current_key = HlsSegmentKey::None;
    let mut pending_discontinuity = false;
    let mut discontinuity_sequence = 0u64;
    let mut pending_program_date_time = None;
    let mut segments = Vec::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with("#EXT-X-SESSION-KEY:") {
            has_session_key = true;
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
                if let Some(group_id) = attrs.get("GROUP-ID") {
                    let id = format!("hls-a-{}", audio.len());
                    audio.push(HlsRendition {
                        id,
                        group_id: group_id.clone(),
                        name: attrs.get("NAME").cloned().unwrap_or_default(),
                        uri: attrs
                            .get("URI")
                            .map(|uri| resolve_url(playlist_url, uri))
                            .transpose()?
                            .unwrap_or_default(),
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
            anyhow::ensure!(
                segments.is_empty(),
                "HLS media sequence must precede segments"
            );
            media_sequence = rest
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid HLS media sequence"))?;
            next_sequence = media_sequence;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            anyhow::ensure!(
                segments.is_empty() && !pending_discontinuity,
                "HLS discontinuity sequence must precede discontinuities and segments"
            );
            discontinuity_sequence = rest.trim().parse()?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pending_program_date_time = parse_program_date_time(rest.trim());
            continue;
        }
        if line.starts_with("#EXT-X-ENDLIST") {
            end_list = true;
            continue;
        }
        if line == "#EXT-X-DISCONTINUITY" {
            discontinuity_sequence = discontinuity_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("HLS discontinuity sequence overflow"))?;
            pending_discontinuity = true;
            continue;
        }
        if line.starts_with("#EXT-X-BYTERANGE:") {
            has_byte_ranges = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            has_map = true;
            let attrs = parse_attr_list(rest);
            has_byte_ranges |= attrs.contains_key("BYTERANGE");
            if last_map_attrs.as_ref().is_some_and(|prev| prev != &attrs) {
                has_multiple_maps = true;
            }
            if let Some(uri) = attrs.get("URI") {
                map_uri = Some(resolve_url(playlist_url, uri)?);
            }
            last_map_attrs = Some(attrs);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            current_key = parse_key(rest, playlist_url)?;
            has_encryption |= current_key != HlsSegmentKey::None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .ok_or_else(|| {
                    anyhow::anyhow!("HLS EXTINF duration must be positive and finite")
                })?;
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
                discontinuity_sequence,
                program_date_time: pending_program_date_time.take(),
            });
            next_sequence = next_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("HLS media sequence cannot be advanced"))?;
        }
    }

    if has_stream_inf {
        Ok(HlsPlaylist::Master(HlsMaster {
            variants,
            audio,
            subtitles,
            has_session_key,
        }))
    } else {
        infer_program_dates(&mut segments);
        Ok(HlsPlaylist::Media(HlsMediaPlaylist {
            uri: playlist_url.to_string(),
            target_duration,
            media_sequence,
            end_list,
            has_map,
            map_uri,
            has_encryption: has_encryption || has_session_key,
            has_multiple_maps,
            has_byte_ranges,
            segments,
        }))
    }
}

fn parse_program_date_time(value: &str) -> Option<f64> {
    let (date, time) = value.split_once(['T', 't'])?;
    let (clock, zone_minutes) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let i = time.rfind(['+', '-'])?;
        let sign = if time.as_bytes()[i] == b'-' { -1 } else { 1 };
        let zone = &time[i + 1..];
        let (h, m) = zone
            .split_once(':')
            .or_else(|| (zone.len() == 4).then_some((zone.get(..2)?, zone.get(2..)?)))?;
        let h = decimal(h, 2)?;
        let m = decimal(m, 2)?;
        if h > 23 || m > 59 {
            return None;
        }
        (&time[..i], sign * (h * 60 + m))
    };
    let mut date = date.split('-');
    let year = decimal(date.next()?, 4)?;
    let month = decimal(date.next()?, 2)?;
    let day = decimal(date.next()?, 2)?;
    if date.next().is_some() || year == 0 || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_per_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=days_per_month[month as usize - 1]).contains(&day) {
        return None;
    }
    let before_year = |year: i64| {
        let y = year - 1;
        y * 365 + y / 4 - y / 100 + y / 400
    };
    let days = before_year(year) - before_year(1970)
        + days_per_month[..month as usize - 1].iter().sum::<i64>()
        + day
        - 1;
    let mut clock = clock.split(':');
    let h = decimal(clock.next()?, 2)?;
    let m = decimal(clock.next()?, 2)?;
    let seconds = clock.next()?;
    if clock.next().is_some() || h > 23 || m > 59 {
        return None;
    }
    let (sec, frac) = seconds.split_once('.').unwrap_or((seconds, ""));
    let sec = decimal(sec, 2)?;
    if sec > 60 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fraction = frac
        .bytes()
        .take(9)
        .enumerate()
        .map(|(i, digit)| f64::from(digit - b'0') * 10f64.powi(-1 - i as i32))
        .sum::<f64>();
    Some((days * 86_400 + h * 3600 + m * 60 + sec - zone_minutes * 60) as f64 + fraction)
}

fn decimal(s: &str, width: usize) -> Option<i64> {
    (s.len() == width && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn infer_program_dates(segments: &mut [HlsSegment]) {
    // Do not extrapolate across an explicit epoch break without a new
    // anchor: media PTS can jump arbitrarily there.
    for epoch in segments.chunk_by_mut(|a, b| a.discontinuity_sequence == b.discontinuity_sequence)
    {
        let Some(first) = epoch.iter().position(|seg| seg.program_date_time.is_some()) else {
            continue;
        };
        let mut next = epoch[first].program_date_time;
        for seg in epoch[..first].iter_mut().rev() {
            next = next.map(|ts| ts - seg.duration);
            seg.program_date_time = next;
        }
        let mut next = epoch[first].program_date_time;
        for seg in &mut epoch[first..] {
            seg.program_date_time = seg.program_date_time.or(next);
            next = seg.program_date_time.map(|ts| ts + seg.duration);
        }
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
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("HLS EXT-X-KEY is missing METHOD"))?;
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
    fn program_date_time_normalizes_zones_and_stays_within_epoch() {
        assert_eq!(
            parse_program_date_time("1970-01-01T08:00:00.125+08:00"),
            Some(0.125)
        );
        assert_eq!(
            parse_program_date_time("1969-12-31T19:00:00-0500"),
            Some(0.0)
        );
        assert!(parse_program_date_time("2023-02-29T00:00:00Z").is_none());
        assert!(parse_program_date_time("2024-02-29T00:00:00").is_none());
        assert_eq!(
            parse_program_date_time("2024-03-01T00:00:00Z").unwrap()
                - parse_program_date_time("2024-02-29T00:00:00Z").unwrap(),
            86_400.0
        );
        let text = concat!(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-DISCONTINUITY-SEQUENCE:8\n",
            "#EXTINF:2,\nfirst.m4s\n#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:00:10Z\n",
            "#EXTINF:3,\nsecond.m4s\n#EXTINF:4,\nthird.m4s\n",
            "#EXT-X-DISCONTINUITY\n#EXTINF:4,\nfourth.m4s\n",
        );
        let HlsPlaylist::Media(p) =
            parse_playlist(text, "https://example.invalid/live.m3u8").unwrap()
        else {
            panic!("expected a Media Playlist");
        };
        assert_eq!(
            p.segments
                .iter()
                .map(|seg| (seg.discontinuity_sequence, seg.program_date_time))
                .collect::<Vec<_>>(),
            [(8, Some(8.0)), (8, Some(10.0)), (8, Some(13.0)), (9, None)]
        );
    }

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
