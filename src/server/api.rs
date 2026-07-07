//! REST API handlers。

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::hls::{
    looks_like_hls, parse_playlist, HlsMaster, HlsMediaPlaylist, HlsPlaylist, HlsRendition,
    HlsVariant,
};
use crate::mpd::{parse_mpd, Representation, TrackKind};
use crate::segment::{HlsOutput, SubtitleOutput};
use crate::task::hls_pipeline::run_hls_pipeline;
use crate::task::manager::{parking_lot_lite, Task};
use crate::task::pipeline::run_pipeline;
use crate::task::{
    live_tuning, manager::on_demand_idle_timeout_secs, KeyMode, RunMode, SourceKind, TaskState,
    TrackSelection,
};

use super::AppState;

pub async fn index() -> Html<&'static str> {
    Html(include_str!("../frontend/index.html"))
}

// ── /api/parse ──────────────────────────────────────────
#[derive(Deserialize)]
pub struct ParseReq {
    pub mpd: String,
}

#[derive(Serialize)]
pub struct TrackOut {
    pub id: String,
    pub kind: String,
    pub codecs: String,
    pub bandwidth: u64,
    pub resolution: String,
    pub lang: String,
    pub segments: usize,
}

#[derive(Serialize)]
pub struct ParseResp {
    pub is_dynamic: bool,
    pub video: Vec<TrackOut>,
    pub audio: Vec<TrackOut>,
    pub subtitles: Vec<TrackOut>,
}

pub async fn parse(
    State(st): State<Arc<AppState>>,
    Json(req): Json<ParseReq>,
) -> Result<Json<ParseResp>, (StatusCode, String)> {
    let text = st
        .mgr
        .downloader
        .get_text(&req.mpd)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("fetch manifest: {e:#}")))?;
    if looks_like_hls(&text) {
        return parse_hls_response(&st.mgr.downloader, &req.mpd, &text).await;
    }

    let info =
        parse_mpd(&text, &req.mpd).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;

    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    for r in &info.representations {
        let out = TrackOut {
            id: r.id.clone(),
            kind: format!("{:?}", r.kind),
            codecs: r.codecs.clone(),
            bandwidth: r.bandwidth,
            resolution: if r.width > 0 {
                format!("{}x{}", r.width, r.height)
            } else {
                String::new()
            },
            lang: r.lang.clone(),
            segments: r.segments.len(),
        };
        match r.kind {
            TrackKind::Video => video.push(out),
            TrackKind::Audio => audio.push(out),
            TrackKind::Subtitle => subtitles.push(out),
            TrackKind::Other => {}
        }
    }
    // 视频按码率降序
    video.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
    Ok(Json(ParseResp {
        is_dynamic: info.is_dynamic,
        video,
        audio,
        subtitles,
    }))
}

// ── /api/tasks POST (create) ────────────────────────────
#[derive(Deserialize)]
pub struct CreateReq {
    #[serde(default)]
    pub name: String,
    pub mpd: String,
    /// 多行 "KID:KEY"，按 KID 自动匹配视频/音频。
    #[serde(default)]
    pub keys: String,
    #[serde(default)]
    pub key_mode: KeyMode,
    #[serde(default)]
    pub run_mode: RunMode,
    pub video_rep_id: String,
    pub audio_rep_id: Option<String>,
    #[serde(default)]
    pub enable_subtitles: bool,
    pub subtitle_rep_id: Option<String>,
    #[serde(default = "default_window")]
    pub window: usize,
    #[serde(default = "default_target_dur")]
    pub target_duration: u64,
}
fn default_window() -> usize {
    6
}
fn default_target_dur() -> u64 {
    7
}

fn normalize_task_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        "未命名任务".to_string()
    } else {
        name.chars().take(80).collect()
    }
}

fn live_publish_delay_segments(is_live: bool, segment_duration_secs: f64) -> usize {
    live_tuning::publish_delay_segments(
        is_live,
        live_tuning::is_short_segment(segment_duration_secs),
    )
}

fn mpd_segment_duration_secs(rep: &Representation) -> f64 {
    rep.segments
        .last()
        .or_else(|| rep.segments.first())
        .map(|s| s.duration_ts as f64 / rep.timescale.max(1) as f64)
        .unwrap_or(0.0)
}

