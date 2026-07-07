use quick_xml::events::Event;
use quick_xml::Reader;

use crate::crypto::cenc::Decryptor;
use crate::mp4::boxes::iter_boxes;
use crate::mp4::sample::parse_media_segment_with_default_iv_size;

#[derive(Default)]
pub struct SubtitleAccumulator {
    body: String,
    duration: f64,
}

impl SubtitleAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append_empty(&mut self, duration: f64) {
        self.duration += duration.max(0.0);
    }

    pub fn append_fragment(
        &mut self,
        data: &[u8],
        duration: f64,
        timescale: u32,
        decryptor: Option<&Decryptor>,
        default_iv_size: Option<u8>,
    ) -> crate::Result<()> {
        let cues = fragment_to_vtt_body(
            data,
            self.duration,
            duration,
            timescale,
            decryptor,
            default_iv_size,
        )?;
        append_body(&mut self.body, &cues);
        self.duration += duration.max(0.0);
        Ok(())
    }

    pub fn take_body(&mut self) -> String {
        self.duration = 0.0;
        std::mem::take(&mut self.body)
    }

    pub fn clear(&mut self) {
        self.body.clear();
        self.duration = 0.0;
    }
}

pub fn fragment_to_vtt_body(
    data: &[u8],
    offset_secs: f64,
    default_duration_secs: f64,
    timescale: u32,
    decryptor: Option<&Decryptor>,
    default_iv_size: Option<u8>,
) -> crate::Result<String> {
    if data.is_empty() {
        return Ok(String::new());
    }

    if let Ok(text) = std::str::from_utf8(data) {
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        if trimmed.starts_with("WEBVTT") {
            return Ok(webvtt_to_body(text, offset_secs, default_duration_secs));
        }
        if looks_like_ttml(trimmed) {
            return Ok(ttml_to_body(text, offset_secs, default_duration_secs));
        }
    }

    let Some(mut parsed) = parse_media_segment_with_default_iv_size(data, default_iv_size) else {
        return Ok(String::new());
    };
    if let Some(dec) = decryptor {
        dec.decrypt_segment(&mut parsed.mdat, &parsed.samples);
    }

    let scale = timescale.max(1) as f64;
    let mut local = 0.0;
    let mut out = String::new();
    for sample in &parsed.samples {
        let (a, b) = sample.data_range;
        let b = b.min(parsed.mdat.len());
        if a >= b {
            local += sample.duration as f64 / scale;
            continue;
        }
        let payload = &parsed.mdat[a..b];
        let sample_duration = sample.duration as f64 / scale;
        let sample_offset = offset_secs + local;
        let sample_body = sample_payload_to_body(payload, sample_offset, sample_duration);
        append_body(&mut out, &sample_body);
        local += sample_duration;
    }
    Ok(out)
}

fn sample_payload_to_body(payload: &[u8], offset_secs: f64, duration_secs: f64) -> String {
    if let Some(text) = extract_wvtt_payload(payload) {
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        if trimmed.starts_with("WEBVTT") {
            return webvtt_to_body(&text, offset_secs, duration_secs);
        }
        if looks_like_ttml(trimmed) {
            return ttml_to_body(&text, offset_secs, duration_secs);
        }
        if trimmed.is_empty() {
            return String::new();
        }
        return cue_body(offset_secs, offset_secs + duration_secs.max(0.001), trimmed);
    }

    if let Ok(text) = std::str::from_utf8(payload) {
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        if trimmed.starts_with("WEBVTT") {
            return webvtt_to_body(text, offset_secs, duration_secs);
        }
        if looks_like_ttml(trimmed) {
            return ttml_to_body(text, offset_secs, duration_secs);
        }
    }

    String::new()
}

