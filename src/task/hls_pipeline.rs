//! HLS input pipeline:
//! - TS media playlist: segment download → optional AES-128-CBC decrypt → publish.
//! - fMP4/CMAF SAMPLE-AES-CTR: CENC decrypt → MP4 samples → TS mux.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::StreamExt;

use crate::clock::ClockState;
use crate::crypto::cenc::{parse_key_hex, Decryptor, KeyStore};
use crate::crypto::hls::{decrypt_aes128_cbc_pkcs7, iv_from_media_sequence};
use crate::crypto::playready::kid_from_playready_data_uri;
use crate::hevc::annexb::AccessUnit;
use crate::hls::{
    parse_playlist, HlsMediaPlaylist, HlsPlaylist, HlsRendition, HlsSegment, HlsSegmentKey,
};
use crate::mp4::sample::{parse_media_segment, parse_media_segment_with_default_iv_size};
use crate::mp4::{DoviConfig, ParamSets, TrackEncryption};
use crate::subtitle::SubtitleAccumulator;
use crate::ts::muxer::{AudioUnit, TsMuxer};
use crate::ts::{AudioCodec, VideoCodec};

use super::{key_resolver, live_tuning, manager::Task};

const SHORT_SEG_THRESHOLD_S: f64 = live_tuning::SHORT_SEG_THRESHOLD_S;
const AGGREGATE_TARGET_S: f64 = live_tuning::AGGREGATE_TARGET_S;

pub async fn run_hls_pipeline(task: Arc<Task>, dl: crate::fetch::SharedDownloader) {
    let cancel = task.cancel_token();
    task.set_paused(false);
    task.set_state(super::TaskState::Running);
    let sel = task.sel.clone();

    if let Err(e) = run_inner(&task, &dl, &sel).await {
        if cancel.is_cancelled() {
            finalize_cancelled(&task);
        } else {
            *task.error_msg.lock() = format!("{e:#}");
            task.set_stage("出错");
            task.set_state(super::TaskState::Error);
            tracing::error!(task = %task.id, "hls pipeline error: {e:#}");
        }
        return;
    }

    if cancel.is_cancelled() {
        finalize_cancelled(&task);
    } else {
        task.hls.finish();
        task.subtitles.finish();
        task.set_stage("已完成");
        task.set_state(super::TaskState::Finished);
    }
}

fn finalize_cancelled(task: &Arc<Task>) {
    if task.is_paused() {
        if task.is_on_demand_auto_paused() {
            task.set_stage("按需空闲暂停");
        } else {
            task.set_stage("已暂停");
        }
        task.set_state(super::TaskState::Paused);
    } else {
        task.set_stage("已停止");
        task.set_state(super::TaskState::Stopped);
    }
}

async fn run_inner(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    sel: &super::TrackSelection,
) -> crate::Result<()> {
    task.set_stage("解析 M3U8");
    let source_text = dl.get_text(&sel.mpd_url).await?;
    let selected = load_selected_media_playlist(
        &source_text,
        &sel.mpd_url,
        &sel.video_rep_id,
        sel.audio_rep_id.as_deref(),
        sel.enable_subtitles,
        sel.subtitle_rep_id.as_deref(),
        dl,
    )
    .await?;

    if selected.video.has_map {
        run_fmp4_inner(task, dl, sel, selected).await
    } else {
        run_ts_inner(task, dl, &sel.keys, selected).await
    }
}

async fn run_ts_inner(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    keys: &str,
    mut selected: SelectedHlsInput,
) -> crate::Result<()> {
    task.total_segments
        .store(selected.video.segments.len() as u64, Ordering::Relaxed);
    let mut ctx = TsContext {
        configured_key: first_configured_key(keys),
        key_cache: HashMap::new(),
        agg_data: Vec::new(),
        agg_duration: 0.0,
        subtitle_acc: SubtitleAccumulator::new(),
        pending_discontinuity: false,
        last_key_marker: None,
    };
    let publish_delay = configure_live_publish_delay(task, &selected.video);
    let mut next_sequence = initial_sequence(task, &selected.video, publish_delay);
    let mut seen_uris = initial_seen_uris(&selected.video, next_sequence);
    let cancel = task.cancel_token();

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let cycle_started = std::time::Instant::now();

        task.set_stage("处理 HLS 分片");
        let jobs: Vec<HlsTsJob> = selected
            .video
            .segments
            .iter()
            .filter(|seg| seg.sequence >= next_sequence || !seen_uris.contains(&seg.uri))
            .filter(|seg| !seen_uris.contains(&seg.uri))
            .cloned()
            .map(|video| {
                let subtitle = selected
                    .subtitle
                    .as_ref()
                    .and_then(|playlist| subtitle_segment_for_hls_video(&video, playlist))
                    .cloned();
                HlsTsJob { video, subtitle }
            })
            .collect();

        let mut skipped_live_gap = false;
        if !jobs.is_empty() {
            let progress =
                process_ts_batch(task, dl, jobs, &mut ctx, selected.video.is_live()).await?;
            next_sequence = progress.next_sequence;
            if let Some(skip) = progress.skipped_failure {
                mark_ts_live_gap_boundary(
                    task,
                    &mut ctx,
                    &format!("HLS TS live segment {} failed", skip.sequence),
                );
                next_sequence = skip.skip_to;
                skipped_live_gap = true;
            }
            task.resume_number.store(next_sequence, Ordering::Relaxed);
            for seg in selected
                .video
                .segments
                .iter()
                .filter(|s| s.sequence < next_sequence)
            {
                seen_uris.insert(seg.uri.clone());
            }
        }

        if !selected.video.is_live() {
            break;
        }

        if skipped_live_gap
            && has_unseen_hls_segment_at_or_after(&selected.video, &seen_uris, next_sequence)
        {
            continue;
        }

        task.set_stage("等待 HLS 更新");
        let wait = live_tuning::hls_refresh_wait(
            media_segment_duration_secs(&selected.video),
            cycle_started.elapsed(),
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }

        match refresh_selected_input(dl, &selected).await {
            Ok(fresh) => {
                if hls_media_sequence_rewound(&selected.video, &fresh.video) {
                    flush_ts_aggregate(task, &mut ctx);
                    ctx.pending_discontinuity = true;
                    seen_uris.clear();
                    next_sequence = fresh
                        .video
                        .segments
                        .first()
                        .map(|s| s.sequence)
                        .unwrap_or(fresh.video.media_sequence);
                    task.resume_number.store(next_sequence, Ordering::Relaxed);
                }
                task.total_segments
                    .store(fresh.video.segments.len() as u64, Ordering::Relaxed);
                selected = fresh;
            }
            Err(e) => tracing::warn!(task=%task.id, "refresh hls playlist failed: {e:#}"),
        }
    }

    flush_ts_aggregate(task, &mut ctx);
    Ok(())
}

struct SelectedHlsInput {
    video_url: String,
    video: HlsMediaPlaylist,
    audio_url: Option<String>,
    audio: Option<HlsMediaPlaylist>,
    subtitle_url: Option<String>,
    subtitle: Option<HlsMediaPlaylist>,
}

async fn load_selected_media_playlist(
    source_text: &str,
    source_url: &str,
    selected_id: &str,
    selected_audio_id: Option<&str>,
    enable_subtitles: bool,
    selected_subtitle_id: Option<&str>,
    dl: &crate::fetch::SharedDownloader,
) -> crate::Result<SelectedHlsInput> {
    match parse_playlist(source_text, source_url)? {
        HlsPlaylist::Media(media) => Ok(SelectedHlsInput {
            video_url: source_url.to_string(),
            video: media,
            audio_url: None,
            audio: None,
            subtitle_url: None,
            subtitle: None,
        }),
        HlsPlaylist::Master(master) => {
            let variant = master
                .variants
                .iter()
                .find(|v| v.id == selected_id)
                .ok_or_else(|| anyhow::anyhow!("HLS variant {selected_id} not found"))?;
            let video_text = dl.get_text(&variant.uri).await?;
            let video = match parse_playlist(&video_text, &variant.uri)? {
                HlsPlaylist::Media(media) => media,
                HlsPlaylist::Master(_) => Err(anyhow::anyhow!(
                    "nested HLS master playlists are not supported"
                ))?,
            };

            let audio_rendition = select_audio_rendition(
                &master.audio,
                variant.audio_group.as_deref(),
                selected_audio_id,
            )?;
            let subtitle_rendition = select_subtitle_rendition(
                &master.subtitles,
                variant.subtitles_group.as_deref(),
                enable_subtitles,
                selected_subtitle_id,
            )?;
            let (audio_url, audio) = match audio_rendition {
                Some(r) => {
                    let text = dl.get_text(&r.uri).await?;
                    let media = match parse_playlist(&text, &r.uri)? {
                        HlsPlaylist::Media(media) => media,
                        HlsPlaylist::Master(_) => {
                            return Err(anyhow::anyhow!(
                                "nested HLS audio master playlists are not supported"
                            ))
                        }
                    };
                    (Some(r.uri.clone()), Some(media))
                }
                None => (None, None),
            };
            let (subtitle_url, subtitle) = match subtitle_rendition {
                Some(r) => {
                    let text = dl.get_text(&r.uri).await?;
                    let media = match parse_playlist(&text, &r.uri)? {
                        HlsPlaylist::Media(media) => media,
                        HlsPlaylist::Master(_) => {
                            return Err(anyhow::anyhow!(
                                "nested HLS subtitle master playlists are not supported"
                            ))
                        }
                    };
                    (Some(r.uri.clone()), Some(media))
                }
                None => (None, None),
            };

            Ok(SelectedHlsInput {
                video_url: variant.uri.clone(),
                video,
                audio_url,
                audio,
                subtitle_url,
                subtitle,
            })
        }
    }
}