fn hls_segment_duration_secs(media: &HlsMediaPlaylist) -> f64 {
    media
        .segments
        .last()
        .or_else(|| media.segments.first())
        .map(|s| s.duration)
        .unwrap_or(media.target_duration as f64)
}

#[derive(Serialize)]
pub struct CreateResp {
    pub id: String,
    pub playlist_url: String,
}

pub async fn create_task(
    State(st): State<Arc<AppState>>,
    Json(req): Json<CreateReq>,
) -> Result<Json<CreateResp>, (StatusCode, String)> {
    // 先解析以拿到该视频轨的元数据（codecs/分辨率/带宽）+ 判断 live
    let text = st
        .mgr
        .downloader
        .get_text(&req.mpd)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("fetch manifest: {e:#}")))?;
    if looks_like_hls(&text) {
        return create_hls_task(st, req, text).await;
    }

    let info =
        parse_mpd(&text, &req.mpd).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let vrep = info
        .representations
        .iter()
        .find(|r| r.id == req.video_rep_id && r.kind == TrackKind::Video)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "video rep not found".into()))?;
    let arep = req.audio_rep_id.as_ref().and_then(|aid| {
        info.representations
            .iter()
            .find(|r| &r.id == aid && r.kind == TrackKind::Audio)
    });
    let srep = select_mpd_subtitle_rep(
        &info.representations,
        req.enable_subtitles,
        req.subtitle_rep_id.as_deref(),
    )?;

    let mut codecs = vrep.codecs.clone();
    if let Some(a) = arep {
        codecs = format!("{},{}", codecs, a.codecs);
    }

    // 粗判 VIDEO-RANGE（master playlist 用）：DV/HEVC 默认 PQ 倾向，H.264 默认 SDR。
    // 精确的 SDR/HDR10/HLG 由播放器读码流 VUI 自适应；这里给合理默认。
    let vr = {
        let c = vrep.codecs.to_lowercase();
        if c.starts_with("dvh") || c.starts_with("dvhe") {
            "PQ"
        } else {
            "SDR"
        }
    };

    let id = Uuid::new_v4().to_string();
    let task_name = normalize_task_name(&req.name);
    // 点播 / 直播分流：直播用滚动窗口贴 live edge；点播必须保留全部段（window=0）。
    // 否则点播会被瞬时全速下完的段不断 GC 掉，播放器只剩尾部 window 段、且 MEDIA-SEQUENCE
    // 已飙高 → 拉 seq 0.. 全部 404，表现为“断片”。
    let window = if info.is_dynamic { req.window } else { 0 };
    let publish_delay =
        live_publish_delay_segments(info.is_dynamic, mpd_segment_duration_secs(vrep));
    let hls = Arc::new(HlsOutput::new(
        window,
        req.target_duration,
        codecs.clone(),
        vr.to_string(),
        info.is_dynamic,
        format!("/p/{id}"),
        publish_delay,
    ));
    let subtitles = Arc::new(SubtitleOutput::new(
        window,
        req.target_duration,
        info.is_dynamic,
        publish_delay,
    ));

    let sel = TrackSelection {
        mpd_url: req.mpd.clone(),
        source_kind: SourceKind::Mpd,
        keys: req.keys,
        key_mode: req.key_mode,
        video_rep_id: req.video_rep_id.clone(),
        audio_rep_id: req.audio_rep_id.clone(),
        enable_subtitles: srep.is_some(),
        subtitle_rep_id: srep.map(|r| r.id.clone()),
    };

    let task = Arc::new(Task {
        id: id.clone(),
        name: task_name,
        run_mode: req.run_mode,
        mpd_url: req.mpd.clone(),
        video_rep_id: req.video_rep_id.clone(),
        audio_rep_id: req.audio_rep_id.clone(),
        subtitles_enabled: sel.enable_subtitles,
        subtitle_rep_id: sel.subtitle_rep_id.clone(),
        subtitle_lang: srep
            .map(|r| subtitle_label(&r.lang, &r.id))
            .unwrap_or_else(String::new),
        source_kind: SourceKind::Mpd,
        codecs,
        width: vrep.width,
        height: vrep.height,
        bandwidth: vrep.bandwidth,
        is_dynamic: info.is_dynamic,
        state: std::sync::atomic::AtomicU8::new(0),
        error_msg: parking_lot_lite::Mutex::new(String::new()),
        stage: parking_lot_lite::Mutex::new("已创建".into()),
        segments_done: AtomicU64::new(0),
        bytes_done: AtomicU64::new(0),
        total_segments: AtomicU64::new(vrep.segments.len() as u64),
        resume_number: AtomicU64::new(0),
        acc_v_dts: AtomicU64::new(0),
        acc_a_dts: AtomicU64::new(0),
        origin_v_tfdt: AtomicU64::new(u64::MAX),
        origin_a_tfdt: AtomicU64::new(u64::MAX),
        hls,
        subtitles,
        fetch_tuning: crate::task::fetch_tuning::AdaptiveFetchTuning::new(),
        cancel: parking_lot_lite::Mutex::new(CancellationToken::new()),
        paused: std::sync::atomic::AtomicBool::new(false),
        last_playback_request_secs: AtomicU64::new(0),
        idle_timeout_secs: on_demand_idle_timeout_secs(),
        on_demand_auto_paused: std::sync::atomic::AtomicBool::new(false),
        sel,
    });
    st.mgr.insert(task.clone());
    if req.run_mode.is_on_demand() {
        st.mgr.arm_on_demand_task(&id);
    } else {
        task.set_state(TaskState::Parsing);
        let dl = st.mgr.downloader.clone();
        tokio::spawn(run_pipeline(task.clone(), dl));
    }

    Ok(Json(CreateResp {
        id: id.clone(),
        playlist_url: format!("/p/{id}"),
    }))
}

