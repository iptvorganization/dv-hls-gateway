//! 解析 DASH MPD：提取视频/音频 Representation 及其分段时间线。
//!
//! 实测目标格式（CMAF DASH）：
//! - SegmentTemplate timescale=24000 initialization="$RepresentationID$/seg.mp4"
//!   media="$RepresentationID$/seg_$Number$.mp4" startNumber="0"
//! - SegmentTimeline: <S t d r/> 累加
//! - 音频模板可能不同 (seg-3.mp4 / seg-3_$Number$.mp4)

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure};
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
    /// 分片在 MPD 播放时间轴的起点（秒）；不是 URL 中的序号。
    pub presentation_start_secs: f64,
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
    /// 轨道或其父适配集是否声明了 ContentProtection，即使没有 KID。
    pub has_content_protection: bool,
    pub media_template: String,
    pub start_number: u64,
    /// Segment cursors use media timestamps rather than sequential numbers.
    pub time_addressed: bool,
    pub presentation_time_offset: u64,
    pub period_duration: Option<Duration>,
    pub segments: Vec<SegmentRef>,
    pub total_duration_ts: u64,
}

#[derive(Debug, Clone)]
struct UnsupportedRepresentation {
    id: String,
    kind: TrackKind,
    reason: String,
}

#[derive(Debug, Clone)]
pub struct MpdInfo {
    pub base_url: String,
    pub representations: Vec<Representation>,
    pub is_dynamic: bool,
    unsupported: Vec<UnsupportedRepresentation>,
}

impl MpdInfo {
    /// Reject an explicitly requested unsupported track rather than silently
    /// selecting another track or blaming it on the entire manifest.
    pub fn select_representation(
        &self,
        id: &str,
        kind: TrackKind,
    ) -> crate::Result<&Representation> {
        if let Some(rep) = self
            .representations
            .iter()
            .find(|r| r.id == id && r.kind == kind)
        {
            return Ok(rep);
        }
        if let Some(track) = self
            .unsupported
            .iter()
            .find(|r| r.id == id && r.kind == kind)
        {
            bail!("DASH {kind:?} representation {id:?}: {}", track.reason);
        }
        bail!("DASH {kind:?} representation {id:?} not found");
    }
}