fn select_audio_rendition<'a>(
    audio: &'a [HlsRendition],
    group: Option<&str>,
    selected_audio_id: Option<&str>,
) -> crate::Result<Option<&'a HlsRendition>> {
    let Some(group) = group else {
        return Ok(None);
    };

    if let Some(id) = selected_audio_id.filter(|id| !id.trim().is_empty()) {
        let selected = audio
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("HLS audio rendition {id} not found"))?;
        if selected.group_id != group {
            return Err(anyhow::anyhow!(
                "HLS audio rendition {id} belongs to group {}, but selected video requires group {group}",
                selected.group_id
            ));
        }
        return Ok(Some(selected));
    }

    Ok(audio
        .iter()
        .find(|r| r.group_id == group && r.is_default)
        .or_else(|| audio.iter().find(|r| r.group_id == group)))
}

fn select_subtitle_rendition<'a>(
    subtitles: &'a [HlsRendition],
    group: Option<&str>,
    enabled: bool,
    selected_subtitle_id: Option<&str>,
) -> crate::Result<Option<&'a HlsRendition>> {
    if !enabled {
        return Ok(None);
    }
    let Some(group) = group else {
        return Err(anyhow::anyhow!(
            "selected HLS variant has no SUBTITLES group"
        ));
    };

    if let Some(id) = selected_subtitle_id.filter(|id| !id.trim().is_empty()) {
        let selected = subtitles
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("HLS subtitle rendition {id} not found"))?;
        if selected.group_id != group {
            return Err(anyhow::anyhow!(
                "HLS subtitle rendition {id} belongs to group {}, but selected video requires group {group}",
                selected.group_id
            ));
        }
        return Ok(Some(selected));
    }

    Ok(subtitles
        .iter()
        .find(|r| r.group_id == group && r.is_default)
        .or_else(|| subtitles.iter().find(|r| r.group_id == group)))
}

async fn refresh_selected_input(
    dl: &crate::fetch::SharedDownloader,
    old: &SelectedHlsInput,
) -> crate::Result<SelectedHlsInput> {
    let video_text = dl.get_text(&old.video_url).await?;
    let video = match parse_playlist(&video_text, &old.video_url)? {
        HlsPlaylist::Media(media) => media,
        HlsPlaylist::Master(_) => return Err(anyhow::anyhow!("HLS media URL refreshed as master")),
    };

    let audio = match &old.audio_url {
        Some(url) => {
            let text = dl.get_text(url).await?;
            match parse_playlist(&text, url)? {
                HlsPlaylist::Media(media) => Some(media),
                HlsPlaylist::Master(_) => {
                    return Err(anyhow::anyhow!("HLS audio media URL refreshed as master"))
                }
            }
        }
        None => None,
    };
    let subtitle = match &old.subtitle_url {
        Some(url) => {
            let text = dl.get_text(url).await?;
            match parse_playlist(&text, url)? {
                HlsPlaylist::Media(media) => Some(media),
                HlsPlaylist::Master(_) => {
                    return Err(anyhow::anyhow!(
                        "HLS subtitle media URL refreshed as master"
                    ))
                }
            }
        }
        None => None,
    };

    Ok(SelectedHlsInput {
        video_url: old.video_url.clone(),
        video,
        audio_url: old.audio_url.clone(),
        audio,
        subtitle_url: old.subtitle_url.clone(),
        subtitle,
    })
}

fn configure_live_publish_delay(task: &Arc<Task>, playlist: &HlsMediaPlaylist) -> usize {
    let segment_duration = media_segment_duration_secs(playlist);
    task.configure_fetch_tuning(live_tuning::is_short_segment(segment_duration));
    let delay = live_tuning::publish_delay_segments(
        playlist.is_live(),
        live_tuning::is_short_segment(segment_duration),
    );
    task.hls.set_publish_delay_segments(delay);
    delay
}

fn media_segment_duration_secs(playlist: &HlsMediaPlaylist) -> f64 {
    playlist
        .segments
        .last()
        .or_else(|| playlist.segments.first())
        .map(|s| s.duration)
        .unwrap_or(playlist.target_duration as f64)
}

fn subtitle_segment_for_hls_video<'a>(
    video: &HlsSegment,
    subtitles: &'a HlsMediaPlaylist,
) -> Option<&'a HlsSegment> {
    if let Some(seg) = subtitles
        .segments
        .iter()
        .find(|seg| seg.sequence == video.sequence)
    {
        return Some(seg);
    }

    let mut cursor = subtitles.media_sequence;
    let mut intervals = Vec::new();
    for seg in &subtitles.segments {
        let start = if seg.sequence >= subtitles.media_sequence {
            subtitles
                .segments
                .iter()
                .take_while(|s| s.sequence < seg.sequence)
                .map(|s| s.duration)
                .sum::<f64>()
        } else {
            0.0
        };
        intervals.push((seg, start, start + seg.duration));
        cursor = seg.sequence.saturating_add(1);
    }
    let _ = cursor;

    let v_start: f64 = 0.0;
    let v_end = video.duration;
    intervals
        .into_iter()
        .filter_map(|(seg, start, end)| {
            let overlap = v_end.min(end) - v_start.max(start);
            (overlap > 0.0).then_some((seg, overlap))
        })
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(seg, _)| seg)
        .or_else(|| {
            subtitles
                .segments
                .iter()
                .find(|seg| seg.sequence >= video.sequence)
        })
}

fn initial_sequence(task: &Arc<Task>, playlist: &HlsMediaPlaylist, publish_delay: usize) -> u64 {
    let resume = task.resume_number.load(Ordering::Relaxed);
    let first = playlist
        .segments
        .first()
        .map(|s| s.sequence)
        .unwrap_or(playlist.media_sequence);
    let last = playlist
        .segments
        .last()
        .map(|s| s.sequence)
        .unwrap_or(first);
    if resume > first {
        resume
    } else if playlist.is_live() {
        let backfill = live_tuning::initial_live_backfill_segments(
            media_segment_duration_secs(playlist),
            publish_delay,
        );
        last.saturating_sub(backfill.saturating_sub(1)).max(first)
    } else {
        first
    }
}

fn initial_seen_uris(playlist: &HlsMediaPlaylist, start_sequence: u64) -> HashSet<String> {
    playlist
        .segments
        .iter()
        .filter(|seg| seg.sequence < start_sequence)
        .map(|seg| seg.uri.clone())
        .collect()
}

struct TsContext {
    configured_key: Option<[u8; 16]>,
    key_cache: HashMap<String, [u8; 16]>,
    agg_data: Vec<u8>,
    agg_duration: f64,
    subtitle_acc: SubtitleAccumulator,
    pending_discontinuity: bool,
    last_key_marker: Option<String>,
}

#[derive(Clone)]
struct HlsTsJob {
    video: HlsSegment,
    subtitle: Option<HlsSegment>,
}

struct HlsBatchProgress {
    next_sequence: u64,
    skipped_failure: Option<HlsSkippedFailure>,
}

#[derive(Clone, Copy)]
struct HlsSkippedFailure {
    sequence: u64,
    skip_to: u64,
}

struct HlsSegmentFailure {
    sequence: u64,
    error: anyhow::Error,
}