async fn parse_hls_response(
    dl: &crate::fetch::SharedDownloader,
    source_url: &str,
    text: &str,
) -> Result<Json<ParseResp>, (StatusCode, String)> {
    let playlist = parse_playlist(text, source_url)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("parse hls: {e:#}")))?;

    match playlist {
        HlsPlaylist::Media(media) => Ok(Json(ParseResp {
            is_dynamic: media.is_live(),
            video: vec![TrackOut {
                id: "hls-0".to_string(),
                kind: "Video".to_string(),
                codecs: "mpegts".to_string(),
                bandwidth: 0,
                resolution: String::new(),
                lang: String::new(),
                segments: media.segments.len(),
            }],
            audio: Vec::new(),
            subtitles: Vec::new(),
        })),
        HlsPlaylist::Master(master) => {
            let mut any_live = false;
            let mut saw_media = false;
            let mut video = Vec::new();
            let audio = audio_tracks_from_hls_master(dl, &master).await;
            let subtitles = subtitle_tracks_from_hls_master(dl, &master).await;

            for variant in &master.variants {
                let mut segments = 0usize;
                if let Ok(media_text) = dl.get_text(&variant.uri).await {
                    if let Ok(HlsPlaylist::Media(media)) = parse_playlist(&media_text, &variant.uri)
                    {
                        segments = media.segments.len();
                        any_live |= media.is_live();
                        saw_media = true;
                    }
                }
                video.push(track_from_hls_variant(&variant, segments));
            }
            video.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));

            Ok(Json(ParseResp {
                // 如果 media playlist 抓取失败，按直播处理，避免前端把 master 误看作已完结 VOD。
                is_dynamic: if saw_media { any_live } else { true },
                video,
                audio,
                subtitles,
            }))
        }
    }
}

fn track_from_hls_variant(v: &HlsVariant, segments: usize) -> TrackOut {
    TrackOut {
        id: v.id.clone(),
        kind: "Video".to_string(),
        codecs: if v.codecs.is_empty() {
            "mpegts".to_string()
        } else {
            v.codecs.clone()
        },
        bandwidth: v.bandwidth,
        resolution: if v.width > 0 {
            format!("{}x{}", v.width, v.height)
        } else {
            String::new()
        },
        lang: String::new(),
        segments,
    }
}

