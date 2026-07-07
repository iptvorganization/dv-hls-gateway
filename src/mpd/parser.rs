//! 解析 DASH MPD：提取视频/音频 Representation 及其分段时间线。
//!
//! 实测目标格式（CMAF DASH）：
//! - SegmentTemplate timescale=24000 initialization="$RepresentationID$/seg.mp4"
//!   media="$RepresentationID$/seg_$Number$.mp4" startNumber="0"
//! - SegmentTimeline: <S t d r/> 累加
//! - 音频模板可能不同 (seg-3.mp4 / seg-3_$Number$.mp4)

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

/// 一个分段引用。
#[derive(Debug, Clone)]
pub struct SegmentRef {
    pub number: u64,
    pub url: String,
    pub duration_ts: u64,
}

/// 一个可选轨道。
#[derive(Debug, Clone)]
pub struct Representation {
    pub id: String,
    pub kind: TrackKind,
    pub codecs: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub lang: String,
    pub timescale: u32,
    pub init_url: String,
    /// KID(s) advertised by MPD ContentProtection/@cenc:default_KID.
    pub manifest_kids: Vec<String>,
    pub media_template: String,
    pub start_number: u64,
    pub segments: Vec<SegmentRef>,
    pub total_duration_ts: u64,
}

#[derive(Debug, Clone)]
pub struct MpdInfo {
    pub base_url: String,
    pub representations: Vec<Representation>,
    pub is_dynamic: bool,
}