async fn run_fmp4_inner(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    sel: &super::TrackSelection,
    mut selected: SelectedHlsInput,
) -> crate::Result<()> {
    task.total_segments
        .store(selected.video.segments.len() as u64, Ordering::Relaxed);
    let mut ctx = setup_fmp4_context(task, dl, sel, &selected).await?;
    let publish_delay = configure_live_publish_delay(task, &selected.video);
    let mut next_sequence = initial_sequence(task, &selected.video, publish_delay);
    let mut seen_uris = initial_seen_uris(&selected.video, next_sequence);
    let cancel = task.cancel_token();

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let cycle_started = std::time::Instant::now();

        task.set_stage("转封装 HLS fMP4");
        let jobs = build_fmp4_jobs(&selected, next_sequence, &seen_uris);
        let mut skipped_live_gap = false;
        if !jobs.is_empty() {
            ensure_fmp4_dynamic_keys_for_jobs(task, sel, &jobs, &mut ctx).await?;
            let progress =
                process_fmp4_batch(task, dl, jobs, &mut ctx, selected.video.is_live()).await?;
            next_sequence = progress.next_sequence;
            if let Some(skip) = progress.skipped_failure {
                mark_fmp4_epoch_boundary(
                    task,
                    &mut ctx,
                    &format!("HLS fMP4 live segment {} failed", skip.sequence),
                );
                next_sequence = skip.skip_to;
                skipped_live_gap = true;
            }
            task.resume_number.store(next_sequence, Ordering::Relaxed);
            for seg in selected
                .video
                .segments
                .iter()
                .filter(|s| s.sequence < next_sequence)
            {
                seen_uris.insert(seg.uri.clone());
            }
        }

        if !selected.video.is_live() {
            break;
        }

        if skipped_live_gap
            && has_unseen_hls_segment_at_or_after(&selected.video, &seen_uris, next_sequence)
        {
            continue;
        }

        task.set_stage("等待 HLS 更新");
        let wait = live_tuning::hls_refresh_wait(
            media_segment_duration_secs(&selected.video),
            cycle_started.elapsed(),
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }

        match refresh_selected_input(dl, &selected).await {
            Ok(fresh) => {
                if hls_media_sequence_rewound(&selected.video, &fresh.video) {
                    mark_fmp4_epoch_boundary(task, &mut ctx, "HLS MEDIA-SEQUENCE rewound");
                    seen_uris.clear();
                    next_sequence = fresh
                        .video
                        .segments
                        .first()
                        .map(|s| s.sequence)
                        .unwrap_or(fresh.video.media_sequence);
                    task.resume_number.store(next_sequence, Ordering::Relaxed);
                }
                refresh_fmp4_init_if_needed(task, dl, sel, &fresh, &mut ctx).await?;
                task.total_segments
                    .store(fresh.video.segments.len() as u64, Ordering::Relaxed);
                selected = fresh;
            }
            Err(e) => tracing::warn!(task=%task.id, "refresh hls fmp4 playlist failed: {e:#}"),
        }
    }

    flush_fmp4_aggregate(task, &mut ctx);
    Ok(())
}

fn hls_media_sequence_rewound(old: &HlsMediaPlaylist, fresh: &HlsMediaPlaylist) -> bool {
    fresh.media_sequence < old.media_sequence
        || fresh
            .segments
            .last()
            .zip(old.segments.first())
            .map(|(fresh_last, old_first)| fresh_last.sequence < old_first.sequence)
            .unwrap_or(false)
}

fn has_unseen_hls_segment_at_or_after(
    playlist: &HlsMediaPlaylist,
    seen_uris: &HashSet<String>,
    sequence: u64,
) -> bool {
    playlist
        .segments
        .iter()
        .any(|seg| seg.sequence >= sequence && !seen_uris.contains(&seg.uri))
}

fn hls_skip_to_after(job_order: &[u64], index: usize, sequence: u64) -> u64 {
    job_order
        .get(index.saturating_add(1))
        .copied()
        .unwrap_or_else(|| sequence.saturating_add(1))
}

struct Fmp4Context {
    keystore: KeyStore,
    v_map_uri: Option<String>,
    a_map_uri: Option<String>,
    s_map_uri: Option<String>,
    v_tenc: Option<TrackEncryption>,
    a_tenc: Option<TrackEncryption>,
    s_tenc: Option<TrackEncryption>,
    v_init_kid: Option<String>,
    a_init_kid: Option<String>,
    s_init_kid: Option<String>,
    v_default_iv_size: Option<u8>,
    a_default_iv_size: Option<u8>,
    s_default_iv_size: Option<u8>,
    video_codec: VideoCodec,
    aac_cfg: Option<crate::mp4::aac::AacConfig>,
    muxer: TsMuxer,
    acc_v_dts: u64,
    acc_a_dts: u64,
    agg_vaus: Vec<AccessUnit>,
    agg_aaus: Vec<AudioUnit>,
    subtitle_acc: SubtitleAccumulator,
    agg_v_dur_ts: u64,
    agg_a_dur_ts: u64,
    agg_duration: f64,
    pending_discontinuity: bool,
    last_video_kid: Option<String>,
    last_audio_kid: Option<String>,
    last_subtitle_kid: Option<String>,
    last_v_tfdt: Option<u64>,
    last_a_tfdt: Option<u64>,
}

async fn setup_fmp4_context(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    sel: &super::TrackSelection,
    selected: &SelectedHlsInput,
) -> crate::Result<Fmp4Context> {
    task.set_stage("下载 HLS init");
    let vinit_url = selected
        .video
        .map_uri
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("HLS fMP4 video playlist is missing EXT-X-MAP URI"))?;
    let vinit = dl.get(vinit_url).await?;
    let video_timescale = crate::mp4::mdhd::timescale_from_init(&vinit)
        .ok_or_else(|| anyhow::anyhow!("video init mdhd timescale not found"))?;
    let video_codec = VideoCodec::from_codecs(&task.codecs);
    let params = match video_codec {
        VideoCodec::Hevc => ParamSets::find_in_init(&vinit).unwrap_or_default(),
        VideoCodec::H264 => crate::mp4::avcc::find_avcc_in_init(&vinit).unwrap_or_default(),
    };
    let dovi = DoviConfig::find_in_init(&vinit);

    let vtenc = TrackEncryption::find_in_init(&vinit);
    let vkid = vtenc.as_ref().map(|t| t.kid.clone());
    let v_default_iv_size = vtenc.as_ref().map(|t| t.iv_size);

    let mut aac_cfg = None;
    let mut audio_codec = None;
    let mut audio_timescale = 48_000;
    let mut a_default_iv_size = None;
    let mut atenc = None;
    let mut akid = None;
    if let Some(audio_playlist) = &selected.audio {
        let ainit_url = audio_playlist
            .map_uri
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("HLS fMP4 audio playlist is missing EXT-X-MAP URI"))?;
        let ainit = dl.get(ainit_url).await?;
        audio_timescale = crate::mp4::mdhd::timescale_from_init(&ainit).unwrap_or(48_000);
        let acodec = detect_audio_codec(&task.codecs);
        audio_codec = Some(acodec);
        if acodec == AudioCodec::AacAdts {
            aac_cfg = crate::mp4::aac::AacConfig::find_in_init(&ainit);
        }

        atenc = TrackEncryption::find_in_init(&ainit);
        akid = atenc.as_ref().map(|t| t.kid.clone());
        a_default_iv_size = atenc.as_ref().map(|t| t.iv_size);
    }
    let mut s_default_iv_size = None;
    let mut stenc = None;
    let mut skid = None;
    if let Some(subtitle_playlist) = &selected.subtitle {
        if let Some(sinit_url) = &subtitle_playlist.map_uri {
            let sinit = dl.get(sinit_url).await?;
            stenc = TrackEncryption::find_in_init(&sinit);
            skid = stenc.as_ref().map(|t| t.kid.clone());
            s_default_iv_size = stenc.as_ref().map(|t| t.iv_size);
        }
    }

    task.set_stage(if sel.key_mode.is_dynamic() {
        "动态获取 KEY"
    } else {
        "解析 KEY"
    });
    let mut required_kids = Vec::new();
    required_kids.extend(vkid.iter().cloned());
    required_kids.extend(akid.iter().cloned());
    required_kids.extend(skid.iter().cloned());
    required_kids.extend(playlist_advertised_kids(&selected.video));
    if let Some(audio_playlist) = &selected.audio {
        required_kids.extend(playlist_advertised_kids(audio_playlist));
    }
    if let Some(subtitle_playlist) = &selected.subtitle {
        required_kids.extend(playlist_advertised_kids(subtitle_playlist));
    }
    let keystore = key_resolver::resolve_key_store(
        sel.key_mode,
        &sel.keys,
        required_kids.iter().map(String::as_str),
    )
    .await?;

    let min_cts = scan_min_cts_hls(dl, &selected.video).await.unwrap_or(0);
    let clock = ClockState::new(min_cts, video_timescale, 90_000);
    let muxer = TsMuxer::new(
        &params,
        video_codec,
        dovi,
        audio_codec,
        audio_timescale,
        clock,
    );

    Ok(Fmp4Context {
        keystore,
        v_map_uri: selected.video.map_uri.clone(),
        a_map_uri: selected
            .audio
            .as_ref()
            .and_then(|playlist| playlist.map_uri.clone()),
        s_map_uri: selected
            .subtitle
            .as_ref()
            .and_then(|playlist| playlist.map_uri.clone()),
        v_tenc: vtenc,
        a_tenc: atenc,
        s_tenc: stenc,
        v_init_kid: vkid,
        a_init_kid: akid,
        s_init_kid: skid,
        v_default_iv_size,
        a_default_iv_size,
        s_default_iv_size,
        video_codec,
        aac_cfg,
        muxer,
        acc_v_dts: task.acc_v_dts.load(Ordering::Relaxed),
        acc_a_dts: task.acc_a_dts.load(Ordering::Relaxed),
        agg_vaus: Vec::new(),
        agg_aaus: Vec::new(),
        subtitle_acc: SubtitleAccumulator::new(),
        agg_v_dur_ts: 0,
        agg_a_dur_ts: 0,
        agg_duration: 0.0,
        pending_discontinuity: false,
        last_video_kid: None,
        last_audio_kid: None,
        last_subtitle_kid: None,
        last_v_tfdt: None,
        last_a_tfdt: None,
    })
}