async fn audio_tracks_from_hls_master(
    dl: &crate::fetch::SharedDownloader,
    master: &HlsMaster,
) -> Vec<TrackOut> {
    futures::future::join_all(
        master
            .audio
            .iter()
            .map(|r| track_from_hls_rendition(dl, master, r)),
    )
    .await
}

async fn subtitle_tracks_from_hls_master(
    dl: &crate::fetch::SharedDownloader,
    master: &HlsMaster,
) -> Vec<TrackOut> {
    futures::future::join_all(
        master
            .subtitles
            .iter()
            .map(|r| track_from_hls_subtitle_rendition(dl, r)),
    )
    .await
}

async fn track_from_hls_subtitle_rendition(
    dl: &crate::fetch::SharedDownloader,
    r: &HlsRendition,
) -> TrackOut {
    let mut segments = 0usize;
    let mut codecs = "webvtt".to_string();
    if let Ok(text) = dl.get_text(&r.uri).await {
        if let Ok(HlsPlaylist::Media(media)) = parse_playlist(&text, &r.uri) {
            segments = media.segments.len();
            if media.has_map {
                codecs = "fmp4-subtitle".to_string();
            }
        }
    }
    TrackOut {
        id: r.id.clone(),
        kind: "Subtitle".to_string(),
        codecs,
        bandwidth: 0,
        resolution: String::new(),
        lang: subtitle_label(&r.language, &r.name),
        segments,
    }
}

async fn track_from_hls_rendition(
    dl: &crate::fetch::SharedDownloader,
    master: &HlsMaster,
    r: &HlsRendition,
) -> TrackOut {
    let codec = master
        .variants
        .iter()
        .find(|v| v.audio_group.as_deref() == Some(r.group_id.as_str()))
        .and_then(|v| audio_codec_from_codecs(&v.codecs))
        .unwrap_or_else(|| "audio".to_string());
    let channels = if r.channels.trim().is_empty() {
        String::new()
    } else {
        format!(" {}ch", r.channels.trim())
    };
    let lang = if r.language.trim().is_empty() {
        r.name.clone()
    } else {
        r.language.clone()
    };
    let bandwidth = probe_hls_audio_bitrate(dl, r).await.unwrap_or(0);

    TrackOut {
        id: r.id.clone(),
        kind: "Audio".to_string(),
        codecs: format!("{codec}{channels}"),
        bandwidth,
        resolution: String::new(),
        lang,
        segments: 0,
    }
}

async fn probe_hls_audio_bitrate(
    dl: &crate::fetch::SharedDownloader,
    r: &HlsRendition,
) -> Option<u64> {
    let text = dl.get_text(&r.uri).await.ok()?;
    let media = match parse_playlist(&text, &r.uri).ok()? {
        HlsPlaylist::Media(media) => media,
        HlsPlaylist::Master(_) => return None,
    };
    let segment = media
        .segments
        .iter()
        .rev()
        .find(|s| s.duration.is_finite() && s.duration > 0.0)?;
    let bytes = dl.get(&segment.uri).await.ok()?;
    Some(((bytes.len() as f64 * 8.0) / segment.duration).round() as u64)
}

fn audio_codec_from_codecs(codecs: &str) -> Option<String> {
    codecs.split(',').map(str::trim).find_map(|codec| {
        let lower = codec.to_ascii_lowercase();
        if lower.starts_with("mp4a")
            || lower.starts_with("ac-3")
            || lower.starts_with("ec-3")
            || lower.starts_with("ac-4")
        {
            Some(codec.to_string())
        } else {
            None
        }
    })
}