/// 解析 MPD 内容。`mpd_url` 用于推导 BaseURL（相对路径解析）。
pub fn parse_mpd(content: &str, mpd_url: &str) -> crate::Result<MpdInfo> {
    let fallback_base = derive_base(mpd_url);
    let origin = derive_origin(mpd_url);
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    // is_dynamic：精确解析 <MPD> 根标签的 type 属性（不受子元素 type="encoder" 等干扰）
    let mut is_dynamic = false;

    // BaseURL 层叠：MPD 级与 Period 级（DASH 允许各层级 <BaseURL> 重写段路径前缀）。
    // 有效 base = period_base 优先，否则 mpd_base，否则从 manifest URL 推导的目录。
    // resolve_url 据 base/origin 处理「相对 / 站点绝对(/) / 完整 http」三种形态。
    let mut mpd_base: Option<String> = None;
    let mut period_base: Option<String> = None;
    // 捕获 <BaseURL>…</BaseURL> 文本：进入元素时置 true，Text 事件读值，End 时清空。
    let mut in_base_url = false;
    // 当前所在层级（用于决定 BaseURL 归 MPD 级还是 Period 级）。
    let mut in_period = false;
    let mut seen_adaptationset_in_period = false;

    // 状态栈
    let mut cur_as_kind = TrackKind::Other;
    let mut cur_as_lang = String::new();
    let mut as_template: Option<SegTemplate> = None;
    let mut as_timeline: Vec<(Option<u64>, u64, u64)> = Vec::new(); // (t, d, r)
    let mut as_timescale: u32 = 1;
    let mut cur_as_codecs = String::new();
    let mut cur_as_manifest_kids: Vec<String> = Vec::new();

    let mut reps: Vec<Representation> = Vec::new();
    let mut buf = Vec::new();

    // 已解析过的 rep id：同一个 id 在后续 Period 重复出现 → SSAI 广告 Period
    // （复用主内容 rep id + 自己的 /atp/ 临时 BaseURL），跳过不纳入。
    let mut seen_rep_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 当前 Representation 临时
    let mut cur_rep: Option<RepTmp> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(ev @ Event::Start(_)) | Ok(ev @ Event::Empty(_)) => {
                let is_empty = matches!(ev, Event::Empty(_));
                let e = match &ev {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => unreachable!(),
                };
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                let attrs = collect_attrs(e);
                match tag.as_str() {
                    "MPD" => {
                        is_dynamic = attrs.get("type").map(|t| t == "dynamic").unwrap_or(false);
                    }
                    "Period" => {
                        in_period = true;
                        period_base = None;
                        seen_adaptationset_in_period = false;
                    }
                    "BaseURL" => {
                        // 空元素 <BaseURL/> 无意义；非空则等 Text 事件取值。
                        if !is_empty {
                            in_base_url = true;
                        }
                    }
                    "AdaptationSet" => {
                        seen_adaptationset_in_period = true;
                        cur_as_kind = detect_track_kind(
                            attrs.get("contentType").map(String::as_str),
                            attrs.get("mimeType").map(String::as_str),
                            attrs.get("codecs").map(String::as_str),
                        );
                        cur_as_lang = attrs.get("lang").cloned().unwrap_or_default();
                        cur_as_codecs = attrs.get("codecs").cloned().unwrap_or_default();
                        as_template = None;
                        as_timeline.clear();
                        as_timescale = 1;
                        cur_as_manifest_kids.clear();
                    }
                    "ContentProtection" => {
                        let kids = default_kids_from_attrs(&attrs);
                        if !kids.is_empty() {
                            if let Some(rep) = cur_rep.as_mut() {
                                extend_unique(&mut rep.manifest_kids, kids);
                            } else {
                                extend_unique(&mut cur_as_manifest_kids, kids);
                            }
                        }
                    }
                    "SegmentTemplate" => {
                        let t = SegTemplate {
                            init: attrs.get("initialization").cloned().unwrap_or_default(),
                            media: attrs.get("media").cloned().unwrap_or_default(),
                            start_number: attrs
                                .get("startNumber")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(1),
                        };
                        if let Some(ts) = attrs.get("timescale").and_then(|s| s.parse().ok()) {
                            as_timescale = ts;
                        }
                        as_template = Some(t);
                    }
                    "S" => {
                        let t = attrs.get("t").and_then(|s| s.parse().ok());
                        let d = attrs.get("d").and_then(|s| s.parse().ok()).unwrap_or(0);
                        let r = attrs.get("r").and_then(|s| s.parse().ok()).unwrap_or(0);
                        as_timeline.push((t, d, r));
                    }
                    "Representation" => {
                        let rt = RepTmp {
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            codecs: attrs
                                .get("codecs")
                                .cloned()
                                .unwrap_or_else(|| cur_as_codecs.clone()),
                            bandwidth: attrs
                                .get("bandwidth")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                            width: attrs.get("width").and_then(|s| s.parse().ok()).unwrap_or(0),
                            height: attrs
                                .get("height")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                            manifest_kids: default_kids_from_attrs(&attrs),
                        };
                        if is_empty {
                            // 自闭合 <Representation .../> ：立即完成
                            if let Some(tpl) = as_template.clone() {
                                // 跳过 SSAI 广告插播 Period：这类 Period 复用主内容 rep id，
                                // 但带自己的临时 BaseURL（/atp/<uuid>/）。按「同 rep id 首次出现
                                // 则纳入、重复出现则跳过」判断：首次见=主内容，后来再出现=广告。
                                // 这样既能过滤 SSAI 广告 Period，又不会误伤正常 DASH
                                // Period 级 BaseURL（如 dash/ 相对路径）。
                                let rep_kind = if cur_as_kind == TrackKind::Other {
                                    detect_track_kind(None, None, Some(&rt.codecs))
                                } else {
                                    cur_as_kind
                                };
                                if rep_kind != TrackKind::Other
                                    && seen_rep_ids.insert(rt.id.clone())
                                {
                                    let eff_base = period_base
                                        .as_deref()
                                        .or(mpd_base.as_deref())
                                        .unwrap_or(&fallback_base);
                                    reps.push(build_representation(
                                        eff_base,
                                        &origin,
                                        rt,
                                        tpl,
                                        rep_kind,
                                        &cur_as_lang,
                                        as_timescale,
                                        &as_timeline,
                                        &cur_as_manifest_kids,
                                    ));
                                }
                            }
                        } else {
                            cur_rep = Some(rt);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                if tag == "Representation" {
                    if let (Some(rt), Some(tpl)) = (cur_rep.take(), as_template.clone()) {
                        // 同上：按 rep id 去重，首次纳入、重复跳过（SSAI 广告 Period）。
                        let rep_kind = if cur_as_kind == TrackKind::Other {
                            detect_track_kind(None, None, Some(&rt.codecs))
                        } else {
                            cur_as_kind
                        };
                        if rep_kind != TrackKind::Other && seen_rep_ids.insert(rt.id.clone()) {
                            let eff_base = period_base
                                .as_deref()
                                .or(mpd_base.as_deref())
                                .unwrap_or(&fallback_base);
                            let rep = build_representation(
                                eff_base,
                                &origin,
                                rt,
                                tpl,
                                rep_kind,
                                &cur_as_lang,
                                as_timescale,
                                &as_timeline,
                                &cur_as_manifest_kids,
                            );
                            reps.push(rep);
                        }
                    }
                } else if tag == "BaseURL" {
                    in_base_url = false;
                } else if tag == "Period" {
                    in_period = false;
                    period_base = None;
                    seen_adaptationset_in_period = false;
                }
            }
            Ok(Event::Text(t)) => {
                if in_base_url {
                    let val = t
                        .unescape()
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    if !val.is_empty() {
                        // 归属层级：在 Period 内且尚未进入任何 AdaptationSet → Period 级；
                        // 否则（在 MPD 直属、Period 前）→ MPD 级。AdaptationSet/Representation
                        // 级 BaseURL 不常见且此源未用，统一并入当前有效层级即可。
                        if in_period && !seen_adaptationset_in_period {
                            // Period 级相对 MPD 级（或 fallback）解析，叠加层叠语义。
                            let parent = mpd_base.as_deref().unwrap_or(&fallback_base);
                            period_base = Some(resolve_base(parent, &origin, &val));
                        } else if !in_period {
                            period_base = None;
                            mpd_base = Some(resolve_base(&fallback_base, &origin, &val));
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("MPD parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    // 合并相同 id 的 Representation（多 Period）：段按 number 去重拼接。
    let merged = merge_by_id(reps);

    Ok(MpdInfo {
        base_url: mpd_base.unwrap_or(fallback_base),
        representations: merged,
        is_dynamic,
    })
}

/// 把多 Period 里同 id 的 Representation 合并，segments 按 number 去重后排序。
fn merge_by_id(reps: Vec<Representation>) -> Vec<Representation> {
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Representation> =
        std::collections::HashMap::new();
    for r in reps {
        match map.get_mut(&r.id) {
            Some(existing) => {
                existing.segments.extend(r.segments);
                existing.total_duration_ts += r.total_duration_ts;
                extend_unique(&mut existing.manifest_kids, r.manifest_kids);
            }
            None => {
                order.push(r.id.clone());
                map.insert(r.id.clone(), r);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| map.remove(&id))
        .map(|mut r| {
            // 按段号去重 + 排序（live 跨 Period/刷新可能重复）
            r.segments.sort_by_key(|s| s.number);
            r.segments.dedup_by_key(|s| s.number);
            r
        })
        .collect()
}

#[derive(Clone)]
struct SegTemplate {
    init: String,
    media: String,
    start_number: u64,
}

struct RepTmp {
    id: String,
    codecs: String,
    bandwidth: u64,
    width: u32,
    height: u32,
    manifest_kids: Vec<String>,
}

fn build_representation(
    base: &str,
    origin: &str,
    rt: RepTmp,
    tpl: SegTemplate,
    kind: TrackKind,
    lang: &str,
    timescale: u32,
    timeline: &[(Option<u64>, u64, u64)],
    as_manifest_kids: &[String],
) -> Representation {
    let init_url = resolve_url(base, origin, &expand_rep_vars(&tpl.init, &rt));
    let media_template = expand_rep_vars(&tpl.media, &rt);
    let manifest_kids = if rt.manifest_kids.is_empty() {
        as_manifest_kids.to_vec()
    } else {
        rt.manifest_kids.clone()
    };

    // 展开 SegmentTimeline → 段号/时长。
    // 支持 $Number$（段号递增）与 $Time$（段起始时间，SegmentTimeline 的 t 累加 d）两种模板。
    // 对于 $Time$ 模板：用 cur_time / d 作为段号（= 从时间零点起的绝对段序号），
    // 这样直播刷新 MPD（t 前进）后段号自然递增，不会被"窗口总是 1..31"困住。
    // 对于 $Number$ 模板：传统递增计数器。
    let is_time_template =
        media_template.contains("$Time$") && !media_template.contains("$Number$");
    let mut segments = Vec::new();
    let mut number = tpl.start_number;
    let mut total = 0u64;
    let mut cur_time: u64 = 0;
    for &(t, d, r) in timeline {
        if let Some(tt) = t {
            cur_time = tt; // 显式 t 重置时间基（DASH 允许 timeline 不连续）
        }
        for _ in 0..=r {
            let path = media_template
                .replace("$Number$", &number.to_string())
                .replace("$Time$", &cur_time.to_string());
            let seg_number = if is_time_template {
                cur_time // 直接用源绝对时间值（不除以 d），保留帧 DTS 底座精度
            } else {
                number
            };
            segments.push(SegmentRef {
                number: seg_number,
                url: resolve_url(base, origin, &path),
                duration_ts: d,
            });
            total += d;
            number += 1;
            cur_time += d;
        }
    }

    Representation {
        id: rt.id,
        kind,
        codecs: rt.codecs,
        bandwidth: rt.bandwidth,
        width: rt.width,
        height: rt.height,
        lang: lang.to_string(),
        timescale,
        init_url,
        manifest_kids,
        media_template: resolve_url(base, origin, &media_template),
        start_number: tpl.start_number,
        segments,
        total_duration_ts: total,
    }
}

fn collect_attrs(e: &quick_xml::events::BytesStart) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for a in e.attributes().flatten() {
        let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
        let v = String::from_utf8_lossy(&a.value).to_string();
        m.insert(k, v);
    }
    m
}

fn detect_track_kind(
    content_type: Option<&str>,
    mime_type: Option<&str>,
    codecs: Option<&str>,
) -> TrackKind {
    let content = content_type.unwrap_or_default().to_ascii_lowercase();
    if content == "video" {
        return TrackKind::Video;
    }
    if content == "audio" {
        return TrackKind::Audio;
    }
    if matches!(content.as_str(), "text" | "subtitle" | "subtitles") {
        return TrackKind::Subtitle;
    }

    let mime = mime_type.unwrap_or_default().to_ascii_lowercase();
    if mime.starts_with("video") {
        return TrackKind::Video;
    }
    if mime.starts_with("audio") {
        return TrackKind::Audio;
    }
    if mime.starts_with("text")
        || mime.contains("ttml")
        || mime.contains("webvtt")
        || mime.contains("subtitle")
    {
        return TrackKind::Subtitle;
    }

    let codecs = codecs.unwrap_or_default().to_ascii_lowercase();
    if codecs
        .split(',')
        .map(str::trim)
        .any(|c| matches!(c, "wvtt" | "stpp" | "ttml" | "webvtt") || c.starts_with("stpp."))
    {
        return TrackKind::Subtitle;
    }

    TrackKind::Other
}

fn default_kids_from_attrs(attrs: &std::collections::HashMap<String, String>) -> Vec<String> {
    attrs
        .iter()
        .filter(|(k, _)| {
            let key = k.to_ascii_lowercase();
            key == "default_kid" || key.ends_with(":default_kid")
        })
        .flat_map(|(_, value)| parse_default_kid_list(value))
        .fold(Vec::new(), |mut acc, kid| {
            if !acc.contains(&kid) {
                acc.push(kid);
            }
            acc
        })
}

fn parse_default_kid_list(value: &str) -> Vec<String> {
    value
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter_map(normalize_default_kid)
        .fold(Vec::new(), |mut acc, kid| {
            if !acc.contains(&kid) {
                acc.push(kid);
            }
            acc
        })
}

fn normalize_default_kid(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches('{')
        .trim_matches('}')
        .to_ascii_lowercase();
    let value = value.strip_prefix("urn:uuid:").unwrap_or(&value);
    let value = value.strip_prefix("0x").unwrap_or(value).replace('-', "");
    (value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())).then_some(value)
}

fn extend_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn expand_rep_vars(template: &str, rt: &RepTmp) -> String {
    template
        .replace("$RepresentationID$", &rt.id)
        .replace("$Bandwidth$", &rt.bandwidth.to_string())
}

/// 从 MPD URL 推导 base（去掉文件名，保留目录，保留 query 之外）。
fn derive_base(mpd_url: &str) -> String {
    let no_query = mpd_url.split('?').next().unwrap_or(mpd_url);
    match no_query.rfind('/') {
        Some(i) => no_query[..=i].to_string(),
        None => String::new(),
    }
}

/// 提取 URL 的 origin（scheme://host[:port]），用于解析以 `/` 开头的站点绝对路径。
/// 例如 https://cdn.example.com/a/b.mpd → https://cdn.example.com
fn derive_origin(url: &str) -> String {
    // 找 "://" 后的第一个 '/'，截到那里。
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        match url[after..].find('/') {
            Some(slash) => url[..after + slash].to_string(),
            None => url.to_string(), // 没有路径部分
        }
    } else {
        String::new()
    }
}

/// 把一个 BaseURL 值或段模板路径相对 `base`（当前有效目录前缀）+ `origin` 解析为绝对 URL。
/// 三种形态：
///   - 以 http(s):// 开头 → 已是绝对 URL，直接用。
///   - 以 `/` 开头 → 站点绝对路径，相对 origin（scheme://host）解析。
///   - 否则 → 相对 base 目录拼接。
fn resolve_url(base: &str, origin: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else if let Some(stripped) = path.strip_prefix('/') {
        // 站点绝对路径：origin + path（path 已含前导 /，origin 不含尾部 /）
        if origin.is_empty() {
            format!("/{stripped}")
        } else {
            format!("{origin}/{stripped}")
        }
    } else {
        format!("{base}{path}")
    }
}

/// 把一个 <BaseURL> 值解析成「目录前缀」（用于后续拼接 init/media 路径）。
/// 与 resolve_url 同样处理三种形态，但保证结果以 `/` 结尾，便于直接前缀拼接。
fn resolve_base(parent: &str, origin: &str, value: &str) -> String {
    let mut b = resolve_url(parent, origin, value);
    if !b.ends_with('/') {
        b.push('/');
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_derivation() {
        assert_eq!(
            derive_base("https://cdn/x/y/master.mpd?a=b"),
            "https://cdn/x/y/"
        );
    }

    #[test]
    fn parse_minimal_mpd() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="24000" initialization="$RepresentationID$/seg.mp4" media="$RepresentationID$/seg_$Number$.mp4" startNumber="0">
        <SegmentTimeline>
          <S t="0" d="144144" r="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="vid/dv5" codecs="dvh1.05.06" bandwidth="15000000" width="3840" height="2160"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/p/master.mpd").unwrap();
        assert!(!info.is_dynamic);
        assert_eq!(info.representations.len(), 1);
        let r = &info.representations[0];
        assert_eq!(r.kind, TrackKind::Video);
        assert_eq!(r.codecs, "dvh1.05.06");
        // r=2 → 3 段 (0,1,2)
        assert_eq!(r.segments.len(), 3);
        assert_eq!(r.segments[0].url, "https://cdn/p/vid/dv5/seg_0.mp4");
        assert_eq!(r.init_url, "https://cdn/p/vid/dv5/seg.mp4");
        assert_eq!(r.segments[1].number, 1);
    }

    #[test]
    fn content_protection_default_kid_is_inherited_from_adaptation_set() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic" xmlns:cenc="urn:mpeg:cenc:2013">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" cenc:default_KID="11111111-1111-1111-8111-000000000000"/>
      <SegmentTemplate timescale="24000" initialization="$RepresentationID$/i.mp4" media="$RepresentationID$/$Number$.mp4" startNumber="1">
        <SegmentTimeline><S t="0" d="24000" r="0"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v" codecs="avc1" bandwidth="1" width="1" height="1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/p/master.mpd").unwrap();
        assert_eq!(
            info.representations[0].manifest_kids,
            vec!["11111111111111118111000000000000"]
        );
    }

    #[test]
    fn representation_content_protection_overrides_adaptation_set_kid() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic" xmlns:cenc="urn:mpeg:cenc:2013">
  <Period>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <ContentProtection cenc:default_KID="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"/>
      <SegmentTemplate timescale="48000" initialization="$RepresentationID$/i.mp4" media="$RepresentationID$/$Number$.mp4" startNumber="1">
        <SegmentTimeline><S t="0" d="96000" r="0"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a" codecs="ec-3" bandwidth="666000">
        <ContentProtection cenc:default_KID="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb cccccccc-cccc-cccc-cccc-cccccccccccc"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/p/master.mpd").unwrap();
        assert_eq!(
            info.representations[0].manifest_kids,
            vec![
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "cccccccccccccccccccccccccccccccc"
            ]
        );
    }

    #[test]
    fn dynamic_type_parsed_precisely() {
        // type="dynamic" 应被识别为 live；且不受子元素 type="encoder" 干扰
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic" minimumUpdatePeriod="PT4S">
  <Period>
    <EventStream><Event type="encoder"/></EventStream>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="24000" initialization="$RepresentationID$/i.mp4" media="$RepresentationID$/$Number$.mp4" startNumber="100">
        <SegmentTimeline><S t="0" d="96096" r="9"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v" codecs="avc1.64001f" bandwidth="3000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/m.mpd").unwrap();
        assert!(info.is_dynamic, "type=dynamic 应识别为 live");
        // startNumber=100, r=9 → 段号 100..109 共 10 段
        let r = &info.representations[0];
        assert_eq!(r.segments.len(), 10);
        assert_eq!(r.segments.first().unwrap().number, 100);
        assert_eq!(r.segments.last().unwrap().number, 109);
    }

    #[test]
    fn static_type_is_vod() {
        let mpd = r#"<MPD type="static"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate timescale="24000" initialization="$RepresentationID$/i.mp4" media="$RepresentationID$/$Number$.mp4" startNumber="0"><SegmentTimeline><S t="0" d="96096" r="0"/></SegmentTimeline></SegmentTemplate><Representation id="v" codecs="hvc1" bandwidth="1" width="1" height="1"/></AdaptationSet></Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/m.mpd").unwrap();
        assert!(!info.is_dynamic);
    }

    #[test]
    fn origin_extraction() {
        assert_eq!(
            derive_origin("https://cdn.example.com/a/b.mpd"),
            "https://cdn.example.com"
        );
        assert_eq!(derive_origin("https://h:8080/x"), "https://h:8080");
        assert_eq!(derive_origin("https://h"), "https://h");
    }

    #[test]
    fn resolve_url_three_forms() {
        let base = "https://cdn/dir/";
        let origin = "https://cdn";
        // 完整 URL：直用
        assert_eq!(
            resolve_url(base, origin, "https://x/y.mp4"),
            "https://x/y.mp4"
        );
        // 站点绝对（/）：origin + path
        assert_eq!(
            resolve_url(base, origin, "/atp/z/seg.mp4"),
            "https://cdn/atp/z/seg.mp4"
        );
        // 相对：base 目录拼接
        assert_eq!(
            resolve_url(base, origin, "v/seg.mp4"),
            "https://cdn/dir/v/seg.mp4"
        );
    }

    /// MPD 级 <BaseURL>（带签名 token 的目录前缀）应重写段路径，而非用 manifest 目录。
    #[test]
    fn mpd_level_baseurl_rewrites_segments() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <BaseURL>/v~sig_e~123_u~abc/co01/channel(5021104)/</BaseURL>
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="240000" initialization="$RepresentationID$_init.m4i" media="$RepresentationID$_Segment-$Number$.mp4" startNumber="4818">
        <SegmentTimeline><S t="0" d="936936" r="1"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="item-07" codecs="avc1.64002a" bandwidth="10000000" width="1920" height="1080"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(
            mpd,
            "https://g003-cdn.example.com/pck-sle/v1/dash/abc/foo/master_2hr.mpd?x=1",
        )
        .unwrap();
        let r = &info.representations[0];
        // init/段应走 origin + MPD BaseURL，而不是 manifest 目录
        assert_eq!(
            r.init_url,
            "https://g003-cdn.example.com/v~sig_e~123_u~abc/co01/channel(5021104)/item-07_init.m4i"
        );
        assert_eq!(r.segments[0].url, "https://g003-cdn.example.com/v~sig_e~123_u~abc/co01/channel(5021104)/item-07_Segment-4818.mp4");
        assert_eq!(r.segments[0].number, 4818);
        assert_eq!(r.segments.len(), 2);
    }

    /// SSAI 广告 Period（带 Period 级 <BaseURL>，rep id 与主内容复用同名）应被跳过，
    /// 只保留主内容 Period（无 Period 级 BaseURL，用 MPD 级签名 BaseURL）的段。
    #[test]
    fn ssai_ad_periods_are_skipped() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <BaseURL>/main/content/</BaseURL>
  <Period id="content">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="240000" initialization="$RepresentationID$_i.m4i" media="$RepresentationID$_$Number$.mp4" startNumber="0">
        <SegmentTimeline><S t="0" d="100" r="0"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v" codecs="avc1" bandwidth="1" width="1" height="1"/>
    </AdaptationSet>
  </Period>
  <Period id="ad">
    <BaseURL>/atp/uuid/cmaf_v3/</BaseURL>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="240000" initialization="master_init.cmfv" media="master_$Time$.cmfv" startNumber="0">
        <SegmentTimeline><S t="0" d="100" r="0"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v" codecs="avc1" bandwidth="1" width="1" height="1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn.example.com/p/master.mpd").unwrap();
        let v = info.representations.iter().find(|r| r.id == "v").unwrap();
        // 只应有主内容 1 段，init/段走 MPD 级 BaseURL，不含广告 Period 的 /atp/ 段。
        assert_eq!(v.init_url, "https://cdn.example.com/main/content/v_i.m4i");
        assert_eq!(v.segments.len(), 1);
        assert_eq!(
            v.segments[0].url,
            "https://cdn.example.com/main/content/v_0.mp4"
        );
        assert!(
            !v.init_url.contains("/atp/"),
            "广告 Period 不应污染主内容 init"
        );
    }

    /// 无任何 <BaseURL> 时回退到 manifest 目录（保持旧行为）。
    #[test]
    fn no_baseurl_falls_back_to_manifest_dir() {
        let mpd = r#"<MPD type="static"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate timescale="24000" initialization="$RepresentationID$/i.mp4" media="$RepresentationID$/$Number$.mp4" startNumber="0"><SegmentTimeline><S t="0" d="96096" r="0"/></SegmentTimeline></SegmentTemplate><Representation id="v" codecs="hvc1" bandwidth="1" width="1" height="1"/></AdaptationSet></Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/p/master.mpd").unwrap();
        assert_eq!(
            info.representations[0].segments[0].url,
            "https://cdn/p/v/0.mp4"
        );
    }

    /// $Time$ 模板：段 URL 用 SegmentTimeline 的 t 值（累加 d），而非 $Number$。
    #[test]
    fn time_template_expands_with_timeline_t() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="240000" initialization="v_init.m4i" media="v_$Time$.cmfv" startNumber="0">
        <SegmentTimeline><S t="1000" d="100" r="2"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v" codecs="avc1" bandwidth="1" width="1" height="1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/p/m.mpd").unwrap();
        let r = &info.representations[0];
        // r=2 → 3 段，time = 1000, 1100, 1200
        assert_eq!(r.segments.len(), 3);
        assert_eq!(r.segments[0].url, "https://cdn/p/v_1000.cmfv");
        assert_eq!(r.segments[1].url, "https://cdn/p/v_1100.cmfv");
        assert_eq!(r.segments[2].url, "https://cdn/p/v_1200.cmfv");
    }

    #[test]
    fn bandwidth_template_expands_in_init_and_media_urls() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="10000000" initialization="chunk_$RepresentationID$_$Bandwidth$_init.mp4" media="chunk_$RepresentationID$_$Bandwidth$_t$Time$.mp4">
        <SegmentTimeline><S t="17832777776145666" d="20020000" r="1"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="video_3" codecs="hvc1.2.4.L153.90" bandwidth="20680000" width="3840" height="2160"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn.example.com/live/manifest.mpd").unwrap();
        let r = &info.representations[0];
        assert_eq!(
            r.init_url,
            "https://cdn.example.com/live/chunk_video_3_20680000_init.mp4"
        );
        assert_eq!(
            r.segments[0].url,
            "https://cdn.example.com/live/chunk_video_3_20680000_t17832777776145666.mp4"
        );
        assert_eq!(
            r.segments[1].url,
            "https://cdn.example.com/live/chunk_video_3_20680000_t17832777796165666.mp4"
        );
    }

    /// 回归：feed1 形态（无 <BaseURL>、$Number$ 模板、大 startNumber），
    /// 修 B 后行为必须与旧代码字节级一致（走 fallback_base + $Number$ 路径）。
    #[test]
    fn regression_number_template_no_baseurl() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="240000" initialization="$RepresentationID$_init.m4i" media="$RepresentationID$_Segment-$Number$.mp4" startNumber="4818">
        <SegmentTimeline><S t="0" d="936936" r="2"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="item-06" codecs="hvc1.2.4.L153.B0" bandwidth="13000000" width="3840" height="2160"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(
            mpd,
            "https://g002-cdn.example.com/v~sig/co01/channel(5021147)/master_2min.mpd",
        )
        .unwrap();
        let r = &info.representations[0];
        assert_eq!(
            r.init_url,
            "https://g002-cdn.example.com/v~sig/co01/channel(5021147)/item-06_init.m4i"
        );
        assert_eq!(r.segments.len(), 3);
        assert_eq!(r.segments[0].number, 4818);
        assert_eq!(
            r.segments[0].url,
            "https://g002-cdn.example.com/v~sig/co01/channel(5021147)/item-06_Segment-4818.mp4"
        );
        assert_eq!(r.segments[2].number, 4820);
    }
}