async fn refresh_fmp4_init_if_needed(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    sel: &super::TrackSelection,
    fresh: &SelectedHlsInput,
    ctx: &mut Fmp4Context,
) -> crate::Result<()> {
    let mut changed = false;
    let mut init_kids = Vec::new();

    if fresh.video.map_uri != ctx.v_map_uri {
        mark_fmp4_epoch_boundary(task, ctx, "HLS video EXT-X-MAP changed");
        let map_uri =
            fresh.video.map_uri.as_ref().ok_or_else(|| {
                anyhow::anyhow!("HLS fMP4 video playlist is missing EXT-X-MAP URI")
            })?;
        let init = dl.get(map_uri).await?;
        ctx.v_tenc = TrackEncryption::find_in_init(&init);
        ctx.v_init_kid = ctx.v_tenc.as_ref().map(|t| t.kid.clone());
        ctx.v_default_iv_size = ctx.v_tenc.as_ref().map(|t| t.iv_size);
        ctx.v_map_uri = Some(map_uri.clone());
        init_kids.extend(ctx.v_init_kid.iter().cloned());
        changed = true;
    }

    let fresh_audio_map = fresh.audio.as_ref().and_then(|p| p.map_uri.clone());
    if fresh_audio_map != ctx.a_map_uri {
        mark_fmp4_epoch_boundary(task, ctx, "HLS audio EXT-X-MAP changed");
        if let Some(map_uri) = fresh_audio_map {
            let init = dl.get(&map_uri).await?;
            ctx.a_tenc = TrackEncryption::find_in_init(&init);
            ctx.a_init_kid = ctx.a_tenc.as_ref().map(|t| t.kid.clone());
            ctx.a_default_iv_size = ctx.a_tenc.as_ref().map(|t| t.iv_size);
            if detect_audio_codec(&task.codecs) == AudioCodec::AacAdts {
                ctx.aac_cfg = crate::mp4::aac::AacConfig::find_in_init(&init);
            }
            ctx.a_map_uri = Some(map_uri);
            init_kids.extend(ctx.a_init_kid.iter().cloned());
        } else {
            ctx.a_tenc = None;
            ctx.a_init_kid = None;
            ctx.a_default_iv_size = None;
            ctx.a_map_uri = None;
        }
        changed = true;
    }

    let fresh_subtitle_map = fresh.subtitle.as_ref().and_then(|p| p.map_uri.clone());
    if fresh_subtitle_map != ctx.s_map_uri {
        mark_fmp4_epoch_boundary(task, ctx, "HLS subtitle EXT-X-MAP changed");
        if let Some(map_uri) = fresh_subtitle_map {
            let init = dl.get(&map_uri).await?;
            ctx.s_tenc = TrackEncryption::find_in_init(&init);
            ctx.s_init_kid = ctx.s_tenc.as_ref().map(|t| t.kid.clone());
            ctx.s_default_iv_size = ctx.s_tenc.as_ref().map(|t| t.iv_size);
            ctx.s_map_uri = Some(map_uri);
            init_kids.extend(ctx.s_init_kid.iter().cloned());
        } else {
            ctx.s_tenc = None;
            ctx.s_init_kid = None;
            ctx.s_default_iv_size = None;
            ctx.s_map_uri = None;
        }
        changed = true;
    }

    if changed && sel.key_mode.is_dynamic() && !init_kids.is_empty() {
        task.set_stage("动态补取 KEY");
        key_resolver::fetch_missing_dynamic_keys(
            &mut ctx.keystore,
            init_kids.iter().map(String::as_str),
        )
        .await?;
        task.set_stage("转封装 HLS fMP4");
    }

    Ok(())
}

fn mark_fmp4_epoch_boundary(task: &Arc<Task>, ctx: &mut Fmp4Context, reason: &str) {
    flush_fmp4_aggregate(task, ctx);
    ctx.pending_discontinuity = true;
    ctx.last_v_tfdt = None;
    ctx.last_a_tfdt = None;
    ctx.last_subtitle_kid = None;
    ctx.subtitle_acc.clear();
    ctx.acc_v_dts = 0;
    ctx.acc_a_dts = 0;
    task.origin_v_tfdt.store(u64::MAX, Ordering::Relaxed);
    task.origin_a_tfdt.store(u64::MAX, Ordering::Relaxed);
    tracing::warn!(task = %task.id, reason, "starting a new HLS output discontinuity epoch");
}

fn hls_key_iv(key: &HlsSegmentKey) -> Option<[u8; 16]> {
    match key {
        HlsSegmentKey::SampleAes { iv, .. } | HlsSegmentKey::Aes128 { iv, .. } => *iv,
        _ => None,
    }
}

fn hls_key_kid(key: &HlsSegmentKey) -> Option<String> {
    let uri = match key {
        HlsSegmentKey::SampleAesCtr { uri: Some(uri), .. }
        | HlsSegmentKey::SampleAes { uri: Some(uri), .. } => uri,
        _ => return None,
    };
    kid_from_playready_data_uri(uri).and_then(|kid| key_resolver::normalize_kid(&kid))
}