async fn create_hls_task(
    st: Arc<AppState>,
    req: CreateReq,
    text: String,
) -> Result<Json<CreateResp>, (StatusCode, String)> {
    let (variant, media) =
        select_hls_media_playlist(&st.mgr.downloader, &req.mpd, &req.video_rep_id, &text).await?;
    let subtitle_rendition = select_hls_subtitle_rendition(
        &text,
        &req.mpd,
        &variant,
        req.enable_subtitles,
        req.subtitle_rep_id.as_deref(),
    )?;
    let codecs = if variant.codecs.is_empty() {
        "mpegts".to_string()
    } else {
        variant.codecs.clone()
    };
    let is_dynamic = media.is_live();
    let window = if is_dynamic { req.window } else { 0 };
    let id = Uuid::new_v4().to_string();
    let task_name = normalize_task_name(&req.name);
    let publish_delay = live_publish_delay_segments(is_dynamic, hls_segment_duration_secs(&media));
    let hls = Arc::new(HlsOutput::new(
        window,
        req.target_duration.max(media.target_duration),
        codecs.clone(),
        variant.video_range.clone(),
        is_dynamic,
        format!("/p/{id}"),
        publish_delay,
    ));
    let subtitles = Arc::new(SubtitleOutput::new(
        window,
        req.target_duration.max(media.target_duration),
        is_dynamic,
        publish_delay,
    ));

    let sel = TrackSelection {
        mpd_url: req.mpd.clone(),
        source_kind: SourceKind::Hls,
        keys: req.keys,
        key_mode: req.key_mode,
        video_rep_id: variant.id.clone(),
        audio_rep_id: req.audio_rep_id.clone(),
        enable_subtitles: subtitle_rendition.is_some(),
        subtitle_rep_id: subtitle_rendition.as_ref().map(|r| r.id.clone()),
    };

    let task = Arc::new(Task {
        id: id.clone(),
        name: task_name,
        run_mode: req.run_mode,
        mpd_url: req.mpd,
        video_rep_id: variant.id,
        audio_rep_id: req.audio_rep_id,
        subtitles_enabled: sel.enable_subtitles,
        subtitle_rep_id: sel.subtitle_rep_id.clone(),
        subtitle_lang: subtitle_rendition
            .as_ref()
            .map(|r| subtitle_label(&r.language, &r.name))
            .unwrap_or_default(),
        source_kind: SourceKind::Hls,
        codecs,
        width: variant.width,
        height: variant.height,
        bandwidth: variant.bandwidth,
        is_dynamic,
        state: std::sync::atomic::AtomicU8::new(0),
        error_msg: parking_lot_lite::Mutex::new(String::new()),
        stage: parking_lot_lite::Mutex::new("已创建".into()),
        segments_done: AtomicU64::new(0),
        bytes_done: AtomicU64::new(0),
        total_segments: AtomicU64::new(media.segments.len() as u64),
        resume_number: AtomicU64::new(0),
        acc_v_dts: AtomicU64::new(0),
        acc_a_dts: AtomicU64::new(0),
        origin_v_tfdt: AtomicU64::new(u64::MAX),
        origin_a_tfdt: AtomicU64::new(u64::MAX),
        hls,
        subtitles,
        fetch_tuning: crate::task::fetch_tuning::AdaptiveFetchTuning::new(),
        cancel: parking_lot_lite::Mutex::new(CancellationToken::new()),
        paused: std::sync::atomic::AtomicBool::new(false),
        last_playback_request_secs: AtomicU64::new(0),
        idle_timeout_secs: on_demand_idle_timeout_secs(),
        on_demand_auto_paused: std::sync::atomic::AtomicBool::new(false),
        sel,
    });
    st.mgr.insert(task.clone());
    if req.run_mode.is_on_demand() {
        st.mgr.arm_on_demand_task(&id);
    } else {
        task.set_state(TaskState::Parsing);
        let dl = st.mgr.downloader.clone();
        tokio::spawn(run_hls_pipeline(task.clone(), dl));
    }

    Ok(Json(CreateResp {
        id: id.clone(),
        playlist_url: format!("/p/{id}"),
    }))
}