fn extract_wvtt_payload(sample: &[u8]) -> Option<String> {
    let mut text = String::new();
    let boxes = iter_boxes(sample);
    if boxes.is_empty() {
        return std::str::from_utf8(sample).ok().map(|s| s.to_string());
    }
    for b in boxes {
        match &b.typ {
            b"vttc" => {
                for child in iter_boxes(b.payload) {
                    if &child.typ == b"payl" {
                        if let Ok(payload) = std::str::from_utf8(child.payload) {
                            append_text_line(&mut text, payload);
                        }
                    }
                }
            }
            b"payl" => {
                if let Ok(payload) = std::str::from_utf8(b.payload) {
                    append_text_line(&mut text, payload);
                }
            }
            b"vtte" => {}
            _ => {}
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

fn append_text_line(target: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(text);
}

#[derive(Clone)]
struct Cue {
    start: f64,
    end: f64,
    text: String,
    settings: String,
    id: Option<String>,
}

fn webvtt_to_body(text: &str, offset_secs: f64, default_duration_secs: f64) -> String {
    let mut cues = Vec::new();
    for block in split_blocks(text) {
        let lines: Vec<&str> = block.lines().map(str::trim_end).collect();
        if lines.is_empty() {
            continue;
        }
        let first = lines[0].trim_start_matches('\u{feff}').trim();
        if first.starts_with("WEBVTT")
            || first.starts_with("NOTE")
            || first.starts_with("STYLE")
            || first.starts_with("REGION")
        {
            continue;
        }

        let timing_idx = lines.iter().position(|line| line.contains("-->"));
        let Some(timing_idx) = timing_idx else {
            continue;
        };
        let Some((start, end, settings)) = parse_vtt_timing_line(lines[timing_idx]) else {
            continue;
        };
        let id = (timing_idx > 0).then(|| lines[..timing_idx].join("\n"));
        let text = lines[timing_idx + 1..].join("\n").trim().to_string();
        if text.is_empty() {
            continue;
        }
        cues.push(Cue {
            start,
            end,
            text,
            settings,
            id,
        });
    }

    normalize_and_format_cues(cues, offset_secs, default_duration_secs)
}

fn ttml_to_body(text: &str, offset_secs: f64, default_duration_secs: f64) -> String {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut cues = Vec::new();
    let mut in_p = false;
    let mut cur_begin = None;
    let mut cur_end = None;
    let mut cur_dur = None;
    let mut cur_text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = local_name(e.name().as_ref());
                if tag == "p" {
                    in_p = true;
                    cur_text.clear();
                    cur_begin = None;
                    cur_end = None;
                    cur_dur = None;
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        let value = String::from_utf8_lossy(&attr.value).to_string();
                        match key.as_str() {
                            "begin" => cur_begin = parse_time_expression(&value),
                            "end" => cur_end = parse_time_expression(&value),
                            "dur" => cur_dur = parse_time_expression(&value),
                            _ => {}
                        }
                    }
                } else if in_p && tag == "br" {
                    cur_text.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                if in_p && local_name(e.name().as_ref()) == "br" {
                    cur_text.push('\n');
                }
            }
            Ok(Event::Text(e)) => {
                if in_p {
                    if let Ok(text) = e.unescape() {
                        cur_text.push_str(text.trim());
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if in_p {
                    cur_text.push_str(String::from_utf8_lossy(&e).trim());
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "p" && in_p {
                    in_p = false;
                    let text = cur_text.trim().to_string();
                    if let Some(start) = cur_begin {
                        let end = cur_end
                            .or_else(|| cur_dur.map(|dur| start + dur))
                            .unwrap_or(start + default_duration_secs.max(0.001));
                        if !text.is_empty() && end > start {
                            cues.push(Cue {
                                start,
                                end,
                                text,
                                settings: String::new(),
                                id: None,
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    normalize_and_format_cues(cues, offset_secs, default_duration_secs)
}

fn normalize_and_format_cues(
    mut cues: Vec<Cue>,
    offset_secs: f64,
    default_duration_secs: f64,
) -> String {
    if cues.is_empty() {
        return String::new();
    }
    let min_start = cues
        .iter()
        .map(|cue| cue.start)
        .fold(f64::INFINITY, f64::min);
    let normalize = if min_start.is_finite() && min_start > default_duration_secs.max(1.0) * 2.0 {
        min_start
    } else {
        0.0
    };

    let mut out = String::new();
    for cue in cues.iter_mut() {
        cue.start = (cue.start - normalize + offset_secs).max(0.0);
        cue.end = (cue.end - normalize + offset_secs).max(cue.start + 0.001);
        if !out.is_empty() {
            out.push('\n');
        }
        if let Some(id) = &cue.id {
            if !id.trim().is_empty() {
                out.push_str(id.trim());
                out.push('\n');
            }
        }
        out.push_str(&format!(
            "{} --> {}{}\n{}\n",
            format_vtt_time(cue.start),
            format_vtt_time(cue.end),
            cue.settings,
            cue.text
        ));
    }
    out
}

fn split_blocks(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_vtt_timing_line(line: &str) -> Option<(f64, f64, String)> {
    let (start, rest) = line.split_once("-->")?;
    let mut right = rest.split_whitespace();
    let end = right.next()?;
    let settings = right.collect::<Vec<_>>();
    let settings = if settings.is_empty() {
        String::new()
    } else {
        format!(" {}", settings.join(" "))
    };
    Some((
        parse_vtt_time(start.trim())?,
        parse_vtt_time(end.trim())?,
        settings,
    ))
}

fn parse_vtt_time(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.split(':').collect();
    let seconds = parts.last()?.replace(',', ".").parse::<f64>().ok()?;
    match parts.len() {
        1 => Some(seconds),
        2 => Some(parts[0].parse::<f64>().ok()? * 60.0 + seconds),
        3 => Some(
            parts[0].parse::<f64>().ok()? * 3600.0 + parts[1].parse::<f64>().ok()? * 60.0 + seconds,
        ),
        _ => None,
    }
}

fn parse_time_expression(value: &str) -> Option<f64> {
    let value = value.trim();
    if let Some(ms) = value.strip_suffix("ms") {
        return ms.trim().parse::<f64>().ok().map(|v| v / 1000.0);
    }
    if let Some(s) = value.strip_suffix('s') {
        return s.trim().parse::<f64>().ok();
    }
    if let Some(m) = value.strip_suffix('m') {
        return m.trim().parse::<f64>().ok().map(|v| v * 60.0);
    }
    if let Some(h) = value.strip_suffix('h') {
        return h.trim().parse::<f64>().ok().map(|v| v * 3600.0);
    }

    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() == 4 {
        let h = parts[0].parse::<f64>().ok()?;
        let m = parts[1].parse::<f64>().ok()?;
        let s = parts[2].parse::<f64>().ok()?;
        let frame = parts[3].parse::<f64>().ok()?;
        return Some(h * 3600.0 + m * 60.0 + s + frame / 25.0);
    }
    parse_vtt_time(value)
}

fn format_vtt_time(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1000.0).round() as u64;
    let h = millis / 3_600_000;
    let m = (millis % 3_600_000) / 60_000;
    let s = (millis % 60_000) / 1000;
    let ms = millis % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

fn cue_body(start: f64, end: f64, text: &str) -> String {
    format!(
        "{} --> {}\n{}\n",
        format_vtt_time(start),
        format_vtt_time(end),
        text.trim()
    )
}

fn append_body(target: &mut String, body: &str) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    if !target.trim().is_empty() {
        target.push('\n');
    }
    target.push_str(body);
    target.push('\n');
}

fn looks_like_ttml(text: &str) -> bool {
    text.starts_with('<') && (text.contains("<tt") || text.contains(":tt") || text.contains("<p"))
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webvtt_body_offsets_cues() {
        let body = webvtt_to_body("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nhi\n", 4.0, 2.0);
        assert!(body.contains("00:00:05.000 --> 00:00:06.000"));
    }

    #[test]
    fn ttml_body_extracts_p_cues() {
        let body = ttml_to_body(
            r#"<tt><body><div><p begin="00:00:01.000" end="00:00:03.000">hello</p></div></body></tt>"#,
            2.0,
            4.0,
        );
        assert!(body.contains("00:00:03.000 --> 00:00:05.000"));
        assert!(body.contains("hello"));
    }
}