fn playlist_advertised_kids(playlist: &HlsMediaPlaylist) -> Vec<String> {
    playlist
        .segments
        .iter()
        .filter_map(|seg| hls_key_kid(&seg.key))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn build_fmp4_decryptor(
    key: [u8; 16],
    tenc: Option<&TrackEncryption>,
    playlist_iv: Option<[u8; 16]>,
    sample_aes: bool,
) -> Decryptor {
    let use_cbcs = tenc.map(|t| t.is_cbcs()).unwrap_or(false) || sample_aes;
    let constant_iv = tenc.and_then(|t| t.constant_iv).or(playlist_iv);
    if use_cbcs {
        let crypt = tenc.map(|t| t.crypt_byte_block).unwrap_or(0);
        let skip = tenc.map(|t| t.skip_byte_block).unwrap_or(0);
        match constant_iv {
            Some(iv) => Decryptor::cbcs_with_constant_iv(key, iv, crypt, skip),
            None => Decryptor::new_cbcs(key, crypt, skip),
        }
    } else {
        match constant_iv {
            Some(iv) => Decryptor::with_constant_iv(key, iv),
            None => Decryptor::new(key),
        }
    }
}

#[derive(Clone, Copy)]
enum Fmp4TrackKind {
    Video,
    Audio,
    Subtitle,
}

fn fmp4_decryptor_for_segment(
    ctx: &Fmp4Context,
    segment: &HlsSegment,
    kind: Fmp4TrackKind,
) -> crate::Result<Option<Decryptor>> {
    let tenc = match kind {
        Fmp4TrackKind::Video => ctx.v_tenc.as_ref(),
        Fmp4TrackKind::Audio => ctx.a_tenc.as_ref(),
        Fmp4TrackKind::Subtitle => ctx.s_tenc.as_ref(),
    };
    let init_kid = match kind {
        Fmp4TrackKind::Video => ctx.v_init_kid.as_deref(),
        Fmp4TrackKind::Audio => ctx.a_init_kid.as_deref(),
        Fmp4TrackKind::Subtitle => ctx.s_init_kid.as_deref(),
    };
    let advertised_kid = hls_key_kid(&segment.key);
    let kid = advertised_kid.as_deref().or(init_kid);
    let encrypted = tenc.is_some()
        || matches!(
            &segment.key,
            HlsSegmentKey::SampleAesCtr { .. } | HlsSegmentKey::SampleAes { .. }
        );

    let key = kid
        .and_then(|k| ctx.keystore.get(k))
        .or_else(|| ctx.keystore.get(""));
    match key {
        Some(key) if encrypted => Ok(Some(build_fmp4_decryptor(
            key,
            tenc,
            hls_key_iv(&segment.key),
            matches!(&segment.key, HlsSegmentKey::SampleAes { .. }),
        ))),
        Some(_) => Ok(None),
        None if !encrypted => Ok(None),
        None => Err(anyhow::anyhow!(
            "no key matches HLS {} KID {:?}",
            match kind {
                Fmp4TrackKind::Video => "video",
                Fmp4TrackKind::Audio => "audio",
                Fmp4TrackKind::Subtitle => "subtitle",
            },
            kid
        )),
    }
}

fn fmp4_segment_kid(
    ctx: &Fmp4Context,
    segment: &HlsSegment,
    kind: Fmp4TrackKind,
) -> Option<String> {
    hls_key_kid(&segment.key).or_else(|| match kind {
        Fmp4TrackKind::Video => ctx.v_init_kid.clone(),
        Fmp4TrackKind::Audio => ctx.a_init_kid.clone(),
        Fmp4TrackKind::Subtitle => ctx.s_init_kid.clone(),
    })
}

fn timeline_rewound(last: Option<u64>, current: Option<u64>) -> bool {
    matches!((last, current), (Some(prev), Some(now)) if now < prev)
}

fn key_changed(previous: Option<&String>, current: Option<&String>) -> bool {
    matches!((previous, current), (Some(prev), Some(curr)) if prev != curr)
}

fn build_fmp4_jobs(
    selected: &SelectedHlsInput,
    next_sequence: u64,
    seen_uris: &HashSet<String>,
) -> Vec<Fmp4Job> {
    let require_audio = selected.audio.is_some();
    let audio_by_seq: HashMap<u64, HlsSegment> = selected
        .audio
        .as_ref()
        .map(|a| a.segments.iter().map(|s| (s.sequence, s.clone())).collect())
        .unwrap_or_default();
    let subtitle_by_seq: HashMap<u64, HlsSegment> = selected
        .subtitle
        .as_ref()
        .map(|s| {
            s.segments
                .iter()
                .map(|seg| (seg.sequence, seg.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut jobs = Vec::new();
    for video in selected
        .video
        .segments
        .iter()
        .filter(|seg| seg.sequence >= next_sequence || !seen_uris.contains(&seg.uri))
        .filter(|seg| !seen_uris.contains(&seg.uri))
        .cloned()
    {
        let audio = audio_by_seq.get(&video.sequence).cloned();
        if require_audio && audio.is_none() {
            break;
        }
        let subtitle = subtitle_by_seq.get(&video.sequence).cloned();
        jobs.push(Fmp4Job {
            sequence: video.sequence,
            audio,
            subtitle,
            video,
        });
    }
    jobs
}

#[derive(Clone)]
struct Fmp4Job {
    sequence: u64,
    video: HlsSegment,
    audio: Option<HlsSegment>,
    subtitle: Option<HlsSegment>,
}

async fn process_fmp4_batch(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    jobs: Vec<Fmp4Job>,
    ctx: &mut Fmp4Context,
    allow_live_skip_failures: bool,
) -> crate::Result<HlsBatchProgress> {
    let batch_first = jobs.first().map(|j| j.sequence).unwrap_or(0);
    let job_order: Vec<u64> = jobs.iter().map(|j| j.sequence).collect();
    let dl2 = dl.clone();
    let task2 = task.clone();
    let fetch_concurrency = task.segment_fetch_concurrency();
    let stream = futures::stream::iter(jobs.into_iter().map(move |job| {
        let dl = dl2.clone();
        let task = task2.clone();
        async move {
            let vdl = dl.clone();
            let adl = dl.clone();
            let sdl = dl.clone();
            let vtask = task.clone();
            let atask = task.clone();
            let stask = task.clone();
            let sequence = job.sequence;
            let vurl = job.video.uri.clone();
            let aurl = job.audio.as_ref().map(|a| a.uri.clone());
            let surl = job.subtitle.as_ref().map(|s| s.uri.clone());
            let video = async move { vtask.fetch_media(&vdl, &vurl).await };
            let audio = async move {
                match aurl {
                    Some(url) => atask.fetch_media(&adl, &url).await.map(Some),
                    None => Ok(None),
                }
            };
            let subtitle = async move {
                match surl {
                    Some(url) => match stask.fetch_media(&sdl, &url).await {
                        Ok(data) => Ok(Some(data)),
                        Err(e) => {
                            tracing::warn!(
                                task = %stask.id,
                                seq = sequence,
                                url = %url,
                                "HLS fMP4 subtitle segment fetch failed; publishing empty subtitle: {e:#}"
                            );
                            Ok(None)
                        }
                    },
                    None => Ok(None),
                }
            };
            match tokio::try_join!(video, audio, subtitle) {
                Ok((venc, aenc, senc)) => Ok(DownloadedFmp4Segment {
                    job,
                    venc,
                    aenc,
                    senc,
                }),
                Err(error) => Err(HlsSegmentFailure { sequence, error }),
            }
        }
    }))
    .buffer_unordered(fetch_concurrency);
    futures::pin_mut!(stream);

    let cancel = task.cancel_token();
    let mut pending: BTreeMap<u64, DownloadedFmp4Segment> = BTreeMap::new();
    let mut failed: BTreeMap<u64, anyhow::Error> = BTreeMap::new();
    let mut next_index = 0usize;
    let mut next_processed = batch_first;

    while let Some(item) = stream.next().await {
        if cancel.is_cancelled() {
            break;
        }
        match item {
            Ok(downloaded) => {
                pending.insert(downloaded.job.sequence, downloaded);
            }
            Err(failure) => {
                if allow_live_skip_failures {
                    tracing::warn!(
                        task = %task.id,
                        seq = failure.sequence,
                        "HLS fMP4 segment fetch failed; will skip stale live segment: {:#}",
                        failure.error
                    );
                    failed.insert(failure.sequence, failure.error);
                } else {
                    return Err(failure.error);
                }
            }
        }

        while let Some(expected) = job_order.get(next_index).copied() {
            if let Some(error) = failed.remove(&expected) {
                let skip_to = hls_skip_to_after(&job_order, next_index, expected);
                tracing::warn!(
                    task = %task.id,
                    seq = expected,
                    skip_to,
                    "HLS fMP4 segment failed at live head; skipping: {error:#}"
                );
                return Ok(HlsBatchProgress {
                    next_sequence: skip_to,
                    skipped_failure: Some(HlsSkippedFailure {
                        sequence: expected,
                        skip_to,
                    }),
                });
            }
            let Some(downloaded) = pending.remove(&expected) else {
                break;
            };
            if cancel.is_cancelled() {
                pending.insert(expected, downloaded);
                break;
            }

            if let Err(error) = process_fmp4_segment(task, downloaded, ctx) {
                if allow_live_skip_failures {
                    let skip_to = hls_skip_to_after(&job_order, next_index, expected);
                    tracing::warn!(
                        task = %task.id,
                        seq = expected,
                        skip_to,
                        "HLS fMP4 segment processing failed; skipping stale live segment: {error:#}"
                    );
                    return Ok(HlsBatchProgress {
                        next_sequence: skip_to,
                        skipped_failure: Some(HlsSkippedFailure {
                            sequence: expected,
                            skip_to,
                        }),
                    });
                }
                return Err(error);
            }
            next_index += 1;
            next_processed = expected + 1;
            task.resume_number.store(next_processed, Ordering::Relaxed);
            task.acc_v_dts.store(ctx.acc_v_dts, Ordering::Relaxed);
            task.acc_a_dts.store(ctx.acc_a_dts, Ordering::Relaxed);
        }
    }

    Ok(HlsBatchProgress {
        next_sequence: next_processed,
        skipped_failure: None,
    })
}

async fn ensure_fmp4_dynamic_keys_for_jobs(
    task: &Arc<Task>,
    sel: &super::TrackSelection,
    jobs: &[Fmp4Job],
    ctx: &mut Fmp4Context,
) -> crate::Result<()> {
    if !sel.key_mode.is_dynamic() {
        return Ok(());
    }
    let mut kids = Vec::new();
    for job in jobs {
        if let Some(kid) = hls_key_kid(&job.video.key).or_else(|| ctx.v_init_kid.clone()) {
            kids.push(kid);
        }
        if let Some(audio) = &job.audio {
            if let Some(kid) = hls_key_kid(&audio.key).or_else(|| ctx.a_init_kid.clone()) {
                kids.push(kid);
            }
        }
        if let Some(subtitle) = &job.subtitle {
            if let Some(kid) = hls_key_kid(&subtitle.key).or_else(|| ctx.s_init_kid.clone()) {
                kids.push(kid);
            }
        }
    }
    if !kids.is_empty() {
        task.set_stage("动态补取 KEY");
        key_resolver::fetch_missing_dynamic_keys(
            &mut ctx.keystore,
            kids.iter().map(String::as_str),
        )
        .await?;
        task.set_stage("转封装 HLS fMP4");
    }
    Ok(())
}

struct DownloadedFmp4Segment {
    job: Fmp4Job,
    venc: bytes::Bytes,
    aenc: Option<bytes::Bytes>,
    senc: Option<bytes::Bytes>,
}

fn process_fmp4_segment(
    task: &Arc<Task>,
    downloaded: DownloadedFmp4Segment,
    ctx: &mut Fmp4Context,
) -> crate::Result<()> {
    ensure_fmp4_key(&downloaded.job.video.key)?;
    if let Some(audio) = &downloaded.job.audio {
        ensure_fmp4_key(&audio.key)?;
    }
    if let Some(subtitle) = &downloaded.job.subtitle {
        ensure_fmp4_key(&subtitle.key)?;
    }
    let video_kid = fmp4_segment_kid(ctx, &downloaded.job.video, Fmp4TrackKind::Video);
    let audio_kid = downloaded
        .job
        .audio
        .as_ref()
        .and_then(|audio| fmp4_segment_kid(ctx, audio, Fmp4TrackKind::Audio));
    let subtitle_kid = downloaded
        .job
        .subtitle
        .as_ref()
        .and_then(|subtitle| fmp4_segment_kid(ctx, subtitle, Fmp4TrackKind::Subtitle));
    if key_changed(ctx.last_video_kid.as_ref(), video_kid.as_ref())
        || key_changed(ctx.last_audio_kid.as_ref(), audio_kid.as_ref())
        || key_changed(ctx.last_subtitle_kid.as_ref(), subtitle_kid.as_ref())
    {
        flush_fmp4_aggregate(task, ctx);
        tracing::info!(
            task = %task.id,
            seq = downloaded.job.sequence,
            video_kid = ?video_kid,
            audio_kid = ?audio_kid,
            subtitle_kid = ?subtitle_kid,
            "HLS fMP4 key boundary: flushed aggregate before switching decryptor"
        );
    }
    if downloaded.job.video.discontinuity
        || downloaded
            .job
            .audio
            .as_ref()
            .map(|a| a.discontinuity)
            .unwrap_or(false)
        || downloaded
            .job
            .subtitle
            .as_ref()
            .map(|s| s.discontinuity)
            .unwrap_or(false)
    {
        mark_fmp4_epoch_boundary(task, ctx, "upstream HLS EXT-X-DISCONTINUITY");
    }
    ctx.last_video_kid = video_kid;
    ctx.last_audio_kid = audio_kid;
    ctx.last_subtitle_kid = subtitle_kid;

    let vdec = fmp4_decryptor_for_segment(ctx, &downloaded.job.video, Fmp4TrackKind::Video)?;
    let adec = downloaded
        .job
        .audio
        .as_ref()
        .map(|audio| fmp4_decryptor_for_segment(ctx, audio, Fmp4TrackKind::Audio))
        .transpose()?
        .flatten();
    let sdec = downloaded
        .job
        .subtitle
        .as_ref()
        .map(|subtitle| fmp4_decryptor_for_segment(ctx, subtitle, Fmp4TrackKind::Subtitle))
        .transpose()?
        .flatten();

    let downloaded_bytes = downloaded.venc.len() as u64
        + downloaded
            .aenc
            .as_ref()
            .map(|a| a.len() as u64)
            .unwrap_or(0)
        + downloaded
            .senc
            .as_ref()
            .map(|s| s.len() as u64)
            .unwrap_or(0);
    task.bytes_done
        .fetch_add(downloaded_bytes, Ordering::Relaxed);

    let mut vparsed =
        parse_media_segment_with_default_iv_size(&downloaded.venc, ctx.v_default_iv_size)
            .ok_or_else(|| {
                anyhow::anyhow!("parse HLS video seq {} failed", downloaded.job.sequence)
            })?;
    if let Some(vdec) = &vdec {
        vdec.decrypt_segment(&mut vparsed.mdat, &vparsed.samples);
    }
    let mut aparsed = match downloaded.aenc {
        Some(aenc) => {
            let mut parsed = parse_media_segment_with_default_iv_size(&aenc, ctx.a_default_iv_size)
                .ok_or_else(|| {
                    anyhow::anyhow!("parse HLS audio seq {} failed", downloaded.job.sequence)
                })?;
            if let Some(adec) = &adec {
                adec.decrypt_segment(&mut parsed.mdat, &parsed.samples);
            }
            Some(parsed)
        }
        None => None,
    };
    if timeline_rewound(ctx.last_v_tfdt, vparsed.base_media_decode_time)
        || timeline_rewound(
            ctx.last_a_tfdt,
            aparsed.as_ref().and_then(|p| p.base_media_decode_time),
        )
    {
        mark_fmp4_epoch_boundary(task, ctx, "HLS fMP4 tfdt rewound");
    }
    ctx.last_v_tfdt = vparsed.base_media_decode_time;
    ctx.last_a_tfdt = aparsed.as_ref().and_then(|p| p.base_media_decode_time);

    let mut seg_v_dts = match vparsed.base_media_decode_time {
        Some(tfdt) => {
            let origin = task.origin_v_tfdt.load(Ordering::Relaxed);
            let origin = if origin == u64::MAX {
                task.origin_v_tfdt.store(tfdt, Ordering::Relaxed);
                tfdt
            } else {
                origin
            };
            tfdt.saturating_sub(origin)
        }
        None => ctx.acc_v_dts,
    };
    let mut vaus = Vec::with_capacity(vparsed.samples.len());
    for s in &vparsed.samples {
        let (a, b) = s.data_range;
        let b = b.min(vparsed.mdat.len());
        let dts = seg_v_dts;
        seg_v_dts += s.duration as u64;
        vaus.push(AccessUnit::from_sample(
            &vparsed.mdat[a..b],
            dts,
            s.cts_offset as i64,
            ctx.video_codec,
        ));
    }
    let seg_v_dur_ts = vparsed
        .samples
        .iter()
        .map(|s| s.duration as u64)
        .sum::<u64>();
    ctx.acc_v_dts += seg_v_dur_ts;

    let mut aaus = Vec::new();
    let mut seg_a_dur_ts = 0u64;
    if let Some(aparsed) = aparsed.take() {
        let mut seg_a_pts = match aparsed.base_media_decode_time {
            Some(tfdt) => {
                let origin = task.origin_a_tfdt.load(Ordering::Relaxed);
                let origin = if origin == u64::MAX {
                    task.origin_a_tfdt.store(tfdt, Ordering::Relaxed);
                    tfdt
                } else {
                    origin
                };
                tfdt.saturating_sub(origin)
            }
            None => ctx.acc_a_dts,
        };
        for s in &aparsed.samples {
            let (x, y) = s.data_range;
            let y = y.min(aparsed.mdat.len());
            let pts = seg_a_pts;
            seg_a_pts += s.duration as u64;
            let frame = &aparsed.mdat[x..y];
            let data = match &ctx.aac_cfg {
                Some(cfg) => cfg.wrap_adts(frame),
                None => frame.to_vec(),
            };
            aaus.push(AudioUnit { data, pts });
        }
        seg_a_dur_ts = aparsed
            .samples
            .iter()
            .map(|s| s.duration as u64)
            .sum::<u64>();
        ctx.acc_a_dts += seg_a_dur_ts;
    }

    if !ctx.agg_vaus.is_empty() && !vaus.is_empty() {
        let first_v = ctx.agg_vaus.first().map(|a| a.dts).unwrap_or(0);
        let expected_v = first_v + ctx.agg_v_dur_ts;
        let v_off = expected_v as i64 - vaus[0].dts as i64;
        if v_off != 0 {
            for au in &mut vaus {
                au.dts = (au.dts as i64 + v_off).max(0) as u64;
            }
        }
    }
    if !ctx.agg_aaus.is_empty() && !aaus.is_empty() {
        let first_a = ctx.agg_aaus.first().map(|a| a.pts).unwrap_or(0);
        let expected_a = first_a + ctx.agg_a_dur_ts;
        let a_off = expected_a as i64 - aaus[0].pts as i64;
        if a_off != 0 {
            for au in &mut aaus {
                au.pts = (au.pts as i64 + a_off).max(0) as u64;
            }
        }
    }

    let single_duration = downloaded.job.video.duration;
    ctx.agg_vaus.extend(vaus);
    ctx.agg_aaus.extend(aaus);
    if let Some(senc) = downloaded.senc.as_ref() {
        if let Err(e) = ctx.subtitle_acc.append_fragment(
            senc,
            single_duration,
            1_000,
            sdec.as_ref(),
            ctx.s_default_iv_size,
        ) {
            tracing::warn!(task=%task.id, seq=downloaded.job.sequence, "HLS fMP4 subtitle conversion failed: {e:#}");
            ctx.subtitle_acc.append_empty(single_duration);
        }
    } else {
        ctx.subtitle_acc.append_empty(single_duration);
    }
    ctx.agg_v_dur_ts += seg_v_dur_ts;
    ctx.agg_a_dur_ts += seg_a_dur_ts;
    ctx.agg_duration += single_duration;

    if ctx.agg_duration >= AGGREGATE_TARGET_S || single_duration >= SHORT_SEG_THRESHOLD_S {
        flush_fmp4_aggregate(task, ctx);
    }
    tracing::debug!(task=%task.id, seq=downloaded.job.sequence, "hls fmp4 segment muxed");
    Ok(())
}

fn flush_fmp4_aggregate(task: &Arc<Task>, ctx: &mut Fmp4Context) {
    if ctx.agg_vaus.is_empty() {
        return;
    }
    let ts_bytes = ctx.muxer.mux_segment(&ctx.agg_vaus, &ctx.agg_aaus);
    let subtitle_body = ctx.subtitle_acc.take_body();
    if ctx.pending_discontinuity {
        task.hls
            .push_segment_discontinuity(ctx.agg_duration, bytes::Bytes::from(ts_bytes));
        if task.subtitles_enabled {
            task.subtitles
                .push_segment_discontinuity(ctx.agg_duration, subtitle_body);
        }
        ctx.pending_discontinuity = false;
    } else {
        task.hls
            .push_segment(ctx.agg_duration, bytes::Bytes::from(ts_bytes));
        if task.subtitles_enabled {
            task.subtitles.push_segment(ctx.agg_duration, subtitle_body);
        }
    }
    task.segments_done.fetch_add(1, Ordering::Relaxed);
    ctx.agg_vaus.clear();
    ctx.agg_aaus.clear();
    ctx.agg_v_dur_ts = 0;
    ctx.agg_a_dur_ts = 0;
    ctx.agg_duration = 0.0;
}

fn ensure_fmp4_key(key: &HlsSegmentKey) -> crate::Result<()> {
    match key {
        HlsSegmentKey::SampleAesCtr { .. }
        | HlsSegmentKey::SampleAes { .. }
        | HlsSegmentKey::None => Ok(()),
        HlsSegmentKey::Aes128 { .. } => Err(anyhow::anyhow!(
            "HLS AES-128 full-segment encryption is not valid for fMP4 EXT-X-MAP input"
        )),
        HlsSegmentKey::Unsupported { method, key_format } => Err(anyhow::anyhow!(
            "unsupported HLS fMP4 encryption method {method} (keyformat={key_format})"
        )),
    }
}

async fn scan_min_cts_hls(
    dl: &crate::fetch::SharedDownloader,
    playlist: &HlsMediaPlaylist,
) -> Option<i64> {
    let first = playlist.segments.first()?;
    let data = dl.get(&first.uri).await.ok()?;
    let parsed = parse_media_segment(&data)?;
    parsed.samples.iter().map(|s| s.cts_offset as i64).min()
}

fn detect_audio_codec(codecs: &str) -> AudioCodec {
    let c = codecs.to_lowercase();
    if c.contains("ec-3") || c.contains("ec3") {
        AudioCodec::Ec3
    } else if c.contains("ac-3") || c.contains("ac3") {
        AudioCodec::Ac3
    } else {
        AudioCodec::AacAdts
    }
}

async fn process_ts_batch(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    jobs: Vec<HlsTsJob>,
    ctx: &mut TsContext,
    allow_live_skip_failures: bool,
) -> crate::Result<HlsBatchProgress> {
    let batch_first = jobs.first().map(|j| j.video.sequence).unwrap_or(0);
    let job_order: Vec<u64> = jobs.iter().map(|j| j.video.sequence).collect();
    let dl2 = dl.clone();
    let task2 = task.clone();
    let fetch_concurrency = task.segment_fetch_concurrency();

    let stream = futures::stream::iter(jobs.into_iter().map(move |job| {
        let dl = dl2.clone();
        let task = task2.clone();
        async move {
            let video = job.video.clone();
            let subtitle = job.subtitle.clone();
            let video_data = match task.fetch_media(&dl, &video.uri).await {
                Ok(data) => data,
                Err(error) => {
                    return Err(HlsSegmentFailure {
                        sequence: video.sequence,
                        error,
                    });
                }
            };
            let subtitle_data = match subtitle.as_ref() {
                Some(seg) => match task.fetch_media(&dl, &seg.uri).await {
                    Ok(data) => Some(data),
                    Err(e) => {
                        tracing::warn!(
                            task = %task.id,
                            seq = video.sequence,
                            url = %seg.uri,
                            "HLS subtitle segment fetch failed; publishing empty subtitle: {e:#}"
                        );
                        None
                    }
                },
                None => None,
            };
            Ok::<_, anyhow::Error>(DownloadedHlsSegment {
                segment: video,
                data: video_data,
                subtitle,
                subtitle_data,
            })
            .map_err(|error| HlsSegmentFailure {
                sequence: job.video.sequence,
                error,
            })
        }
    }))
    .buffer_unordered(fetch_concurrency);
    futures::pin_mut!(stream);

    let cancel = task.cancel_token();
    let mut pending: BTreeMap<u64, DownloadedHlsSegment> = BTreeMap::new();
    let mut failed: BTreeMap<u64, anyhow::Error> = BTreeMap::new();
    let mut next_index = 0usize;
    let mut next_processed = batch_first;

    while let Some(item) = stream.next().await {
        if cancel.is_cancelled() {
            break;
        }
        match item {
            Ok(downloaded) => {
                pending.insert(downloaded.segment.sequence, downloaded);
            }
            Err(failure) => {
                if allow_live_skip_failures {
                    tracing::warn!(
                        task = %task.id,
                        seq = failure.sequence,
                        "HLS TS segment fetch failed; will skip stale live segment: {:#}",
                        failure.error
                    );
                    failed.insert(failure.sequence, failure.error);
                } else {
                    return Err(failure.error);
                }
            }
        }

        while let Some(expected) = job_order.get(next_index).copied() {
            if let Some(error) = failed.remove(&expected) {
                let skip_to = hls_skip_to_after(&job_order, next_index, expected);
                tracing::warn!(
                    task = %task.id,
                    seq = expected,
                    skip_to,
                    "HLS TS segment failed at live head; skipping: {error:#}"
                );
                return Ok(HlsBatchProgress {
                    next_sequence: skip_to,
                    skipped_failure: Some(HlsSkippedFailure {
                        sequence: expected,
                        skip_to,
                    }),
                });
            }
            let Some(downloaded) = pending.remove(&expected) else {
                break;
            };
            if cancel.is_cancelled() {
                pending.insert(expected, downloaded);
                break;
            }

            if let Err(error) = process_downloaded_segment(task, dl, downloaded, ctx).await {
                if allow_live_skip_failures {
                    let skip_to = hls_skip_to_after(&job_order, next_index, expected);
                    tracing::warn!(
                        task = %task.id,
                        seq = expected,
                        skip_to,
                        "HLS TS segment processing failed; skipping stale live segment: {error:#}"
                    );
                    return Ok(HlsBatchProgress {
                        next_sequence: skip_to,
                        skipped_failure: Some(HlsSkippedFailure {
                            sequence: expected,
                            skip_to,
                        }),
                    });
                }
                return Err(error);
            }
            next_index += 1;
            next_processed = expected + 1;
            task.resume_number.store(next_processed, Ordering::Relaxed);
        }
    }

    Ok(HlsBatchProgress {
        next_sequence: next_processed,
        skipped_failure: None,
    })
}

struct DownloadedHlsSegment {
    segment: HlsSegment,
    data: bytes::Bytes,
    subtitle: Option<HlsSegment>,
    subtitle_data: Option<bytes::Bytes>,
}

async fn process_downloaded_segment(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    downloaded: DownloadedHlsSegment,
    ctx: &mut TsContext,
) -> crate::Result<()> {
    let DownloadedHlsSegment {
        segment,
        data,
        subtitle,
        subtitle_data,
    } = downloaded;
    task.bytes_done
        .fetch_add(data.len() as u64, Ordering::Relaxed);

    let key_marker = hls_ts_key_marker(&segment.key);
    if key_changed(ctx.last_key_marker.as_ref(), key_marker.as_ref()) {
        flush_ts_aggregate(task, ctx);
    }
    if segment.discontinuity {
        flush_ts_aggregate(task, ctx);
        ctx.pending_discontinuity = true;
    }
    ctx.last_key_marker = key_marker;

    let clear = match &segment.key {
        HlsSegmentKey::None => data.to_vec(),
        HlsSegmentKey::Aes128 { uri, iv } => {
            let key = match ctx.configured_key {
                Some(k) => k,
                None => resolve_key(uri, dl, &mut ctx.key_cache).await?,
            };
            let iv = iv.unwrap_or_else(|| iv_from_media_sequence(segment.sequence));
            decrypt_aes128_cbc_pkcs7(&data, key, iv)?
        }
        HlsSegmentKey::SampleAesCtr { key_format, .. } => {
            return Err(anyhow::anyhow!(
                "HLS SAMPLE-AES-CTR (keyformat={key_format}) requires fMP4 EXT-X-MAP input"
            ));
        }
        HlsSegmentKey::SampleAes { key_format, .. } => {
            return Err(anyhow::anyhow!(
                "HLS SAMPLE-AES (keyformat={key_format}) requires fMP4 EXT-X-MAP input"
            ));
        }
        HlsSegmentKey::Unsupported { method, key_format } => {
            return Err(anyhow::anyhow!(
                "unsupported HLS encryption method {method} (keyformat={key_format})"
            ));
        }
    };

    let single_duration = segment.duration;
    ctx.agg_data.extend_from_slice(&clear);
    ctx.agg_duration += single_duration;
    if let Some(data) = subtitle_data {
        if let Err(e) = ctx
            .subtitle_acc
            .append_fragment(&data, single_duration, 1_000, None, None)
        {
            tracing::warn!(task=%task.id, seq=segment.sequence, "HLS subtitle conversion failed: {e:#}");
            ctx.subtitle_acc.append_empty(single_duration);
        }
    } else {
        let _ = subtitle;
        ctx.subtitle_acc.append_empty(single_duration);
    }
    if ctx.agg_duration >= AGGREGATE_TARGET_S || single_duration >= SHORT_SEG_THRESHOLD_S {
        flush_ts_aggregate(task, ctx);
    }
    tracing::debug!(task=%task.id, seq=segment.sequence, "hls segment published");
    Ok(())
}

fn flush_ts_aggregate(task: &Arc<Task>, ctx: &mut TsContext) {
    if ctx.agg_data.is_empty() {
        return;
    }
    let data = std::mem::take(&mut ctx.agg_data);
    let duration = ctx.agg_duration;
    ctx.agg_duration = 0.0;
    let subtitle_body = ctx.subtitle_acc.take_body();
    if ctx.pending_discontinuity {
        task.hls
            .push_segment_discontinuity(duration, bytes::Bytes::from(data));
        if task.subtitles_enabled {
            task.subtitles
                .push_segment_discontinuity(duration, subtitle_body);
        }
        ctx.pending_discontinuity = false;
    } else {
        task.hls.push_segment(duration, bytes::Bytes::from(data));
        if task.subtitles_enabled {
            task.subtitles.push_segment(duration, subtitle_body);
        }
    }
    task.segments_done.fetch_add(1, Ordering::Relaxed);
}

fn mark_ts_live_gap_boundary(task: &Arc<Task>, ctx: &mut TsContext, reason: &str) {
    flush_ts_aggregate(task, ctx);
    ctx.pending_discontinuity = true;
    ctx.subtitle_acc.clear();
    ctx.last_key_marker = None;
    tracing::warn!(task = %task.id, reason, "starting a new HLS TS output discontinuity epoch");
}

fn hls_ts_key_marker(key: &HlsSegmentKey) -> Option<String> {
    match key {
        HlsSegmentKey::None => None,
        HlsSegmentKey::Aes128 { uri, .. } => Some(format!("aes128:{uri}")),
        HlsSegmentKey::SampleAesCtr { uri, .. } => Some(format!("sample-aes-ctr:{uri:?}")),
        HlsSegmentKey::SampleAes { uri, .. } => Some(format!("sample-aes:{uri:?}")),
        HlsSegmentKey::Unsupported { method, key_format } => {
            Some(format!("unsupported:{method}:{key_format}"))
        }
    }
}

async fn resolve_key(
    uri: &str,
    dl: &crate::fetch::SharedDownloader,
    key_cache: &mut HashMap<String, [u8; 16]>,
) -> crate::Result<[u8; 16]> {
    if let Some(k) = key_cache.get(uri).copied() {
        return Ok(k);
    }

    let body = dl.get(uri).await?;
    let key = parse_key_body(&body)
        .ok_or_else(|| anyhow::anyhow!("HLS key URI returned unexpected body length"))?;
    key_cache.insert(uri.to_string(), key);
    Ok(key)
}

fn first_configured_key(text: &str) -> Option<[u8; 16]> {
    text.lines().find_map(parse_key_hex)
}

fn parse_key_body(body: &[u8]) -> Option<[u8; 16]> {
    if body.len() == 16 {
        let mut key = [0u8; 16];
        key.copy_from_slice(body);
        return Some(key);
    }
    let text = std::str::from_utf8(body).ok()?;
    text.lines().find_map(parse_key_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_key_accepts_kid_key_line() {
        let key = first_configured_key(
            "00112233445566778899aabbccddeeff:d493d5a70c793362324638f61d1726ac",
        )
        .unwrap();
        assert_eq!(key[0], 0xd4);
        assert_eq!(key[15], 0xac);
    }

    #[test]
    fn key_body_accepts_raw_or_hex() {
        assert_eq!(parse_key_body(&[7u8; 16]).unwrap(), [7u8; 16]);
        let key = parse_key_body(b"kid:d493d5a70c793362324638f61d1726ac\n").unwrap();
        assert_eq!(key[0], 0xd4);
    }

    #[test]
    fn hls_fmp4_audio_codec_detection_distinguishes_ac3_ec3_and_aac() {
        assert_eq!(detect_audio_codec("hvc1.2.4.L153.90,ac-3"), AudioCodec::Ac3);
        assert_eq!(detect_audio_codec("hvc1.2.4.L153.90,ec-3"), AudioCodec::Ec3);
        assert_eq!(
            detect_audio_codec("avc1.64001f,mp4a.40.2"),
            AudioCodec::AacAdts
        );
    }

    #[test]
    fn fmp4_jobs_wait_for_matching_audio_sequence() {
        let selected = SelectedHlsInput {
            video_url: "https://cdn/v.m3u8".to_string(),
            video: playlist(vec![10, 11, 12]),
            audio_url: Some("https://cdn/a.m3u8".to_string()),
            audio: Some(playlist(vec![10])),
            subtitle_url: None,
            subtitle: None,
        };
        let jobs = build_fmp4_jobs(&selected, 10, &HashSet::new());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].sequence, 10);
        assert!(jobs[0].audio.is_some());
    }

    #[test]
    fn fmp4_jobs_allow_video_only_when_no_audio_track_selected() {
        let selected = SelectedHlsInput {
            video_url: "https://cdn/v.m3u8".to_string(),
            video: playlist(vec![10, 11]),
            audio_url: None,
            audio: None,
            subtitle_url: None,
            subtitle: None,
        };
        let jobs = build_fmp4_jobs(&selected, 10, &HashSet::new());
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.audio.is_none()));
    }

    #[test]
    fn hls_media_sequence_rewind_starts_new_epoch() {
        let old = playlist(vec![100, 101, 102]);
        let fresh = playlist(vec![10, 11, 12]);
        assert!(hls_media_sequence_rewound(&old, &fresh));
        assert!(!hls_media_sequence_rewound(&fresh, &old));
    }

    fn playlist(sequences: Vec<u64>) -> HlsMediaPlaylist {
        HlsMediaPlaylist {
            uri: "https://cdn/p.m3u8".to_string(),
            target_duration: 6,
            media_sequence: sequences.first().copied().unwrap_or(0),
            end_list: false,
            has_map: true,
            map_uri: Some("https://cdn/init.mp4".to_string()),
            segments: sequences
                .into_iter()
                .map(|sequence| HlsSegment {
                    sequence,
                    uri: format!("https://cdn/{sequence}.m4s"),
                    duration: 6.0,
                    key: HlsSegmentKey::None,
                    discontinuity: false,
                })
                .collect(),
        }
    }
}