async fn select_hls_media_playlist(
    dl: &crate::fetch::SharedDownloader,
    source_url: &str,
    selected_id: &str,
    text: &str,
) -> Result<(HlsVariant, HlsMediaPlaylist), (StatusCode, String)> {
    match parse_playlist(text, source_url)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("parse hls: {e:#}")))?
    {
        HlsPlaylist::Media(media) => {
            if !selected_id.is_empty() && selected_id != "hls-0" {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "HLS media playlist track id must be hls-0".into(),
                ));
            }
            Ok((
                HlsVariant {
                    id: "hls-0".to_string(),
                    uri: source_url.to_string(),
                    audio_group: None,
                    subtitles_group: None,
                    bandwidth: 0,
                    codecs: "mpegts".to_string(),
                    width: 0,
                    height: 0,
                    video_range: "SDR".to_string(),
                },
                media,
            ))
        }
        HlsPlaylist::Master(master) => {
            let variant = master
                .variants
                .into_iter()
                .find(|v| v.id == selected_id)
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("HLS variant {selected_id} not found"),
                    )
                })?;
            let media_text = dl.get_text(&variant.uri).await.map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("fetch HLS media playlist: {e:#}"),
                )
            })?;
            match parse_playlist(&media_text, &variant.uri).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("parse HLS media playlist: {e:#}"),
                )
            })? {
                HlsPlaylist::Media(media) => Ok((variant, media)),
                HlsPlaylist::Master(_) => Err((
                    StatusCode::BAD_REQUEST,
                    "nested HLS master playlists are not supported".into(),
                )),
            }
        }
    }
}

fn select_mpd_subtitle_rep<'a>(
    reps: &'a [Representation],
    enabled: bool,
    selected_id: Option<&str>,
) -> Result<Option<&'a Representation>, (StatusCode, String)> {
    if !enabled {
        return Ok(None);
    }
    if let Some(id) = selected_id.filter(|id| !id.trim().is_empty()) {
        return reps
            .iter()
            .find(|r| r.id == id && r.kind == TrackKind::Subtitle)
            .map(Some)
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("subtitle rep {id} not found"),
                )
            });
    }
    reps.iter()
        .find(|r| r.kind == TrackKind::Subtitle)
        .map(Some)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "no subtitle track found".into()))
}

fn select_hls_subtitle_rendition(
    source_text: &str,
    source_url: &str,
    variant: &HlsVariant,
    enabled: bool,
    selected_id: Option<&str>,
) -> Result<Option<HlsRendition>, (StatusCode, String)> {
    if !enabled {
        return Ok(None);
    }
    let master = match parse_playlist(source_text, source_url)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("parse hls: {e:#}")))?
    {
        HlsPlaylist::Master(master) => master,
        HlsPlaylist::Media(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "HLS media playlist has no subtitle renditions".into(),
            ))
        }
    };
    let group = variant.subtitles_group.as_deref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "selected HLS variant has no SUBTITLES group".into(),
        )
    })?;
    if let Some(id) = selected_id.filter(|id| !id.trim().is_empty()) {
        let selected = master
            .subtitles
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("HLS subtitle rendition {id} not found"),
                )
            })?;
        if selected.group_id != group {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "HLS subtitle rendition {id} belongs to group {}, but selected video requires group {group}",
                    selected.group_id
                ),
            ));
        }
        return Ok(Some(selected));
    }

    master
        .subtitles
        .iter()
        .find(|r| r.group_id == group && r.is_default)
        .or_else(|| master.subtitles.iter().find(|r| r.group_id == group))
        .cloned()
        .map(Some)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("no subtitle rendition found for group {group}"),
            )
        })
}

fn subtitle_label(lang: &str, fallback: &str) -> String {
    if !lang.trim().is_empty() {
        lang.trim().to_string()
    } else if !fallback.trim().is_empty() {
        fallback.trim().to_string()
    } else {
        "subtitles".to_string()
    }
}

// ── /api/tasks GET (list) ───────────────────────────────
pub async fn list_tasks(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.mgr.list())
}

// ── 控制：stop / pause / start(resume) / delete ─────────
pub async fn stop_task(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if st.mgr.stop(&id) {
        (StatusCode::OK, "stopped")
    } else {
        (StatusCode::NOT_FOUND, "not found")
    }
}

pub async fn pause_task(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if st.mgr.pause(&id) {
        (StatusCode::OK, "paused")
    } else {
        (StatusCode::CONFLICT, "cannot pause (not running)")
    }
}

pub async fn start_task(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if st.mgr.start(&id) {
        (StatusCode::OK, "started")
    } else {
        (
            StatusCode::CONFLICT,
            "cannot start (already running or not found)",
        )
    }
}

pub async fn delete_task(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(t) = st.mgr.remove(&id) {
        t.cancel.lock().cancel();
        (StatusCode::OK, "deleted")
    } else {
        (StatusCode::NOT_FOUND, "not found")
    }
}