/// 解析 MPD 内容。`mpd_url` 用于推导 BaseURL（相对路径解析）。
pub fn parse_mpd(content: &str, mpd_url: &str) -> crate::Result<MpdInfo> {
    let fallback_base = derive_base(mpd_url);
    let origin = derive_origin(mpd_url);
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut is_dynamic = false;
    let mut has_availability_start = false;
    let mut media_duration = None;
    let mut mpd_base: Option<String> = None;
    let mut period_base: Option<String> = None;
    let mut in_base_url = false;
    let mut in_timeline = false;
    let mut periods = Vec::new();
    let mut period_index = None;
    let mut period_template = SegTemplate::default();
    let mut adaptation: Option<Adaptation> = None;
    let mut cur_rep: Option<RepTmp> = None;
    let mut drafts = Vec::new();
    let mut seen_rep_ids = HashSet::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(ev @ Event::Start(_)) | Ok(ev @ Event::Empty(_)) => {
                let is_empty = matches!(ev, Event::Empty(_));
                let e = match &ev {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => unreachable!(),
                };
                let attrs = collect_attrs(e)?;
                match e.local_name().as_ref() {
                    b"MPD" => {
                        is_dynamic = attrs.get("type").is_some_and(|t| t == "dynamic");
                        has_availability_start = attrs.contains_key("availabilityStartTime");
                        media_duration = duration_attr(&attrs, "mediaPresentationDuration")?;
                    }
                    b"Period" => {
                        let declared_start = duration_attr(&attrs, "start")?;
                        let duration = duration_attr(&attrs, "duration")?;
                        let start = declared_start.or_else(|| match periods.last() {
                            None => Some(Duration::ZERO),
                            Some(prev) => {
                                let prev: &PeriodTiming = prev;
                                prev.start?.checked_add(prev.duration?)
                            }
                        });
                        if !is_empty {
                            period_index = Some(periods.len());
                            period_base = None;
                            period_template = SegTemplate::default();
                        }
                        periods.push(PeriodTiming { start, duration });
                    }
                    b"BaseURL" => in_base_url = !is_empty,
                    b"AdaptationSet" if !is_empty => {
                        adaptation = Some(Adaptation {
                            kind: detect_track_kind(
                                attrs.get("contentType").map(String::as_str),
                                attrs.get("mimeType").map(String::as_str),
                                attrs.get("codecs").map(String::as_str),
                            ),
                            lang: attrs.get("lang").cloned().unwrap_or_default(),
                            codecs: attrs.get("codecs").cloned().unwrap_or_default(),
                            template: SegTemplate::default(),
                            manifest_kids: Vec::new(),
                            has_content_protection: false,
                        });
                    }
                    b"ContentProtection" => {
                        let kids = default_kids_from_attrs(&attrs);
                        if let Some(rep) = cur_rep.as_mut() {
                            rep.has_content_protection = true;
                            extend_unique(&mut rep.manifest_kids, kids);
                        } else if let Some(aset) = adaptation.as_mut() {
                            aset.has_content_protection = true;
                            extend_unique(&mut aset.manifest_kids, kids);
                        }
                    }
                    b"SegmentTemplate" => {
                        let template =
                            active_template(&mut cur_rep, &mut adaptation, &mut period_template);
                        if let Err(e) = template.read_attributes(&attrs) {
                            template
                                .error
                                .get_or_insert_with(|| Arc::from(e.to_string()));
                        }
                    }
                    b"SegmentTimeline" => {
                        active_template(&mut cur_rep, &mut adaptation, &mut period_template)
                            .timeline = Some(Arc::new(Vec::new()));
                        in_timeline = !is_empty;
                    }
                    b"S" if in_timeline => {
                        let template =
                            active_template(&mut cur_rep, &mut adaptation, &mut period_template);
                        let item = parse_timeline_entry(&attrs);
                        match (template.timeline.as_mut(), item) {
                            (Some(timeline), Ok(entry)) => Arc::make_mut(timeline).push(entry),
                            (None, _) => {
                                template.error.get_or_insert_with(|| {
                                    Arc::from("SegmentTimeline changed scope unexpectedly")
                                });
                            }
                            (_, Err(e)) => {
                                template
                                    .error
                                    .get_or_insert_with(|| Arc::from(e.to_string()));
                            }
                        }
                    }
                    b"Representation" => {
                        let aset = adaptation
                            .as_ref()
                            .ok_or_else(|| anyhow!("Representation requires an AdaptationSet"))?;
                        let rt = RepTmp {
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            kind: detect_track_kind(
                                attrs.get("contentType").map(String::as_str),
                                attrs.get("mimeType").map(String::as_str),
                                attrs.get("codecs").map(String::as_str),
                            ),
                            codecs: attrs
                                .get("codecs")
                                .cloned()
                                .unwrap_or_else(|| aset.codecs.clone()),
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
                            has_content_protection: false,
                            template: SegTemplate::default(),
                        };
                        if is_empty {
                            record_representation(
                                rt,
                                aset,
                                period_index,
                                &period_template,
                                period_base
                                    .as_deref()
                                    .or(mpd_base.as_deref())
                                    .unwrap_or(&fallback_base),
                                &mut drafts,
                                &mut seen_rep_ids,
                            );
                        } else {
                            cur_rep = Some(rt);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"Representation" => {
                    if let (Some(rt), Some(aset)) = (cur_rep.take(), adaptation.as_ref()) {
                        record_representation(
                            rt,
                            aset,
                            period_index,
                            &period_template,
                            period_base
                                .as_deref()
                                .or(mpd_base.as_deref())
                                .unwrap_or(&fallback_base),
                            &mut drafts,
                            &mut seen_rep_ids,
                        );
                    }
                }
                b"AdaptationSet" => adaptation = None,
                b"BaseURL" => in_base_url = false,
                b"SegmentTimeline" => in_timeline = false,
                b"Period" => {
                    period_index = None;
                    period_base = None;
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_base_url => {
                let val = t.unescape()?.trim().to_string();
                if !val.is_empty() {
                    // 与旧解析器一致，支持 MPD 与 Period 两级 BaseURL。
                    if period_index.is_some() && adaptation.is_none() {
                        let parent = mpd_base.as_deref().unwrap_or(&fallback_base);
                        period_base = Some(resolve_base(parent, &origin, &val));
                    } else if period_index.is_none() && adaptation.is_none() {
                        mpd_base = Some(resolve_base(&fallback_base, &origin, &val));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow!("MPD parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    let mut budget = ExpansionBudget::default();
    let mut representations = Vec::new();
    let mut unsupported = Vec::new();
    for draft in drafts {
        let (period_start, period_duration) = period_bounds(&periods, draft.period, media_duration);
        let id = draft.rep.id.clone();
        let kind = draft.kind;
        match build_representation(
            draft,
            &origin,
            period_start,
            period_duration,
            is_dynamic && has_availability_start,
            &mut budget,
        ) {
            Ok(rep) => representations.push(rep),
            Err(err) if err.is::<GlobalExpansionLimit>() => return Err(err),
            Err(err) => unsupported.push(UnsupportedRepresentation {
                id,
                kind,
                reason: err.to_string(),
            }),
        }
    }
    if representations.is_empty() {
        if let Some(track) = unsupported.first() {
            bail!("DASH representation {:?}: {}", track.id, track.reason);
        }
    }

    Ok(MpdInfo {
        base_url: mpd_base.unwrap_or(fallback_base),
        representations,
        is_dynamic,
        unsupported,
    })
}

type Attrs = HashMap<String, String>;

#[derive(Clone, Default)]
struct SegTemplate {
    init: Option<String>,
    media: Option<String>,
    start_number: Option<u64>,
    duration: Option<u64>,
    timescale: Option<u32>,
    presentation_time_offset: Option<u64>,
    ept_delta: Option<i64>,
    timeline: Option<Arc<Vec<TimelineEntry>>>,
    error: Option<Arc<str>>,
}

impl SegTemplate {
    fn read_attributes(&mut self, attrs: &Attrs) -> crate::Result<()> {
        self.init = attrs.get("initialization").cloned();
        self.media = attrs.get("media").cloned();
        self.start_number = parse_attr(attrs, "startNumber")?;
        self.duration = parse_attr(attrs, "duration")?;
        self.timescale = parse_attr(attrs, "timescale")?;
        self.presentation_time_offset = parse_attr(attrs, "presentationTimeOffset")?;
        self.ept_delta = parse_attr(attrs, "eptDelta")?;
        Ok(())
    }

    fn inherit(&self, parent: &Self) -> Self {
        Self {
            init: self.init.clone().or_else(|| parent.init.clone()),
            media: self.media.clone().or_else(|| parent.media.clone()),
            start_number: self.start_number.or(parent.start_number),
            duration: self.duration.or(parent.duration),
            timescale: self.timescale.or(parent.timescale),
            presentation_time_offset: self
                .presentation_time_offset
                .or(parent.presentation_time_offset),
            ept_delta: self.ept_delta.or(parent.ept_delta),
            timeline: self.timeline.clone().or_else(|| parent.timeline.clone()),
            error: self.error.clone().or_else(|| parent.error.clone()),
        }
    }
}

#[derive(Clone, Copy)]
struct TimelineEntry {
    t: Option<u64>,
    d: u64,
    r: i64,
}

fn parse_timeline_entry(attrs: &Attrs) -> crate::Result<TimelineEntry> {
    let d = parse_attr(attrs, "d")?
        .filter(|value: &u64| *value > 0)
        .ok_or_else(|| anyhow!("SegmentTimeline S@d must be positive"))?;
    Ok(TimelineEntry {
        t: parse_attr(attrs, "t")?,
        d,
        r: parse_attr(attrs, "r")?.unwrap_or(0),
    })
}

struct PeriodTiming {
    start: Option<Duration>,
    duration: Option<Duration>,
}

struct Adaptation {
    kind: TrackKind,
    lang: String,
    codecs: String,
    template: SegTemplate,
    manifest_kids: Vec<String>,
    has_content_protection: bool,
}

struct RepTmp {
    id: String,
    kind: TrackKind,
    codecs: String,
    bandwidth: u64,
    width: u32,
    height: u32,
    manifest_kids: Vec<String>,
    has_content_protection: bool,
    template: SegTemplate,
}

struct RepDraft {
    rep: RepTmp,
    template: SegTemplate,
    kind: TrackKind,
    lang: String,
    base: String,
    period: Option<usize>,
}

fn active_template<'a>(
    rep: &'a mut Option<RepTmp>,
    adaptation: &'a mut Option<Adaptation>,
    period: &'a mut SegTemplate,
) -> &'a mut SegTemplate {
    if let Some(rep) = rep.as_mut() {
        &mut rep.template
    } else if let Some(aset) = adaptation.as_mut() {
        &mut aset.template
    } else {
        period
    }
}

fn record_representation(
    mut rep: RepTmp,
    aset: &Adaptation,
    period: Option<usize>,
    period_template: &SegTemplate,
    base: &str,
    drafts: &mut Vec<RepDraft>,
    seen_rep_ids: &mut HashSet<String>,
) {
    let kind = if rep.kind != TrackKind::Other {
        rep.kind
    } else if aset.kind == TrackKind::Other {
        detect_track_kind(None, None, Some(&rep.codecs))
    } else {
        aset.kind
    };
    if kind == TrackKind::Other {
        return;
    }
    let template = rep
        .template
        .inherit(&aset.template.inherit(period_template));
    // 既有 SSAI 策略：同名轨只保留首次出现的 Period，不尝试跨 Period 拼接。
    if !seen_rep_ids.insert(rep.id.clone()) {
        return;
    }
    if rep.manifest_kids.is_empty() {
        rep.manifest_kids.clone_from(&aset.manifest_kids);
    }
    rep.has_content_protection |= aset.has_content_protection || !rep.manifest_kids.is_empty();
    drafts.push(RepDraft {
        rep,
        template,
        kind,
        lang: aset.lang.clone(),
        base: base.to_string(),
        period,
    });
}

fn period_bounds(
    periods: &[PeriodTiming],
    index: Option<usize>,
    media_duration: Option<Duration>,
) -> (Option<Duration>, Option<Duration>) {
    let Some(i) = index else {
        return (Some(Duration::ZERO), media_duration);
    };
    let period = &periods[i];
    let duration = period.duration.or_else(|| {
        let end = match periods.get(i + 1) {
            Some(next) => next.start?,
            None => media_duration?,
        };
        end.checked_sub(period.start?)
    });
    (period.start, duration)
}

const MAX_SEGMENTS_PER_REP: u64 = 20_000;
const MAX_MPD_SEGMENTS: u64 = 100_000;
const MAX_MPD_SEGMENT_URL_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("MPD expansion exceeds {0}")]
struct GlobalExpansionLimit(&'static str);

#[derive(Default)]
struct ExpansionBudget {
    segments: u64,
    url_bytes: usize,
}

impl ExpansionBudget {
    fn reserve(&mut self, existing: usize, count: u64) -> crate::Result<()> {
        ensure!(
            count <= MAX_SEGMENTS_PER_REP - existing as u64,
            "MPD expansion exceeds {MAX_SEGMENTS_PER_REP} segments per representation"
        );
        if count > MAX_MPD_SEGMENTS - self.segments {
            return Err(GlobalExpansionLimit("100000 total segments").into());
        }
        self.segments += count;
        Ok(())
    }

    fn add_url(&mut self, url: &str) -> crate::Result<()> {
        self.url_bytes = self.url_bytes.saturating_add(url.len());
        if self.url_bytes > MAX_MPD_SEGMENT_URL_BYTES {
            return Err(GlobalExpansionLimit("32 MiB of segment URLs").into());
        }
        Ok(())
    }
}

fn build_representation(
    draft: RepDraft,
    origin: &str,
    period_start: Option<Duration>,
    period_duration: Option<Duration>,
    live_availability_required: bool,
    budget: &mut ExpansionBudget,
) -> crate::Result<Representation> {
    let RepDraft {
        rep,
        template: tpl,
        kind,
        lang,
        base,
        ..
    } = draft;
    if let Some(reason) = &tpl.error {
        bail!("{reason}");
    }
    let timescale = tpl.timescale.unwrap_or(1);
    ensure!(timescale > 0, "SegmentTemplate@timescale must be positive");
    let start_number = tpl.start_number.unwrap_or(1);
    let pto = tpl.presentation_time_offset.unwrap_or(0);
    let init_url = tpl
        .init
        .as_deref()
        .filter(|init| !init.is_empty())
        .map(|init| {
            let compiled = CompiledTemplate::parse(init)?;
            ensure!(
                !compiled.uses_number && !compiled.uses_time,
                "initialization template cannot use segment number or time"
            );
            Ok::<_, anyhow::Error>(resolve_url(&base, origin, &compiled.render(&rep, 0, 0)))
        })
        .transpose()?
        .unwrap_or_default();
    let media_template = tpl
        .media
        .as_deref()
        .filter(|media| !media.is_empty())
        .ok_or_else(|| anyhow!("no supported media SegmentTemplate"))?;
    let template = CompiledTemplate::parse(media_template)?;
    let is_time_template = template.uses_time && !template.uses_number;
    let period_start_secs = period_start.unwrap_or_default().as_secs_f64();
    let period_end_ts = period_duration
        .map(|duration| duration_ticks(duration, timescale))
        .transpose()?
        .map(i128::from);
    let mut segments = Vec::new();
    let mut total = 0u64;
    let mut append = |offset: u64,
                      url_time: u64,
                      sample_time: i128,
                      duration: u64,
                      budget: &mut ExpansionBudget|
     -> crate::Result<Option<SegmentRef>> {
        // Include only references that intersect the Period. Preserve the
        // URL's original sample time/number if some references were skipped.
        let relative_start = sample_time - i128::from(pto);
        let relative_end = relative_start + i128::from(duration);
        let clipped_end = period_end_ts.map_or(relative_end, |end| relative_end.min(end));
        if clipped_end <= relative_start.max(0) {
            return Ok(None);
        }
        // Preserve raw presentation start for matching; an unrepresentable
        // leading crop is checked against the actual access units later.
        let duration = u64::try_from(clipped_end - relative_start)?;
        let number = if is_time_template {
            url_time
        } else {
            start_number
                .checked_add(offset)
                .ok_or_else(|| anyhow!("SegmentTemplate number overflow"))?
        };
        ensure!(
            number < u64::MAX,
            "SegmentTemplate number exceeds cursor range"
        );
        let path = template.render(&rep, number, url_time);
        let url = resolve_url(&base, origin, &path);
        budget.add_url(&url)?;
        total = total
            .checked_add(duration)
            .ok_or_else(|| anyhow!("MPD segment duration overflow"))?;
        Ok(Some(SegmentRef {
            number,
            url,
            duration_ts: duration,
            presentation_start_secs: period_start_secs
                + relative_start as f64 / f64::from(timescale),
        }))
    };

    if let Some(timeline) = tpl.timeline.as_deref().filter(|t| !t.is_empty()) {
        let mut current = 0u64;
        let mut offset = 0u64;
        for (i, entry) in timeline.iter().enumerate() {
            current = entry.t.unwrap_or(current);
            let count = match entry.r {
                r if r >= 0 => r as u64 + 1,
                -1 => {
                    let end = match timeline.get(i + 1) {
                        Some(next) => next.t,
                        None => period_duration
                            .map(|d| duration_ticks(d, timescale))
                            .transpose()?
                            .and_then(|d| pto.checked_add(d)),
                    }
                    .ok_or_else(|| anyhow!("SegmentTimeline S@r=-1 requires a finite end time"))?;
                    ensure!(
                        end > current,
                        "SegmentTimeline S@r=-1 ends before its start"
                    );
                    (end - current).div_ceil(entry.d)
                }
                _ => bail!("SegmentTimeline S@r must not be less than -1"),
            };
            budget.reserve(usize::try_from(offset)?, count)?;
            let end = count
                .checked_mul(entry.d)
                .and_then(|span| current.checked_add(span))
                .ok_or_else(|| anyhow!("MPD SegmentTimeline time overflow"))?;
            for i in 0..count {
                let t = current + i * entry.d; // Bounded by checked end above.
                segments.extend(append(offset + i, t, i128::from(t), entry.d, budget)?);
            }
            offset += count; // Bounded by the per-track expansion budget.
            current = end;
        }
    } else if let Some(d) = tpl.duration {
        ensure!(d > 0, "SegmentTemplate@duration must be positive");
        ensure!(
            period_start.is_some(),
            "duration-only SegmentTemplate requires a known Period start"
        );
        ensure!(
            !live_availability_required,
            "live duration-only SegmentTemplate with availabilityStartTime is not supported"
        );
        ensure!(
            is_time_template || template.uses_number,
            "duration-only SegmentTemplate requires a $Number$ or $Time$ media variable"
        );
        let period_end = period_end_ts.ok_or_else(|| {
            anyhow!("duration-only SegmentTemplate requires a finite Period duration")
        })?;
        let ept_delta = i128::from(tpl.ept_delta.unwrap_or(0));
        ensure!(
            period_end > 0
                && ept_delta <= 0
                && ept_delta > -i128::from(d)
                && i128::from(pto) + ept_delta >= 0,
            "invalid duration-only SegmentTemplate timing"
        );
        let period_span = u64::try_from(period_end - ept_delta)
            .map_err(|_| anyhow!("MPD Period duration overflow"))?;
        let count = period_span.div_ceil(d);
        budget.reserve(segments.len(), count)?;
        for offset in 0..count {
            let elapsed = offset
                .checked_mul(d)
                .ok_or_else(|| anyhow!("MPD SegmentTemplate time overflow"))?;
            let duration = d.min(period_span - elapsed);
            // Simple addressing uses PTO + n*d for $Time$, regardless of eptDelta.
            let t = pto
                .checked_add(elapsed)
                .ok_or_else(|| anyhow!("MPD SegmentTemplate time overflow"))?;
            segments.extend(append(
                offset,
                t,
                i128::from(t) + ept_delta,
                duration,
                budget,
            )?);
        }
    } else {
        bail!("SegmentTemplate has neither a sample timeline nor a fixed duration");
    }

    // Keep cursor lookup ordered, including legacy sources that reset explicit S@t.
    segments.sort_by_key(|seg| seg.number);
    segments.dedup_by_key(|seg| seg.number);

    Ok(Representation {
        id: rep.id,
        kind,
        codecs: rep.codecs,
        bandwidth: rep.bandwidth,
        width: rep.width,
        height: rep.height,
        lang,
        timescale,
        init_url,
        manifest_kids: rep.manifest_kids,
        has_content_protection: rep.has_content_protection,
        media_template: resolve_url(&base, origin, media_template),
        start_number,
        time_addressed: is_time_template,
        presentation_time_offset: pto,
        period_duration,
        segments,
        total_duration_ts: total,
    })
}

fn duration_ticks(duration: Duration, timescale: u32) -> crate::Result<u64> {
    // Duration is nanosecond resolution; round a partial tick up so the last
    // segment still covers the whole advertised Period.
    u64::try_from((duration.as_nanos() * u128::from(timescale)).div_ceil(1_000_000_000))
        .map_err(|_| anyhow!("MPD Period duration exceeds segment time range"))
}

fn parse_attr<T: std::str::FromStr>(attrs: &Attrs, key: &str) -> crate::Result<Option<T>> {
    attrs
        .get(key)
        .map(|s| {
            s.parse()
                .map_err(|_| anyhow!("invalid MPD attribute {key}"))
        })
        .transpose()
}

fn duration_attr(attrs: &Attrs, key: &str) -> crate::Result<Option<Duration>> {
    attrs
        .get(key)
        .map(|s| parse_iso8601_duration(s).ok_or_else(|| anyhow!("invalid MPD duration {key}")))
        .transpose()
}

fn collect_attrs(e: &quick_xml::events::BytesStart) -> crate::Result<Attrs> {
    let mut m = Attrs::new();
    for a in e.attributes() {
        let a = a?;
        let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
        let v = a.unescape_value()?.into_owned();
        m.insert(k, v);
    }
    Ok(m)
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

#[derive(Clone, Copy)]
enum TemplateValue {
    Number,
    Time,
    Bandwidth,
}

enum TemplatePart<'a> {
    Literal(&'a str),
    Representation,
    Numeric(TemplateValue, NumericFormat),
}

#[derive(Clone, Copy)]
struct NumericFormat {
    width: usize,
    kind: u8,
}

impl NumericFormat {
    fn parse(spec: &str) -> crate::Result<Self> {
        let (kind, prefix) = spec
            .as_bytes()
            .split_last()
            .ok_or_else(|| anyhow!("empty SegmentTemplate numeric format"))?;
        let width = if prefix.is_empty() {
            0
        } else {
            let digits = prefix
                .strip_prefix(b"0")
                .filter(|s| !s.is_empty() && s.iter().all(u8::is_ascii_digit))
                .ok_or_else(|| anyhow!("invalid SegmentTemplate numeric width"))?;
            let width = std::str::from_utf8(digits)?.parse::<usize>()?;
            ensure!(width <= 64, "SegmentTemplate numeric width exceeds 64");
            width
        };
        ensure!(
            matches!(kind, b'd' | b'i' | b'u' | b'o' | b'x' | b'X'),
            "unsupported SegmentTemplate numeric format"
        );
        Ok(Self { width, kind: *kind })
    }

    fn write(self, out: &mut String, value: u64) {
        use std::fmt::Write;
        let width = self.width;
        let result = match self.kind {
            b'o' => write!(out, "{value:0width$o}"),
            b'x' => write!(out, "{value:0width$x}"),
            b'X' => write!(out, "{value:0width$X}"),
            _ => write!(out, "{value:0width$}"),
        };
        // std::fmt::Write on String cannot fail.
        debug_assert!(result.is_ok());
    }
}

struct CompiledTemplate<'a> {
    parts: Vec<TemplatePart<'a>>,
    uses_number: bool,
    uses_time: bool,
}

impl<'a> CompiledTemplate<'a> {
    fn parse(mut source: &'a str) -> crate::Result<Self> {
        let mut template = Self {
            parts: Vec::new(),
            uses_number: false,
            uses_time: false,
        };
        while let Some((literal, after_dollar)) = source.split_once('$') {
            if !literal.is_empty() {
                template.parts.push(TemplatePart::Literal(literal));
            }
            let (token, tail) = after_dollar
                .split_once('$')
                .ok_or_else(|| anyhow!("unterminated SegmentTemplate variable"))?;
            source = tail;
            let part = match token {
                "" => TemplatePart::Literal("$"),
                "RepresentationID" => TemplatePart::Representation,
                _ => {
                    let (name, spec) = token.split_once('%').unwrap_or((token, "d"));
                    let value = match name {
                        "Number" => {
                            template.uses_number = true;
                            TemplateValue::Number
                        }
                        "Time" => {
                            template.uses_time = true;
                            TemplateValue::Time
                        }
                        "Bandwidth" => TemplateValue::Bandwidth,
                        _ => bail!("unsupported SegmentTemplate variable"),
                    };
                    TemplatePart::Numeric(value, NumericFormat::parse(spec)?)
                }
            };
            template.parts.push(part);
        }
        if !source.is_empty() {
            template.parts.push(TemplatePart::Literal(source));
        }
        Ok(template)
    }

    fn render(&self, rep: &RepTmp, number: u64, time: u64) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Literal(s) => out.push_str(s),
                TemplatePart::Representation => out.push_str(&rep.id),
                TemplatePart::Numeric(value, format) => format.write(
                    &mut out,
                    match value {
                        TemplateValue::Number => number,
                        TemplateValue::Time => time,
                        TemplateValue::Bandwidth => rep.bandwidth,
                    },
                ),
            }
        }
        out
    }
}

fn parse_iso8601_duration(value: &str) -> Option<Duration> {
    let rest = value.trim().strip_prefix('P')?;
    let mut in_time = false;
    let mut saw_time_value = false;
    let mut unit_order = 0;
    let mut number = String::new();
    let mut total = Duration::ZERO;

    for ch in rest.chars() {
        if ch == 'T' && !in_time && number.is_empty() {
            in_time = true;
            continue;
        }
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        let (order, seconds) = match ch {
            'D' if !in_time => (1, 86_400),
            'H' if in_time => (2, 3_600),
            'M' if in_time => (3, 60),
            'S' if in_time => (4, 1),
            _ => return None,
        };
        if order <= unit_order {
            return None;
        }
        let mut nanos = 0;
        let whole = if ch == 'S' {
            if let Some((whole, fraction)) = number.split_once('.') {
                // Round to nanosecond precision; don't multiply through f64
                // when deriving a Period's segment count.
                if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let digits = fraction.len().min(9);
                nanos = fraction[..digits].parse::<u32>().ok()? * 10u32.pow((9 - digits) as u32);
                if fraction.as_bytes().get(9).is_some_and(|d| *d >= b'5') {
                    nanos += 1;
                }
                whole
            } else {
                &number
            }
        } else {
            &number
        };
        let n = whole.parse::<u64>().ok()?;
        let part = Duration::from_secs(n.checked_mul(seconds)?)
            .checked_add(Duration::from_nanos(u64::from(nanos)))?;
        total = total.checked_add(part)?;
        number.clear();
        saw_time_value |= in_time;
        unit_order = order;
    }
    (number.is_empty() && unit_order > 0 && (!in_time || saw_time_value)).then_some(total)
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
    fn applies_formatted_urls_and_escaped_dollar() {
        let mpd = r#"<MPD mediaPresentationDuration="PT4S"><Period>
          <AdaptationSet contentType="video">
            <Representation id="n" bandwidth="128">
              <SegmentTemplate timescale="100" duration="200" startNumber="7"
                initialization="init-$RepresentationID$-$Bandwidth%04X$.mp4"
                media="chunk-$$-$Number%05d$.m4s"/>
            </Representation>
            <Representation id="t">
              <SegmentTemplate timescale="100" duration="200" media="chunk-$Time%04x$.m4s"/>
            </Representation>
          </AdaptationSet></Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        let n = info.select_representation("n", TrackKind::Video).unwrap();
        assert_eq!(n.init_url, "https://cdn/init-n-0080.mp4");
        assert_eq!(n.segments[0].url, "https://cdn/chunk-$-00007.m4s");
        assert!(!n.time_addressed);
        let t = info.select_representation("t", TrackKind::Video).unwrap();
        assert_eq!(t.segments[1].number, 200);
        assert_eq!(t.segments[1].url, "https://cdn/chunk-00c8.m4s");
        assert!(t.time_addressed);
    }

    #[test]
    fn unsupported_sibling_does_not_prevent_selecting_a_playable_track() {
        let mpd = r#"<MPD mediaPresentationDuration="PT4S"><Period>
          <AdaptationSet contentType="video">
            <Representation id="bad-template">
              <SegmentTemplate duration="2" media="$Unsupported$.m4s"/>
            </Representation>
            <Representation id="bad-duration">
              <SegmentTemplate duration="two" media="$Number$.m4s"/>
            </Representation>
            <Representation id="bad-timeline">
              <SegmentTemplate media="$Number$.m4s">
                <SegmentTimeline><S d="-1"/></SegmentTimeline>
              </SegmentTemplate>
            </Representation>
            <Representation id="good">
              <SegmentTemplate duration="2" media="$Number$.m4s"/>
            </Representation>
          </AdaptationSet></Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        assert_eq!(info.representations.len(), 1);
        assert_eq!(
            info.select_representation("good", TrackKind::Video)
                .unwrap()
                .segments
                .len(),
            2
        );
        for (id, reason) in [
            ("bad-template", "unsupported SegmentTemplate variable"),
            ("bad-duration", "invalid MPD attribute duration"),
            ("bad-timeline", "invalid MPD attribute d"),
        ] {
            let err = info
                .select_representation(id, TrackKind::Video)
                .unwrap_err();
            assert!(err.to_string().contains(reason), "{err}");
        }
    }

    #[test]
    fn template_unescapes_xml_and_detects_representation_level_mime_type() {
        let mpd = r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet>
          <Representation id="v" mimeType="video/mp4">
            <SegmentTemplate duration="2" media="$Number$.m4s?track=v&amp;part=1"/>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn.test/path/index.mpd").unwrap();
        let track = info.select_representation("v", TrackKind::Video).unwrap();
        assert_eq!(
            track.segments[0].url,
            "https://cdn.test/path/1.m4s?track=v&part=1"
        );
    }

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
    fn duration_only_covers_the_short_tail_of_static_media() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT9.5S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000" initialization="v/$RepresentationID$/init.mp4" media="v/$RepresentationID$/$Number$.m4s" startNumber="100"/>
      <Representation id="pv" codecs="hev1" bandwidth="1" width="1" height="1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/live/index.mpd").unwrap();
        let r = &info.representations[0];
        assert_eq!(r.segments.len(), 5);
        assert_eq!(r.segments.first().unwrap().number, 100);
        assert_eq!(r.segments.last().unwrap().number, 104);
        assert_eq!(
            r.segments.last().unwrap().url,
            "https://cdn/live/v/pv/104.m4s"
        );
        assert_eq!(r.segments.last().unwrap().duration_ts, 1500);
        assert_eq!(r.total_duration_ts, 9_500);
    }

    #[test]
    fn duration_only_uses_period_bounds_and_time_template_addresses() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT10.5S">
  <Period start="PT1S">
    <SegmentTemplate timescale="1000" duration="2000" presentationTimeOffset="250" media="p1/$Time$.m4s"/>
    <AdaptationSet contentType="video"><Representation id="one"/></AdaptationSet>
  </Period>
  <Period start="PT4.5S" duration="PT3S">
    <AdaptationSet contentType="video">
      <SegmentTemplate timescale="1000" duration="2000" media="p2/$Number$.m4s"/>
      <Representation id="two"/>
    </AdaptationSet>
  </Period>
  <Period>
    <AdaptationSet contentType="video">
      <SegmentTemplate timescale="1000" duration="2000" media="p3/$Number$.m4s"/>
      <Representation id="three"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        let [r1, r2, r3] = info.representations.as_slice() else {
            panic!("expected three distinct period tracks");
        };
        assert_eq!(r1.segments[0].presentation_start_secs, 1.0);
        assert_eq!(r1.segments[1].url, "https://cdn/p1/2250.m4s");
        assert_eq!(r1.total_duration_ts, 3500);
        assert_eq!(r2.segments[0].presentation_start_secs, 4.5);
        assert_eq!(r2.total_duration_ts, 3000);
        assert_eq!(r3.segments[0].presentation_start_secs, 7.5);
        assert_eq!(r3.total_duration_ts, 3000);
        assert!(r1.init_url.is_empty());
    }

    #[test]
    fn template_attributes_are_inherited_without_leaking_to_siblings() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S">
  <Period><AdaptationSet contentType="video">
    <SegmentTemplate timescale="1000" duration="2000" initialization="$RepresentationID$/init.mp4"
      media="$RepresentationID$/$Number$.m4s"/>
    <Representation id="hi"><SegmentTemplate duration="4000"/></Representation>
    <Representation id="lo"/>
  </AdaptationSet></Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        let [hi, lo] = info.representations.as_slice() else {
            panic!("expected two representations");
        };
        assert_eq!(hi.init_url, "https://cdn/hi/init.mp4");
        assert_eq!(hi.segments.len(), 2);
        assert_eq!(lo.init_url, "https://cdn/lo/init.mp4");
        assert_eq!(lo.segments[3].url, "https://cdn/lo/4.m4s");
        assert_eq!(lo.segments.len(), 4);
    }

    #[test]
    fn timeline_uses_period_start_and_pto_without_resetting_the_window() {
        let mpd = r#"<MPD type="dynamic"><Period start="PT3S">
  <AdaptationSet contentType="video">
    <SegmentTemplate timescale="1000" presentationTimeOffset="1000" media="$Number$.m4s" startNumber="100">
      <SegmentTimeline><S t="500" d="2000" r="1"/><S t="5000" d="2000"/></SegmentTimeline>
    </SegmentTemplate>
    <Representation id="v"/>
  </AdaptationSet>
</Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        let r = &info.representations[0];
        assert_eq!(r.segments[0].number, 100);
        assert_eq!(r.segments[0].presentation_start_secs, 2.5);
        assert_eq!(r.segments[1].presentation_start_secs, 4.5);
        assert_eq!(r.segments[2].presentation_start_secs, 7.0);
    }

    #[test]
    fn duration_only_dynamic_snapshot_needs_a_finite_extent_without_live_availability() {
        let mpd = r#"<MPD type="dynamic" mediaPresentationDuration="PT2S">
  <Period><AdaptationSet contentType="video"><SegmentTemplate duration="1" media="$Number$.m4s"/>
    <Representation id="v"/>
  </AdaptationSet></Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        assert_eq!(info.representations[0].segments.len(), 2);
        let open = mpd.replace(r#" mediaPresentationDuration="PT2S""#, "");
        assert!(parse_mpd(&open, "https://cdn/index.mpd")
            .unwrap_err()
            .to_string()
            .contains("requires a finite Period duration"));
        let live = mpd.replace(
            "<MPD ",
            r#"<MPD availabilityStartTime="2025-01-01T00:00:00Z" "#,
        );
        assert!(parse_mpd(&live, "https://cdn/index.mpd")
            .unwrap_err()
            .to_string()
            .contains("availabilityStartTime is not supported"));
    }

    #[test]
    fn rejects_segment_expansion_beyond_the_budget() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT20001S">
  <Period><AdaptationSet contentType="video">
    <SegmentTemplate duration="1" media="$Number$.m4s"/><Representation id="v"/>
  </AdaptationSet></Period>
</MPD>"#;
        let timeline = mpd.replace(r#"media="$Number$.m4s"/>"#, r#"media="$Number$.m4s"><SegmentTimeline><S d="1" r="20000"/></SegmentTimeline></SegmentTemplate>"#);
        for text in [mpd, timeline.as_str()] {
            assert!(parse_mpd(text, "https://cdn/index.mpd")
                .unwrap_err()
                .to_string()
                .contains("segments per representation"));
        }
        let mut budget = ExpansionBudget {
            segments: MAX_MPD_SEGMENTS - 2,
            ..Default::default()
        };
        assert!(budget.reserve(0, 2).is_ok());
        assert!(budget
            .reserve(2, 1)
            .unwrap_err()
            .to_string()
            .contains("total segments"));
    }

    #[test]
    fn rejects_segment_number_without_room_for_the_next_cursor() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT1S">
  <Period><AdaptationSet contentType="video">
    <SegmentTemplate duration="1" media="$Number$.m4s" startNumber="18446744073709551615"/>
    <Representation id="v"/>
  </AdaptationSet></Period>
</MPD>"#;
        assert!(parse_mpd(mpd, "https://cdn/index.mpd")
            .unwrap_err()
            .to_string()
            .contains("number exceeds cursor range"));
    }

    #[test]
    fn repeat_to_next_explicit_t_is_bounded() {
        let mpd = r#"<MPD type="static"><Period><AdaptationSet contentType="video">
    <SegmentTemplate media="$Time$.m4s">
      <SegmentTimeline><S t="3" d="2" r="-1"/><S t="9" d="2"/></SegmentTimeline>
    </SegmentTemplate><Representation id="v"/>
  </AdaptationSet></Period></MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        let times: Vec<_> = info.representations[0]
            .segments
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(times, vec![3, 5, 7, 9]);
    }

    #[test]
    fn content_protection_without_a_kid_is_scoped_to_the_track() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT2S">
  <Period><AdaptationSet contentType="video">
    <SegmentTemplate duration="1" media="$Number$.m4s"/>
    <Representation id="protected"><ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"/></Representation>
    <Representation id="clear"/>
  </AdaptationSet><AdaptationSet contentType="audio">
    <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"/>
    <SegmentTemplate duration="1" media="$Number$.m4s"/>
    <Representation id="audio"/>
  </AdaptationSet></Period>
</MPD>"#;
        let info = parse_mpd(mpd, "https://cdn/index.mpd").unwrap();
        let [protected, clear, audio] = info.representations.as_slice() else {
            panic!("expected three representations");
        };
        assert!(protected.has_content_protection);
        assert!(audio.has_content_protection);
        assert!(protected.manifest_kids.is_empty());
        assert!(!clear.has_content_protection);
    }

    #[test]
    fn iso8601_duration_supports_precise_fractional_seconds() {
        assert_eq!(
            parse_iso8601_duration("PT9.5S"),
            Some(Duration::from_millis(9500))
        );
        assert_eq!(
            parse_iso8601_duration("PT1H2M3.5S"),
            Some(Duration::from_millis(3723500))
        );
        assert_eq!(
            parse_iso8601_duration("P1DT2H"),
            Some(Duration::from_secs(93_600))
        );
        assert_eq!(parse_iso8601_duration("PT0S"), Some(Duration::ZERO));
        assert_eq!(parse_iso8601_duration("P1DT"), None);
        assert_eq!(
            duration_ticks(parse_iso8601_duration("PT1.1S").unwrap(), 10).unwrap(),
            11
        );
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
