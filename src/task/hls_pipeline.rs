//! HLS input pipeline:
//! - TS media playlist: segment download → optional AES-128-CBC decrypt → publish.
//! - fMP4/CMAF SAMPLE-AES-CTR: CENC decrypt → MP4 samples → TS mux.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::StreamExt;

use crate::clock::ClockState;
use crate::crypto::cenc::{parse_key_hex, Decryptor, KeyStore};
use crate::crypto::hls::{decrypt_aes128_cbc_pkcs7, iv_from_media_sequence};
use crate::crypto::playready::kid_from_playready_data_uri;
use crate::hevc::annexb::AccessUnit;
use crate::hls::{
    parse_playlist, HlsMaster, HlsMediaPlaylist, HlsPlaylist, HlsRendition, HlsSegment,
    HlsSegmentKey,
};
use crate::mp4::protection::{ensure_clear_fragment, ensure_clear_init, ProtectedInput};
use crate::mp4::sample::{parse_media_segment_for_track, ParsedSegment, SampleDefaults};
use crate::mp4::tracks::{tracks_from_init, InitTrack};
use crate::mp4::{DoviConfig, ParamSets, TrackEncryption};
use crate::segment::OutputLimitExceeded;
use crate::subtitle::SubtitleAccumulator;
use crate::ts::muxer::{AudioUnit, TsMuxer};
use crate::ts::packet::first_video_pts_90k;
use crate::ts::{VideoCodec, SYNC_BYTE, TS_PACKET_SIZE};

use super::{
    audio::{Fmp4AudioConfig, UnsupportedAc4CompositionOffset},
    key_resolver, live_tuning,
    manager::Task,
    KeyMode,
};

const SHORT_SEG_THRESHOLD_S: f64 = live_tuning::SHORT_SEG_THRESHOLD_S;
const AGGREGATE_TARGET_S: f64 = live_tuning::AGGREGATE_TARGET_S;
const MAX_HLS_AUDIO_WAIT_POLLS: u8 = 4;
const HLS_RENDITION_BOUNDARY_SLACK_SECS: f64 = 0.150;
const CLEAR_TS_FRAME_ATTEMPTS: usize = 2;
const MAX_HLS_AGGREGATE_BYTES: usize = 128 * 1024 * 1024;

pub(super) async fn run_hls_pipeline(
    task: Arc<Task>,
    dl: crate::fetch::SharedDownloader,
) -> crate::Result<()> {
    run_inner(&task, &dl, &task.sel).await
}

async fn run_inner(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    sel: &super::TrackSelection,
) -> crate::Result<()> {
    task.set_stage("解析 M3U8");
    let manifest = dl.get_manifest(&sel.mpd_url).await?;
    let selected = load_selected_media_playlist(&manifest.text, &manifest.url, sel, dl).await?;

    if selected.video.has_map {
        run_fmp4_inner(task, dl, sel, selected).await?;
    } else {
        run_ts_inner(task, dl, sel.key_mode, &sel.keys, selected).await?;
    }
    anyhow::ensure!(
        task.cancel_token().is_cancelled() || task.hls.segment_count() > 0,
        "HLS input ended without producing any video segments"
    );
    Ok(())
}

async fn run_ts_inner(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    key_mode: KeyMode,
    keys: &str,
    mut selected: SelectedHlsInput,
) -> crate::Result<()> {
    ensure_ts_audio_layout(&selected)?;
    task.total_segments
        .store(selected.video.segments.len() as u64, Ordering::Relaxed);
    if key_mode.is_clear() {
        ensure_clear_ts_subtitle_init(dl, selected.subtitle.as_ref()).await?;
    }
    let mut ctx = TsContext {
        clear_mode: key_mode.is_clear(),
        configured_key: if key_mode == KeyMode::Static {
            first_configured_key(keys)
        } else {
            None
        },
        key_cache: HashMap::new(),
        agg_data: Vec::new(),
        agg_duration: 0.0,
        agg_input_bytes: 0,
        agg_video_pts_90k: None,
        subtitle_acc: SubtitleAccumulator::new(),
        pending_discontinuity: false,
        last_key_marker: None,
    };
    let publish_delay = configure_live_publish_delay(task, &selected.video);
    let mut next_sequence = initial_sequence(task, &selected.video, publish_delay);
    let cancel = task.cancel_token();

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let cycle_started = std::time::Instant::now();

        task.set_stage("处理 HLS 分片");
        let jobs = build_ts_jobs(&selected, next_sequence);

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
                )?;
                next_sequence = skip.skip_to;
                skipped_live_gap = true;
            }
            task.resume_number.store(next_sequence, Ordering::Relaxed);
        }

        if !selected.video.is_live() {
            break;
        }

        if skipped_live_gap && has_hls_segment_at_or_after(&selected.video, next_sequence) {
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
                ensure_hls_input_layout(&fresh)?;
                ensure_ts_audio_layout(&fresh)?;
                if fresh.video.has_map {
                    anyhow::bail!("HLS video changed from TS to fMP4; restart the task");
                }
                if key_mode.is_clear() {
                    ensure_clear_hls_input(&fresh)?;
                    if fresh.subtitle.as_ref().and_then(|p| p.map_uri.as_deref())
                        != selected
                            .subtitle
                            .as_ref()
                            .and_then(|p| p.map_uri.as_deref())
                    {
                        ensure_clear_ts_subtitle_init(dl, fresh.subtitle.as_ref()).await?;
                    }
                }
                if hls_media_sequence_rewound(&selected.video, &fresh.video) {
                    flush_ts_aggregate(task, &mut ctx)?;
                    ctx.pending_discontinuity = true;
                    next_sequence = first_hls_sequence(&fresh.video);
                    task.resume_number.store(next_sequence, Ordering::Relaxed);
                } else if let Some(first) = hls_expired_cursor(&fresh.video, next_sequence) {
                    mark_ts_live_gap_boundary(
                        task,
                        &mut ctx,
                        "HLS TS window skipped unprocessed segments",
                    )?;
                    next_sequence = first;
                    task.resume_number.store(next_sequence, Ordering::Relaxed);
                }
                task.total_segments
                    .store(fresh.video.segments.len() as u64, Ordering::Relaxed);
                selected = fresh;
            }
            Err(e) => tracing::warn!(task=%task.id, "refresh hls playlist failed: {e:#}"),
        }
    }

    flush_ts_aggregate(task, &mut ctx)
}

struct SelectedHlsInput {
    video_url: String,
    video: HlsMediaPlaylist,
    audio_url: Option<String>,
    audio: Option<HlsMediaPlaylist>,
    expected_in_band_audio: bool,
    subtitle_url: Option<String>,
    subtitle: Option<HlsMediaPlaylist>,
}

pub(crate) fn ensure_clear_hls_master(master: &HlsMaster) -> crate::Result<()> {
    if master.has_session_key {
        return Err(ProtectedInput::new("HLS master", "EXT-X-SESSION-KEY").into());
    }
    Ok(())
}

pub(crate) fn ensure_clear_hls_media(
    playlist: &HlsMediaPlaylist,
    track: &str,
) -> crate::Result<()> {
    if playlist.has_encryption {
        return Err(ProtectedInput::new(
            &format!("HLS {track} playlist"),
            "HLS encryption tags (EXT-X-KEY/EXT-X-SESSION-KEY)",
        )
        .into());
    }
    if playlist.has_multiple_maps {
        anyhow::bail!(
            "HLS {track} playlist contains multiple init maps, which Clear mode cannot verify"
        );
    }
    if playlist.has_map && playlist.map_uri.is_none() {
        anyhow::bail!("HLS {track} playlist has EXT-X-MAP without a URI");
    }
    Ok(())
}

fn ensure_clear_hls_input(selected: &SelectedHlsInput) -> crate::Result<()> {
    ensure_clear_hls_media(&selected.video, "video")?;
    if let Some(audio) = &selected.audio {
        ensure_clear_hls_media(audio, "audio")?;
    }
    if let Some(subtitle) = &selected.subtitle {
        ensure_clear_hls_media(subtitle, "subtitle")?;
    }
    Ok(())
}

async fn ensure_clear_ts_subtitle_init(
    dl: &crate::fetch::SharedDownloader,
    subtitle: Option<&HlsMediaPlaylist>,
) -> crate::Result<()> {
    if let Some(map_uri) = subtitle.and_then(|playlist| playlist.map_uri.as_deref()) {
        ensure_clear_init(&dl.get(map_uri).await?, "HLS subtitle")?;
    }
    Ok(())
}

async fn load_selected_media_playlist(
    source_text: &str,
    source_url: &str,
    sel: &super::TrackSelection,
    dl: &crate::fetch::SharedDownloader,
) -> crate::Result<SelectedHlsInput> {
    let selected = match parse_playlist(source_text, source_url)? {
        HlsPlaylist::Media(media) => {
            anyhow::ensure!(
                !sel.audio_rep_id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty()),
                "HLS Media Playlist has no independently selectable audio rendition"
            );
            SelectedHlsInput {
                video_url: sel.mpd_url.clone(),
                video: media,
                audio_url: None,
                audio: None,
                expected_in_band_audio: false,
                subtitle_url: None,
                subtitle: None,
            }
        }
        HlsPlaylist::Master(master) => {
            if sel.key_mode.is_clear() {
                ensure_clear_hls_master(&master)?;
            }
            let variant = master
                .variants
                .iter()
                .find(|v| v.id == sel.video_rep_id)
                .ok_or_else(|| anyhow::anyhow!("HLS variant {} not found", sel.video_rep_id))?;
            let manifest = dl.get_manifest(&variant.uri).await?;
            let video = match parse_playlist(&manifest.text, &manifest.url)? {
                HlsPlaylist::Media(media) => media,
                HlsPlaylist::Master(_) => Err(anyhow::anyhow!(
                    "nested HLS master playlists are not supported"
                ))?,
            };

            let audio_rendition = select_audio_rendition(
                &master.audio,
                variant.audio_group.as_deref(),
                sel.audio_rep_id.as_deref(),
            )?;
            let subtitle_rendition = select_subtitle_rendition(
                &master.subtitles,
                variant.subtitles_group.as_deref(),
                sel.enable_subtitles,
                sel.subtitle_rep_id.as_deref(),
            )?;
            let expected_in_band_audio = audio_rendition.is_some_and(|r| r.uri.is_empty());
            let (audio_url, audio) = match audio_rendition.filter(|r| !r.uri.is_empty()) {
                Some(r) => {
                    let manifest = dl.get_manifest(&r.uri).await?;
                    let media = match parse_playlist(&manifest.text, &manifest.url)? {
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
                    let manifest = dl.get_manifest(&r.uri).await?;
                    let media = match parse_playlist(&manifest.text, &manifest.url)? {
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

            SelectedHlsInput {
                video_url: variant.uri.clone(),
                video,
                audio_url,
                audio,
                expected_in_band_audio,
                subtitle_url,
                subtitle,
            }
        }
    };
    ensure_hls_input_layout(&selected)?;
    if sel.key_mode.is_clear() {
        ensure_clear_hls_input(&selected)?;
    }
    Ok(selected)
}

fn ensure_hls_input_layout(selected: &SelectedHlsInput) -> crate::Result<()> {
    for (track, playlist) in [
        ("video", Some(&selected.video)),
        ("audio", selected.audio.as_ref()),
        ("subtitle", selected.subtitle.as_ref()),
    ] {
        let Some(playlist) = playlist else {
            continue;
        };
        anyhow::ensure!(
            !playlist.has_byte_ranges,
            "HLS {track} uses byte-range segments or init maps, which are not supported"
        );
        anyhow::ensure!(
            !playlist.has_multiple_maps && (!playlist.has_map || playlist.map_uri.is_some()),
            "HLS {track} has multiple or missing init maps, which are not supported"
        );
    }
    Ok(())
}

fn ensure_ts_audio_layout(selected: &SelectedHlsInput) -> crate::Result<()> {
    anyhow::ensure!(
        selected.audio.is_none(),
        "HLS MPEG-TS input with a separate audio rendition is not supported"
    );
    Ok(())
}

fn select_audio_rendition<'a>(
    audio: &'a [HlsRendition],
    group: Option<&str>,
    selected_audio_id: Option<&str>,
) -> crate::Result<Option<&'a HlsRendition>> {
    let Some(group) = group else {
        anyhow::ensure!(
            selected_audio_id.is_none_or(|id| id.trim().is_empty()),
            "selected HLS variant has no AUDIO group"
        );
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

    audio
        .iter()
        .find(|r| r.group_id == group && r.is_default)
        .or_else(|| audio.iter().find(|r| r.group_id == group))
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("selected HLS AUDIO group {group} has no rendition"))
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
    let manifest = dl.get_manifest(&old.video_url).await?;
    let video = match parse_playlist(&manifest.text, &manifest.url)? {
        HlsPlaylist::Media(media) => media,
        HlsPlaylist::Master(_) => return Err(anyhow::anyhow!("HLS media URL refreshed as master")),
    };

    let audio = match &old.audio_url {
        Some(url) => {
            let manifest = dl.get_manifest(url).await?;
            match parse_playlist(&manifest.text, &manifest.url)? {
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
            let manifest = dl.get_manifest(url).await?;
            match parse_playlist(&manifest.text, &manifest.url)? {
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
        expected_in_band_audio: old.expected_in_band_audio,
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
    task.subtitles.set_publish_delay_segments(delay);
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

struct RenditionTimeline<'a> {
    playlist: &'a HlsMediaPlaylist,
    elapsed_starts: Vec<f64>,
    dated: Vec<(u64, f64, &'a HlsSegment)>,
}

impl<'a> RenditionTimeline<'a> {
    fn new(playlist: &'a HlsMediaPlaylist, video_ended: bool) -> Self {
        let mut elapsed = 0.0;
        let elapsed_starts = if video_ended && playlist.end_list {
            playlist
                .segments
                .iter()
                .map(|seg| {
                    let start = elapsed;
                    elapsed += seg.duration;
                    start
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut dated: Vec<_> = playlist
            .segments
            .iter()
            .filter_map(|seg| {
                seg.program_date_time
                    .map(|start| (seg.discontinuity_sequence, start, seg))
            })
            .collect();
        dated.sort_by(|(ae, at, _), (be, bt, _)| ae.cmp(be).then_with(|| at.total_cmp(bt)));
        Self {
            playlist,
            elapsed_starts,
            dated,
        }
    }

    /// A sequence-only guess is not a timing guarantee; audio using that
    /// fallback is checked against the actual sample timestamps after fetch.
    fn pair(
        &self,
        video: &HlsSegment,
        video_index: usize,
        elapsed: f64,
    ) -> Option<(&'a HlsSegment, bool)> {
        let same_epoch =
            |seg: &HlsSegment| seg.discontinuity_sequence == video.discontinuity_sequence;
        if let Some(video_start) = video.program_date_time.filter(|_| !self.dated.is_empty()) {
            let epoch = video.discontinuity_sequence;
            let start = self.dated.partition_point(|(id, time, _)| {
                *id < epoch
                    || (*id == epoch && *time < video_start - HLS_RENDITION_BOUNDARY_SLACK_SECS)
            });
            return self.dated[start..]
                .iter()
                .take_while(|(id, time, _)| {
                    *id == epoch && *time <= video_start + HLS_RENDITION_BOUNDARY_SLACK_SECS
                })
                .filter(|(_, time, seg)| rendition_boundaries_match(video, video_start, seg, *time))
                .min_by(|(_, a, _), (_, b, _)| {
                    (a - video_start).abs().total_cmp(&(b - video_start).abs())
                })
                .map(|(_, _, seg)| (*seg, false));
        }
        if !self.elapsed_starts.is_empty() {
            return self
                .playlist
                .segments
                .get(video_index)
                .filter(|seg| {
                    rendition_boundaries_match(
                        video,
                        elapsed,
                        seg,
                        self.elapsed_starts[video_index],
                    )
                })
                .map(|seg| (seg, false));
        }
        hls_segment_by_sequence(self.playlist, video.sequence)
            .filter(|seg| {
                same_epoch(seg)
                    && (seg.duration - video.duration).abs() <= HLS_RENDITION_BOUNDARY_SLACK_SECS
            })
            .map(|seg| (seg, true))
    }

    fn has_dated_epoch(&self, video: &HlsSegment) -> bool {
        let epoch = video.discontinuity_sequence;
        let first = self.dated.partition_point(|(id, _, _)| *id < epoch);
        self.dated.get(first).is_some_and(|(id, _, _)| *id == epoch)
    }

    fn pair_fmp4_audio(
        &self,
        video: &HlsSegment,
        video_index: usize,
        elapsed: f64,
        ended_offsets: &BTreeMap<u64, f64>,
    ) -> Option<(&'a HlsSegment, bool)> {
        if self.elapsed_starts.is_empty()
            || (video.program_date_time.is_some() && self.has_dated_epoch(video))
        {
            return self.pair(video, video_index, elapsed);
        }

        // ENDLIST is not a shared presentation origin: the two renditions
        // can retain differently-sized final windows. The measured offset
        // between their media clocks identifies an elapsed-time candidate;
        // verify the candidate's actual sample timestamps before publishing.
        let target = elapsed + ended_offsets.get(&video.discontinuity_sequence)?;
        let first = self
            .elapsed_starts
            .partition_point(|time| *time < target - HLS_RENDITION_BOUNDARY_SLACK_SECS);
        self.elapsed_starts[first..]
            .iter()
            .zip(&self.playlist.segments[first..])
            .take_while(|(time, _)| **time <= target + HLS_RENDITION_BOUNDARY_SLACK_SECS)
            .filter(|(time, seg)| rendition_boundaries_match(video, target, seg, **time))
            .min_by(|(a, _), (b, _)| (*a - target).abs().total_cmp(&(*b - target).abs()))
            .map(|(_, seg)| (seg, true))
    }
}

fn rendition_boundaries_match(
    video: &HlsSegment,
    video_start: f64,
    seg: &HlsSegment,
    start: f64,
) -> bool {
    seg.discontinuity_sequence == video.discontinuity_sequence
        && (start - video_start).abs() <= HLS_RENDITION_BOUNDARY_SLACK_SECS
        && ((start + seg.duration) - (video_start + video.duration)).abs()
            <= HLS_RENDITION_BOUNDARY_SLACK_SECS
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
    if resume != 0 {
        resume.max(first)
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

fn first_hls_sequence(playlist: &HlsMediaPlaylist) -> u64 {
    playlist
        .segments
        .first()
        .map_or(playlist.media_sequence, |seg| seg.sequence)
}

fn hls_expired_cursor(playlist: &HlsMediaPlaylist, next_sequence: u64) -> Option<u64> {
    let first = playlist.segments.first()?.sequence;
    (first > next_sequence).then_some(first)
}

fn build_ts_jobs(selected: &SelectedHlsInput, next_sequence: u64) -> Vec<HlsTsJob> {
    let subtitle_timing = selected
        .subtitle
        .as_ref()
        .map(|subtitles| RenditionTimeline::new(subtitles, selected.video.end_list));
    let mut video_start = 0.0;
    selected
        .video
        .segments
        .iter()
        .enumerate()
        .filter_map(|(index, video)| {
            let start = video_start;
            video_start += video.duration;
            if video.sequence < next_sequence {
                return None;
            }
            let subtitle = subtitle_timing
                .as_ref()
                .and_then(|timeline| timeline.pair(video, index, start))
                .map(|(seg, _)| seg.clone());
            Some(HlsTsJob {
                video: video.clone(),
                subtitle,
            })
        })
        .collect()
}

struct TsContext {
    clear_mode: bool,
    configured_key: Option<[u8; 16]>,
    key_cache: HashMap<String, [u8; 16]>,
    agg_data: Vec<u8>,
    agg_duration: f64,
    agg_input_bytes: usize,
    agg_video_pts_90k: Option<u64>,
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

#[derive(Debug, thiserror::Error)]
#[error("HLS audio and video sample clocks disagree for video sequence {0}")]
struct RenditionTimingMismatch(u64);

#[derive(Debug, thiserror::Error)]
#[error("HLS output has subtitle cues but no video PTS for their time map")]
struct SubtitleTimingUnavailable;

#[derive(Debug, thiserror::Error)]
#[error(
    "HLS input fragment exceeds the {MAX_HLS_AGGREGATE_BYTES}-byte aggregation budget ({0} bytes)"
)]
struct AggregateLimitExceeded(usize);

fn aggregate_needs_flush(used: usize, next: usize) -> crate::Result<bool> {
    if next > MAX_HLS_AGGREGATE_BYTES {
        return Err(AggregateLimitExceeded(next).into());
    }
    Ok(used > MAX_HLS_AGGREGATE_BYTES - next)
}

fn is_permanent_segment_error(err: &anyhow::Error) -> bool {
    err.is::<ProtectedInput>()
        || err.is::<OutputLimitExceeded>()
        || err.is::<RenditionTimingMismatch>()
        || err.is::<SubtitleTimingUnavailable>()
        || err.is::<AggregateLimitExceeded>()
        || err.is::<UnsupportedAc4CompositionOffset>()
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
    let mut audio_wait_polls = 0u8;
    let cancel = task.cancel_token();

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let cycle_started = std::time::Instant::now();

        task.set_stage("转封装 HLS fMP4");
        let ended_offsets =
            probe_ended_audio_offsets(task, dl, &selected, &ctx, next_sequence).await?;
        let (jobs, blocked_video) = build_fmp4_jobs(&selected, next_sequence, &ended_offsets);
        let mut skipped_live_gap = false;
        if !jobs.is_empty() {
            audio_wait_polls = 0;
            ensure_fmp4_dynamic_keys_for_jobs(task, sel, &jobs, &mut ctx).await?;
            let progress =
                process_fmp4_batch(task, dl, jobs, &mut ctx, selected.video.is_live()).await?;
            next_sequence = progress.next_sequence;
            if let Some(skip) = progress.skipped_failure {
                mark_fmp4_epoch_boundary(
                    task,
                    &mut ctx,
                    &format!("HLS fMP4 live segment {} failed", skip.sequence),
                )?;
                next_sequence = skip.skip_to;
                skipped_live_gap = true;
            }
            task.resume_number.store(next_sequence, Ordering::Relaxed);
        }

        if let Some(seq) = blocked_video.filter(|&seq| seq == next_sequence) {
            audio_wait_polls = audio_wait_polls.saturating_add(1);
            if (!selected.video.is_live() || audio_wait_polls >= MAX_HLS_AUDIO_WAIT_POLLS)
                && !cancel.is_cancelled()
            {
                anyhow::bail!("HLS fMP4 audio cannot be aligned to video segment {seq}");
            }
        }
        if !selected.video.is_live() {
            break;
        }

        if skipped_live_gap && has_hls_segment_at_or_after(&selected.video, next_sequence) {
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
                ensure_hls_input_layout(&fresh)?;
                if sel.key_mode.is_clear() {
                    ensure_clear_hls_input(&fresh)?;
                }
                if hls_media_sequence_rewound(&selected.video, &fresh.video) {
                    mark_fmp4_epoch_boundary(task, &mut ctx, "HLS MEDIA-SEQUENCE rewound")?;
                    audio_wait_polls = 0;
                    next_sequence = first_hls_sequence(&fresh.video);
                    task.resume_number.store(next_sequence, Ordering::Relaxed);
                } else if let Some(first) = hls_expired_cursor(&fresh.video, next_sequence) {
                    mark_fmp4_epoch_boundary(
                        task,
                        &mut ctx,
                        "HLS fMP4 window skipped unprocessed segments",
                    )?;
                    next_sequence = first;
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

    flush_fmp4_aggregate(task, &mut ctx)
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

fn has_hls_segment_at_or_after(playlist: &HlsMediaPlaylist, sequence: u64) -> bool {
    playlist.segments.iter().any(|seg| seg.sequence >= sequence)
}

fn hls_skip_to_after(job_order: &[u64], index: usize, sequence: u64) -> u64 {
    job_order
        .get(index.saturating_add(1))
        .copied()
        .unwrap_or_else(|| sequence.saturating_add(1))
}

struct Fmp4Context {
    key_mode: KeyMode,
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
    video_track: Fmp4Track,
    audio_track: Option<Fmp4Track>,
    in_band_audio: bool,
    subtitle_timescale: u32,
    audio_config: Option<Fmp4AudioConfig>,
    muxer: TsMuxer,
    acc_v_dts: u64,
    acc_a_dts: u64,
    agg_vaus: Vec<AccessUnit>,
    agg_aaus: Vec<AudioUnit>,
    subtitle_acc: SubtitleAccumulator,
    agg_v_dur_ts: u64,
    agg_a_dur_ts: u64,
    agg_duration: f64,
    agg_input_bytes: usize,
    pending_discontinuity: bool,
    last_video_kid: Option<String>,
    last_audio_kid: Option<String>,
    last_subtitle_kid: Option<String>,
    last_v_tfdt: Option<u64>,
    last_a_tfdt: Option<u64>,
}

#[derive(Clone, Copy)]
struct Fmp4Track {
    id: u32,
    timescale: u32,
    defaults: SampleDefaults,
}

impl From<&InitTrack<'_>> for Fmp4Track {
    fn from(track: &InitTrack<'_>) -> Self {
        Self {
            id: track.track_id,
            timescale: track.timescale,
            defaults: track.defaults,
        }
    }
}

impl Fmp4Track {
    fn parse(self, data: &[u8], iv_size: Option<u8>) -> Option<ParsedSegment> {
        parse_media_segment_for_track(data, self.id, iv_size, self.defaults)
    }
}

fn find_unique_init_track<'a, 'b>(
    tracks: &'b [InitTrack<'a>],
    handler: &[u8; 4],
) -> crate::Result<Option<&'b InitTrack<'a>>> {
    let mut matching = tracks.iter().filter(|track| &track.handler == handler);
    let first = matching.next();
    anyhow::ensure!(
        matching.next().is_none(),
        "HLS fMP4 init advertises multiple {} tracks where exactly one is supported",
        String::from_utf8_lossy(handler)
    );
    Ok(first)
}

fn hls_video_config(track: &InitTrack<'_>) -> crate::Result<(VideoCodec, ParamSets)> {
    let (codec, params) = match track.codec.as_ref() {
        Some(b"avc1" | b"avc3") => (
            VideoCodec::H264,
            crate::mp4::avcc::find_avcc_in_init(track.trak).unwrap_or_default(),
        ),
        Some(b"hev1" | b"hvc1" | b"dvh1" | b"dvhe") => (
            VideoCodec::Hevc,
            ParamSets::find_in_init(track.trak).unwrap_or_default(),
        ),
        Some(b"encv") => {
            if let Some(avcc) = crate::mp4::avcc::find_avcc_in_init(track.trak) {
                (VideoCodec::H264, avcc)
            } else if let Some(hvcc) = ParamSets::find_in_init(track.trak) {
                (VideoCodec::Hevc, hvcc)
            } else {
                anyhow::bail!("HLS fMP4 init is missing supported video codec configuration");
            }
        }
        _ => anyhow::bail!("HLS fMP4 video sample entry is not supported"),
    };
    Ok((codec, params))
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
    ensure_hls_init_for_mode(sel.key_mode, &vinit, "HLS video")?;
    let video_init_tracks = tracks_from_init(&vinit)
        .ok_or_else(|| anyhow::anyhow!("HLS video init has malformed track metadata"))?;
    let video_init_track = find_unique_init_track(&video_init_tracks, b"vide")?
        .ok_or_else(|| anyhow::anyhow!("HLS init has no video track"))?;
    let video_track = Fmp4Track::from(video_init_track);
    let (video_codec, params) = hls_video_config(video_init_track)?;
    let dovi = DoviConfig::find_in_init(video_init_track.trak);

    let vtenc = TrackEncryption::find_in_init(video_init_track.trak);
    let vkid = vtenc.as_ref().map(|t| t.kid.clone());
    let v_default_iv_size = vtenc.as_ref().map(|t| t.iv_size);

    let mut audio_config = None;
    let mut audio_track = None;
    let mut in_band_audio = false;
    let mut a_default_iv_size = None;
    let mut atenc = None;
    let mut akid = None;
    if let Some(audio_playlist) = &selected.audio {
        let ainit_url = audio_playlist
            .map_uri
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("HLS fMP4 audio playlist is missing EXT-X-MAP URI"))?;
        let ainit = dl.get(ainit_url).await?;
        ensure_hls_init_for_mode(sel.key_mode, &ainit, "HLS audio")?;
        let tracks = tracks_from_init(&ainit)
            .ok_or_else(|| anyhow::anyhow!("HLS audio init has malformed track metadata"))?;
        let track = find_unique_init_track(&tracks, b"soun")?
            .ok_or_else(|| anyhow::anyhow!("HLS audio rendition init has no audio track"))?;
        audio_track = Some(Fmp4Track::from(track));
        audio_config = Some(
            Fmp4AudioConfig::from_init(track.trak).map_err(|e| e.context("HLS fMP4 audio init"))?,
        );

        atenc = TrackEncryption::find_in_init(track.trak);
        akid = atenc.as_ref().map(|t| t.kid.clone());
        a_default_iv_size = atenc.as_ref().map(|t| t.iv_size);
    } else if let Some(track) = find_unique_init_track(&video_init_tracks, b"soun")? {
        anyhow::ensure!(
            !selected.video.has_encryption
                && !vtenc.as_ref().is_some_and(|tenc| tenc.is_protected)
                && track.codec.as_ref() != Some(b"enca")
                && !TrackEncryption::find_in_init(track.trak).is_some_and(|tenc| tenc.is_protected),
            "HLS fMP4 with encrypted in-band audio is not supported"
        );
        audio_config = Some(
            Fmp4AudioConfig::from_init(track.trak).map_err(|e| e.context("HLS fMP4 audio init"))?,
        );
        audio_track = Some(Fmp4Track::from(track));
        in_band_audio = true;
    }
    anyhow::ensure!(
        !selected.expected_in_band_audio || in_band_audio,
        "HLS variant advertises in-band audio but its init has no audio track"
    );
    let audio_codec = audio_config.map(Fmp4AudioConfig::codec);
    let audio_timescale = audio_track.map_or(48_000, |track| track.timescale);
    let mut s_default_iv_size = None;
    let mut stenc = None;
    let mut skid = None;
    let mut subtitle_timescale = 1_000;
    if let Some(subtitle_playlist) = &selected.subtitle {
        if let Some(sinit_url) = &subtitle_playlist.map_uri {
            let sinit = dl.get(sinit_url).await?;
            ensure_hls_init_for_mode(sel.key_mode, &sinit, "HLS subtitle")?;
            subtitle_timescale = crate::mp4::mdhd::timescale_from_init(&sinit)
                .filter(|&scale| scale > 0)
                .unwrap_or(subtitle_timescale);
            stenc = TrackEncryption::find_in_init(&sinit);
            skid = stenc.as_ref().map(|t| t.kid.clone());
            s_default_iv_size = stenc.as_ref().map(|t| t.iv_size);
        }
    }

    task.set_stage(if sel.key_mode.is_clear() {
        "无需 KEY"
    } else if sel.key_mode.is_dynamic() {
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

    let min_cts = scan_min_cts_hls(dl, &selected.video, video_track, v_default_iv_size)
        .await
        .unwrap_or(0);
    let clock = ClockState::new(min_cts, video_track.timescale, 90_000);
    let muxer = TsMuxer::new(
        &params,
        video_codec,
        dovi,
        audio_codec,
        audio_timescale,
        clock,
    )
    .with_preserved_audio_lead(in_band_audio);

    task.hls
        .observe_output_codecs(super::output_codecs::verified_fmp4_codecs(
            video_init_track.codec,
            &params,
            dovi.is_some(),
            audio_config,
        ));

    Ok(Fmp4Context {
        key_mode: sel.key_mode,
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
        video_track,
        audio_track,
        in_band_audio,
        subtitle_timescale,
        audio_config,
        muxer,
        acc_v_dts: task.acc_v_dts.load(Ordering::Relaxed),
        acc_a_dts: task.acc_a_dts.load(Ordering::Relaxed),
        agg_vaus: Vec::new(),
        agg_aaus: Vec::new(),
        subtitle_acc: SubtitleAccumulator::new(),
        agg_v_dur_ts: 0,
        agg_a_dur_ts: 0,
        agg_duration: 0.0,
        agg_input_bytes: 0,
        pending_discontinuity: false,
        last_video_kid: None,
        last_audio_kid: None,
        last_subtitle_kid: None,
        last_v_tfdt: None,
        last_a_tfdt: None,
    })
}

fn ensure_hls_init_for_mode(key_mode: KeyMode, data: &[u8], track: &str) -> crate::Result<()> {
    if key_mode.is_clear() {
        ensure_clear_init(data, track)?;
    }
    Ok(())
}

async fn refresh_fmp4_init_if_needed(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    sel: &super::TrackSelection,
    fresh: &SelectedHlsInput,
    ctx: &mut Fmp4Context,
) -> crate::Result<()> {
    let audio_map = fresh.audio.as_ref().and_then(|p| p.map_uri.as_ref());
    let subtitle_map = fresh.subtitle.as_ref().and_then(|p| p.map_uri.as_ref());
    if fresh.video.map_uri == ctx.v_map_uri
        && audio_map == ctx.a_map_uri.as_ref()
        && subtitle_map == ctx.s_map_uri.as_ref()
    {
        return Ok(());
    }

    // The new map can change much more than its key: track IDs, default
    // sample fields, parameter sets, codecs, timescales and mux signalling
    // must all belong to the same initialization segment.
    let mut replacement = setup_fmp4_context(task, dl, sel, fresh).await?;
    mark_fmp4_epoch_boundary(task, ctx, "HLS fMP4 EXT-X-MAP changed")?;
    replacement.acc_v_dts = 0;
    replacement.acc_a_dts = 0;
    replacement.pending_discontinuity = true;
    *ctx = replacement;
    Ok(())
}

fn mark_fmp4_epoch_boundary(
    task: &Arc<Task>,
    ctx: &mut Fmp4Context,
    reason: &str,
) -> crate::Result<()> {
    flush_fmp4_aggregate(task, ctx)?;
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
    Ok(())
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
    if ctx.key_mode.is_clear() {
        return Ok(None);
    }
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

fn first_segment_per_epoch(
    playlist: &HlsMediaPlaylist,
    next_sequence: u64,
) -> BTreeMap<u64, (&HlsSegment, f64)> {
    let mut starts = BTreeMap::new();
    let mut elapsed = 0.0;
    for segment in &playlist.segments {
        if segment.sequence >= next_sequence {
            starts
                .entry(segment.discontinuity_sequence)
                .or_insert((segment, elapsed));
        }
        elapsed += segment.duration;
    }
    starts
}

async fn probe_ended_audio_offsets(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    selected: &SelectedHlsInput,
    ctx: &Fmp4Context,
    next_sequence: u64,
) -> crate::Result<BTreeMap<u64, f64>> {
    let mut offsets = BTreeMap::new();
    let Some((audio, audio_track)) = selected
        .audio
        .as_ref()
        .zip(ctx.audio_track)
        .filter(|(p, _)| p.end_list && selected.video.end_list)
    else {
        return Ok(offsets);
    };

    let first_audios = first_segment_per_epoch(audio, 0);
    for (epoch, (video_first, video_elapsed)) in
        first_segment_per_epoch(&selected.video, next_sequence)
    {
        let Some(&(audio_first, audio_elapsed)) = first_audios.get(&epoch) else {
            continue;
        };
        if video_first.program_date_time.is_some() && audio_first.program_date_time.is_some() {
            continue;
        }
        // The identity and location of samples are needed, not decoded or
        // decrypted content. Probe once per needed epoch of the frozen lists.
        let fetch_pts = |track: Fmp4Track, iv_size, url: String| async move {
            let bytes = task.fetch_media(dl, &url).await?;
            track
                .parse(&bytes, iv_size)
                .as_ref()
                .and_then(earliest_fmp4_pts)
                .ok_or_else(|| anyhow::anyhow!("HLS fMP4 final window has no usable sample timing"))
        };
        let (v_pts, a_pts) = tokio::try_join!(
            fetch_pts(
                ctx.video_track,
                ctx.v_default_iv_size,
                video_first.uri.clone()
            ),
            fetch_pts(audio_track, ctx.a_default_iv_size, audio_first.uri.clone()),
        )?;
        let v_scale = ctx.video_track.timescale;
        let a_scale = audio_track.timescale;
        let delta = cross_scaled_pts_delta(v_pts, v_scale, a_pts, a_scale) as f64
            / (f64::from(v_scale) * f64::from(a_scale));
        offsets.insert(epoch, audio_elapsed - video_elapsed + delta);
    }
    Ok(offsets)
}

fn build_fmp4_jobs(
    selected: &SelectedHlsInput,
    next_sequence: u64,
    ended_offsets: &BTreeMap<u64, f64>,
) -> (Vec<Fmp4Job>, Option<u64>) {
    let audio_timing = selected
        .audio
        .as_ref()
        .map(|audio| RenditionTimeline::new(audio, selected.video.end_list));
    let subtitle_timing = selected
        .subtitle
        .as_ref()
        .map(|subtitle| RenditionTimeline::new(subtitle, selected.video.end_list));
    let mut video_start = 0.0;
    let mut jobs = Vec::new();
    for (idx, video) in selected.video.segments.iter().enumerate() {
        let start = video_start;
        video_start += video.duration;
        if video.sequence < next_sequence {
            continue;
        }
        let (audio, verify_sample_time) = match audio_timing.as_ref() {
            Some(timeline) => match timeline.pair_fmp4_audio(video, idx, start, ended_offsets) {
                Some((seg, verify)) => (Some(seg.clone()), verify),
                None => return (jobs, Some(video.sequence)),
            },
            None => (None, false),
        };
        let subtitle = subtitle_timing
            .as_ref()
            .and_then(|timeline| timeline.pair(video, idx, start))
            .map(|(seg, _)| seg.clone());
        jobs.push(Fmp4Job {
            sequence: video.sequence,
            audio,
            verify_sample_time,
            subtitle,
            video: video.clone(),
        });
    }
    (jobs, None)
}

fn hls_segment_by_sequence(playlist: &HlsMediaPlaylist, number: u64) -> Option<&HlsSegment> {
    playlist
        .segments
        .binary_search_by_key(&number, |seg| seg.sequence)
        .ok()
        .map(|idx| &playlist.segments[idx])
}

#[derive(Clone)]
struct Fmp4Job {
    sequence: u64,
    verify_sample_time: bool,
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
    .buffered(fetch_concurrency);
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

            // A mode mismatch is permanent; the live-gap recovery below must not swallow it.
            if ctx.key_mode.is_clear() || ctx.in_band_audio {
                downloaded.ensure_clear()?;
            }
            if let Err(error) = process_fmp4_segment(task, downloaded, ctx) {
                if allow_live_skip_failures && !is_permanent_segment_error(&error) {
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

impl DownloadedFmp4Segment {
    fn ensure_clear(&self) -> crate::Result<()> {
        ensure_clear_fragment(&self.venc, "HLS video")?;
        if let Some(audio) = &self.aenc {
            ensure_clear_fragment(audio, "HLS audio")?;
        }
        if let Some(subtitle) = &self.senc {
            ensure_clear_fragment(subtitle, "HLS subtitle")?;
        }
        Ok(())
    }
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
        flush_fmp4_aggregate(task, ctx)?;
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
        mark_fmp4_epoch_boundary(task, ctx, "upstream HLS EXT-X-DISCONTINUITY")?;
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

    let downloaded_bytes = downloaded
        .venc
        .len()
        .saturating_add(downloaded.aenc.as_ref().map_or(0, |d| d.len()))
        .saturating_add(downloaded.senc.as_ref().map_or(0, |d| d.len()));
    if aggregate_needs_flush(ctx.agg_input_bytes, downloaded_bytes)? {
        flush_fmp4_aggregate(task, ctx)?;
    }
    task.bytes_done
        .fetch_add(downloaded_bytes as u64, Ordering::Relaxed);

    let mut vparsed = ctx
        .video_track
        .parse(&downloaded.venc, ctx.v_default_iv_size)
        .ok_or_else(|| anyhow::anyhow!("parse HLS video seq {} failed", downloaded.job.sequence))?;
    if let Some(vdec) = &vdec {
        vdec.decrypt_segment(&mut vparsed.mdat, &vparsed.samples);
    }
    let audio_data = downloaded
        .aenc
        .as_deref()
        .or_else(|| ctx.in_band_audio.then_some(downloaded.venc.as_ref()));
    let mut aparsed = match (ctx.audio_track, audio_data) {
        (Some(track), Some(aenc)) => {
            let mut parsed = track.parse(aenc, ctx.a_default_iv_size).ok_or_else(|| {
                anyhow::anyhow!("parse HLS audio seq {} failed", downloaded.job.sequence)
            })?;
            if let Some(adec) = &adec {
                adec.decrypt_segment(&mut parsed.mdat, &parsed.samples);
            }
            Some(parsed)
        }
        (None, None) => None,
        _ => anyhow::bail!("HLS audio data does not match the selected init track"),
    };
    if downloaded.job.verify_sample_time {
        let aligned = aparsed
            .as_ref()
            .zip(ctx.audio_track)
            .is_some_and(|(audio, track)| {
                fmp4_rendition_sample_times_align(
                    &vparsed,
                    ctx.video_track.timescale,
                    audio,
                    track.timescale,
                )
            });
        if !aligned {
            return Err(RenditionTimingMismatch(downloaded.job.sequence).into());
        }
    }
    if let (Some(audio), Some(track)) = (aparsed.as_ref(), ctx.audio_track) {
        let seg = downloaded
            .job
            .audio
            .as_ref()
            .unwrap_or(&downloaded.job.video);
        let coverage = audio
            .samples
            .iter()
            .map(|sample| f64::from(sample.duration))
            .sum::<f64>()
            / f64::from(track.timescale);
        anyhow::ensure!(
            coverage >= seg.duration * 0.9,
            "HLS audio for media sequence {} contains less than 90% of its advertised duration",
            seg.sequence
        );
    }
    let seg_v_dur_ts = fmp4_samples_span(&vparsed)
        .ok_or_else(|| anyhow::anyhow!("HLS video fragment has no valid samples"))?;
    anyhow::ensure!(
        seg_v_dur_ts as f64 / f64::from(ctx.video_track.timescale)
            >= downloaded.job.video.duration * 0.9,
        "HLS video seq {} contains less media than its EXTINF",
        downloaded.job.sequence
    );
    if timeline_rewound(ctx.last_v_tfdt, vparsed.base_media_decode_time)
        || timeline_rewound(
            ctx.last_a_tfdt,
            aparsed.as_ref().and_then(|p| p.base_media_decode_time),
        )
    {
        mark_fmp4_epoch_boundary(task, ctx, "HLS fMP4 tfdt rewound")?;
    }
    ctx.last_v_tfdt = vparsed.base_media_decode_time;
    ctx.last_a_tfdt = aparsed.as_ref().and_then(|p| p.base_media_decode_time);

    if ctx.in_band_audio
        && task.origin_v_tfdt.load(Ordering::Relaxed) == u64::MAX
        && task.origin_a_tfdt.load(Ordering::Relaxed) == u64::MAX
    {
        if let Some((video, audio)) = ctx.last_v_tfdt.zip(ctx.last_a_tfdt) {
            let (video_origin, audio_origin) = shared_fmp4_decode_origins(
                video,
                ctx.video_track.timescale,
                audio,
                ctx.muxer.audio_timescale,
            );
            task.origin_v_tfdt.store(video_origin, Ordering::Relaxed);
            task.origin_a_tfdt.store(audio_origin, Ordering::Relaxed);
        }
    }

    let seg_v_dts = match vparsed.base_media_decode_time {
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
    for (s, &decode_offset) in vparsed.samples.iter().zip(&vparsed.decode_time_offsets) {
        let (a, b) = s.data_range;
        let sample = vparsed
            .mdat
            .get(a..b)
            .ok_or_else(|| anyhow::anyhow!("HLS video sample is out of bounds"))?;
        let dts = seg_v_dts
            .checked_add(decode_offset)
            .ok_or_else(|| anyhow::anyhow!("HLS video timestamp overflow"))?;
        vaus.push(AccessUnit::from_sample(
            sample,
            dts,
            s.cts_offset as i64,
            ctx.video_codec,
        ));
    }
    ctx.acc_v_dts = seg_v_dts
        .checked_add(seg_v_dur_ts)
        .ok_or_else(|| anyhow::anyhow!("HLS video timestamp overflow"))?;

    let mut aaus = Vec::new();
    let mut seg_a_dur_ts = 0u64;
    if let Some(aparsed) = aparsed.take() {
        seg_a_dur_ts = fmp4_samples_span(&aparsed)
            .ok_or_else(|| anyhow::anyhow!("HLS selected audio fragment has no valid samples"))?;
        let seg_a_pts = match aparsed.base_media_decode_time {
            Some(tfdt) => {
                let origin = task.origin_a_tfdt.load(Ordering::Relaxed);
                let origin = if origin == u64::MAX {
                    let video_origin = task.origin_v_tfdt.load(Ordering::Relaxed);
                    let origin = if ctx.in_band_audio && video_origin != u64::MAX {
                        u64::try_from(
                            (u128::from(video_origin) * u128::from(ctx.muxer.audio_timescale))
                                / u128::from(ctx.video_track.timescale),
                        )
                        .map_err(|_| anyhow::anyhow!("HLS shared audio origin overflow"))?
                    } else {
                        tfdt
                    };
                    task.origin_a_tfdt.store(origin, Ordering::Relaxed);
                    origin
                } else {
                    origin
                };
                tfdt.saturating_sub(origin)
            }
            None => ctx.acc_a_dts,
        };
        for (s, &decode_offset) in aparsed.samples.iter().zip(&aparsed.decode_time_offsets) {
            let (x, y) = s.data_range;
            let frame = aparsed
                .mdat
                .get(x..y)
                .ok_or_else(|| anyhow::anyhow!("HLS audio sample is out of bounds"))?;
            let pts = seg_a_pts
                .checked_add(decode_offset)
                .ok_or_else(|| anyhow::anyhow!("HLS audio timestamp overflow"))?;
            let data = ctx
                .audio_config
                .ok_or_else(|| anyhow::anyhow!("HLS fMP4 audio has no validated configuration"))?
                .pack_sample(frame, s.cts_offset)
                .map_err(|e| {
                    e.context(format!(
                        "HLS audio sequence {}",
                        downloaded.job.video.sequence
                    ))
                })?;
            aaus.push(AudioUnit { data, pts });
        }
        ctx.acc_a_dts = seg_a_pts
            .checked_add(seg_a_dur_ts)
            .ok_or_else(|| anyhow::anyhow!("HLS audio timestamp overflow"))?;
    }

    // A multiplexed fragment supplies both tracks on the same time axis.
    // Do not independently smooth those clocks inside an output aggregate.
    if (!ctx.in_band_audio || ctx.last_v_tfdt.is_none())
        && !ctx.agg_vaus.is_empty()
        && !vaus.is_empty()
    {
        let first_v = ctx.agg_vaus.first().map(|a| a.dts).unwrap_or(0);
        let expected_v = first_v + ctx.agg_v_dur_ts;
        let v_off = expected_v as i64 - vaus[0].dts as i64;
        if v_off != 0 {
            for au in &mut vaus {
                au.dts = (au.dts as i64 + v_off).max(0) as u64;
            }
        }
    }
    if (!ctx.in_band_audio || ctx.last_a_tfdt.is_none())
        && !ctx.agg_aaus.is_empty()
        && !aaus.is_empty()
    {
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
            ctx.subtitle_timescale,
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
    ctx.agg_input_bytes += downloaded_bytes;

    if ctx.agg_duration >= AGGREGATE_TARGET_S || single_duration >= SHORT_SEG_THRESHOLD_S {
        flush_fmp4_aggregate(task, ctx)?;
    }
    tracing::debug!(task=%task.id, seq=downloaded.job.sequence, "hls fmp4 segment muxed");
    Ok(())
}

/// One epoch origin mapped into both clocks, at or before both tracks so a
/// legitimate lead never gets clipped by the unsigned normalized timeline.
fn shared_fmp4_decode_origins(
    video: u64,
    video_scale: u32,
    audio: u64,
    audio_scale: u32,
) -> (u64, u64) {
    let video_scaled = u128::from(video) * u128::from(audio_scale);
    let audio_scaled = u128::from(audio) * u128::from(video_scale);
    if video_scaled <= audio_scaled {
        // Quotient <= audio by the comparison above.
        (video, (video_scaled / u128::from(video_scale)) as u64)
    } else {
        ((audio_scaled / u128::from(audio_scale)) as u64, audio)
    }
}

fn earliest_fmp4_pts(media: &ParsedSegment) -> Option<i128> {
    let base = i128::from(media.base_media_decode_time?);
    media
        .samples
        .iter()
        .zip(&media.decode_time_offsets)
        .map(|(sample, offset)| base + i128::from(*offset) + i128::from(sample.cts_offset))
        .min()
}

fn cross_scaled_pts_delta(video: i128, v_scale: u32, audio: i128, a_scale: u32) -> i128 {
    video * i128::from(a_scale) - audio * i128::from(v_scale)
}

fn fmp4_rendition_sample_times_align(
    video: &ParsedSegment,
    video_scale: u32,
    audio: &ParsedSegment,
    audio_scale: u32,
) -> bool {
    let (Some(video_pts), Some(audio_pts)) = (earliest_fmp4_pts(video), earliest_fmp4_pts(audio))
    else {
        return false;
    };
    let cross_scale = f64::from(audio_scale) * f64::from(video_scale);
    let delta = cross_scaled_pts_delta(video_pts, video_scale, audio_pts, audio_scale);
    (delta.unsigned_abs() as f64) <= (HLS_RENDITION_BOUNDARY_SLACK_SECS * cross_scale)
}

fn fmp4_samples_span(parsed: &ParsedSegment) -> Option<u64> {
    let last = parsed.samples.last()?;
    parsed
        .decode_time_offsets
        .last()?
        .checked_add(u64::from(last.duration))
        .filter(|&ticks| ticks > 0)
}

fn flush_fmp4_aggregate(task: &Arc<Task>, ctx: &mut Fmp4Context) -> crate::Result<()> {
    if ctx.agg_vaus.is_empty() {
        return Ok(());
    }
    let pts = ctx.muxer.segment_pts_90k(&ctx.agg_vaus);
    anyhow::ensure!(
        ctx.audio_track.is_none() || !ctx.agg_aaus.is_empty(),
        "HLS output aggregation lost the selected audio track"
    );
    let ts_bytes = ctx.muxer.mux_segment(&ctx.agg_vaus, &ctx.agg_aaus);
    publish_hls_outputs(
        task,
        ctx.agg_duration,
        ts_bytes,
        ctx.subtitle_acc.take_body(),
        pts,
        ctx.pending_discontinuity,
    )?;
    ctx.pending_discontinuity = false;
    ctx.agg_vaus.clear();
    ctx.agg_aaus.clear();
    ctx.agg_v_dur_ts = 0;
    ctx.agg_a_dur_ts = 0;
    ctx.agg_duration = 0.0;
    ctx.agg_input_bytes = 0;
    Ok(())
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
    track: Fmp4Track,
    default_iv_size: Option<u8>,
) -> Option<i64> {
    let first = playlist.segments.first()?;
    let data = dl.get(&first.uri).await.ok()?;
    let parsed = track.parse(&data, default_iv_size)?;
    parsed.samples.iter().map(|s| s.cts_offset as i64).min()
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
    let clear_mode = ctx.clear_mode;

    let stream = futures::stream::iter(jobs.into_iter().map(move |job| {
        let dl = dl2.clone();
        let task = task2.clone();
        async move {
            let video = job.video.clone();
            let subtitle = job.subtitle.clone();
            let video_data = match fetch_ts_segment(&task, &dl, &video, clear_mode).await {
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
    .buffered(fetch_concurrency);
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
                if failure.error.is::<ProtectedInput>() {
                    return Err(failure.error);
                }
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

            if ctx.clear_mode {
                if let Some(subtitle) = &downloaded.subtitle_data {
                    ensure_clear_fragment(subtitle, "HLS subtitle")?;
                }
            }
            if let Err(error) = process_downloaded_segment(task, dl, downloaded, ctx).await {
                if allow_live_skip_failures && !is_permanent_segment_error(&error) {
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

async fn fetch_ts_segment(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    segment: &HlsSegment,
    clear_mode: bool,
) -> crate::Result<bytes::Bytes> {
    if !clear_mode {
        return task.fetch_media(dl, &segment.uri).await;
    }
    if segment.key != HlsSegmentKey::None {
        return Err(ProtectedInput::new("HLS video segment", "a segment key").into());
    }
    for attempt in 1..CLEAR_TS_FRAME_ATTEMPTS {
        let data = task.fetch_media(dl, &segment.uri).await?;
        match ensure_clear_ts_payload(&data) {
            Ok(()) => return Ok(data),
            Err(e) if e.is::<ProtectedInput>() => return Err(e),
            Err(e) => {
                tracing::debug!(task=%task.id, seq=segment.sequence, attempt, "retrying incomplete Clear HLS TS segment: {e:#}");
                let cancel = task.cancel_token();
                tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("cancelled while waiting for HLS TS segment"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {}
                }
            }
        }
    }
    let data = task.fetch_media(dl, &segment.uri).await?;
    ensure_clear_ts_payload(&data)?;
    Ok(data)
}

fn ensure_clear_ts_payload(data: &[u8]) -> crate::Result<()> {
    // Don't publish opaque bytes (e.g. non-TS input or a missing encryption tag)
    // as unencrypted media.
    let packets = data.chunks_exact(TS_PACKET_SIZE);
    if data.is_empty() || !packets.remainder().is_empty() {
        anyhow::bail!("Clear HLS video segment does not contain complete MPEG-TS packets");
    }
    let mut has_scrambled_packet = false;
    for packet in packets {
        if packet[0] != SYNC_BYTE {
            anyhow::bail!("Clear HLS video segment has invalid MPEG-TS framing");
        }
        has_scrambled_packet |= packet[3] & 0xc0 != 0;
    }
    if has_scrambled_packet {
        return Err(ProtectedInput::new("HLS video segment", "scrambled TS packets").into());
    }
    Ok(())
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
        flush_ts_aggregate(task, ctx)?;
    }
    if segment.discontinuity {
        flush_ts_aggregate(task, ctx)?;
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

    let input_bytes = clear
        .len()
        .saturating_add(subtitle_data.as_ref().map_or(0, |d| d.len()));
    if aggregate_needs_flush(ctx.agg_input_bytes, input_bytes)? {
        flush_ts_aggregate(task, ctx)?;
    }
    ctx.agg_data.try_reserve_exact(clear.len())?;
    ctx.agg_input_bytes += input_bytes;
    let single_duration = segment.duration;
    let video_pts = task
        .subtitles_enabled
        .then(|| first_video_pts_90k(&clear))
        .flatten();
    if ctx.agg_data.is_empty() {
        ctx.agg_video_pts_90k = video_pts;
    }
    ctx.agg_data.extend_from_slice(&clear);
    ctx.agg_duration += single_duration;
    if let Some(data) = subtitle_data {
        if let Err(e) = ctx.subtitle_acc.append_fragment_at(
            &data,
            single_duration,
            1_000,
            None,
            None,
            video_pts,
        ) {
            tracing::warn!(task=%task.id, seq=segment.sequence, "HLS subtitle conversion failed: {e:#}");
            ctx.subtitle_acc.append_empty(single_duration);
        }
    } else {
        let _ = subtitle;
        ctx.subtitle_acc.append_empty(single_duration);
    }
    if ctx.agg_duration >= AGGREGATE_TARGET_S || single_duration >= SHORT_SEG_THRESHOLD_S {
        flush_ts_aggregate(task, ctx)?;
    }
    tracing::debug!(task=%task.id, seq=segment.sequence, "hls segment published");
    Ok(())
}

fn flush_ts_aggregate(task: &Arc<Task>, ctx: &mut TsContext) -> crate::Result<()> {
    if ctx.agg_data.is_empty() {
        return Ok(());
    }
    publish_hls_outputs(
        task,
        ctx.agg_duration,
        std::mem::take(&mut ctx.agg_data),
        ctx.subtitle_acc.take_body(),
        ctx.agg_video_pts_90k.take(),
        ctx.pending_discontinuity,
    )?;
    ctx.agg_duration = 0.0;
    ctx.agg_input_bytes = 0;
    ctx.pending_discontinuity = false;
    Ok(())
}

fn publish_hls_outputs(
    task: &Arc<Task>,
    duration: f64,
    ts_bytes: Vec<u8>,
    subtitle_body: String,
    pts: Option<u64>,
    discontinuity: bool,
) -> crate::Result<()> {
    let subtitle = if task.subtitles_enabled {
        let timestamp = match pts {
            Some(timestamp) => timestamp,
            None if subtitle_body.is_empty() => 0, // No cues need a time reference.
            None => return Err(SubtitleTimingUnavailable.into()),
        };
        Some((task.subtitles.as_ref(), subtitle_body, timestamp))
    } else {
        None
    };
    task.hls.push_segment_pair(
        duration,
        bytes::Bytes::from(ts_bytes),
        discontinuity,
        subtitle,
    )?;
    task.segments_done.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn mark_ts_live_gap_boundary(
    task: &Arc<Task>,
    ctx: &mut TsContext,
    reason: &str,
) -> crate::Result<()> {
    flush_ts_aggregate(task, ctx)?;
    ctx.pending_discontinuity = true;
    ctx.subtitle_acc.clear();
    ctx.last_key_marker = None;
    tracing::warn!(task = %task.id, reason, "starting a new HLS TS output discontinuity epoch");
    Ok(())
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

    const HLS_FIXTURE_URL: &str = "https://example.invalid/index.m3u8";

    fn parsed_media(text: &str) -> HlsMediaPlaylist {
        match parse_playlist(text, HLS_FIXTURE_URL).unwrap() {
            HlsPlaylist::Media(media) => media,
            HlsPlaylist::Master(_) => panic!("expected media playlist"),
        }
    }

    #[test]
    fn clear_hls_accepts_unencrypted_ts_and_fmp4_playlists() {
        let ts = parsed_media("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nclear.ts\n");
        let fmp4 = parsed_media("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nclear.m4s\n");
        assert!(!ts.has_map);
        assert!(fmp4.has_map);
        assert!(ensure_clear_hls_media(&ts, "video").is_ok());
        assert!(ensure_clear_hls_media(&fmp4, "video").is_ok());
    }

    #[test]
    fn clear_hls_rejects_encryption_signaled_by_any_selected_playlist() {
        let clear = parsed_media("#EXTM3U\n#EXTINF:4,\nclear.ts\n");
        let encrypted = parsed_media(concat!(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x01\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\npart.m4s\n",
        ));
        assert_eq!(encrypted.segments[0].key, HlsSegmentKey::None);

        let mut selected = SelectedHlsInput {
            video_url: HLS_FIXTURE_URL.into(),
            video: clear.clone(),
            audio_url: None,
            audio: None,
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        assert!(ensure_clear_hls_input(&selected).is_ok());
        // A refresh can change any of the selected renditions.
        selected.subtitle = Some(encrypted.clone());
        assert!(ensure_clear_hls_input(&selected)
            .unwrap_err()
            .to_string()
            .contains("subtitle"));
        selected.subtitle = None;
        selected.audio = Some(encrypted.clone());
        assert!(ensure_clear_hls_input(&selected)
            .unwrap_err()
            .to_string()
            .contains("audio"));
        selected.audio = None;
        selected.video = encrypted;
        assert!(ensure_clear_hls_input(&selected)
            .unwrap_err()
            .to_string()
            .contains("video"));
    }

    #[test]
    fn clear_hls_rejects_master_session_key_and_distinct_maps() {
        let playlist = parse_playlist(
            concat!(
                "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"key.bin\"\n",
                "#EXT-X-STREAM-INF:BANDWIDTH=1\nvideo.m3u8\n",
            ),
            HLS_FIXTURE_URL,
        )
        .unwrap();
        let HlsPlaylist::Master(master) = playlist else {
            panic!("expected master");
        };
        assert!(ensure_clear_hls_master(&master).is_err());

        let multiple_maps = parsed_media(concat!(
            "#EXTM3U\n#EXT-X-MAP:URI=\"a.mp4\"\n#EXTINF:4,\na.m4s\n",
            "#EXT-X-MAP:URI=\"b.mp4\"\n#EXTINF:4,\nb.m4s\n",
        ));
        assert!(ensure_clear_hls_media(&multiple_maps, "video").is_err());
    }

    #[test]
    fn clear_hls_rejects_scrambled_ts_packets() {
        let mut data = vec![0; 2 * TS_PACKET_SIZE];
        data[0] = SYNC_BYTE;
        data[TS_PACKET_SIZE] = SYNC_BYTE;
        data[3] = 0x10;
        data[TS_PACKET_SIZE + 3] = 0x10;
        assert!(ensure_clear_ts_payload(&data).is_ok());
        data[TS_PACKET_SIZE + 3] |= 0x80;
        assert!(ensure_clear_ts_payload(&data).is_err());
    }

    #[test]
    fn clear_hls_rejects_non_ts_or_truncated_data() {
        assert!(!is_permanent_segment_error(
            &ensure_clear_ts_payload(b"").unwrap_err()
        ));
        assert!(ensure_clear_ts_payload(&[0; TS_PACKET_SIZE]).is_err());
        let mut truncated = vec![0; TS_PACKET_SIZE + 1];
        truncated[0] = SYNC_BYTE;
        assert!(ensure_clear_ts_payload(&truncated).is_err());
    }

    #[test]
    fn in_band_audio_rendition_is_not_replaced_by_external_alternative() {
        let HlsPlaylist::Master(master) = parse_playlist(
            concat!(
                "#EXTM3U\n",
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Main\",DEFAULT=YES\n",
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Alt\",URI=\"alt.m3u8\"\n",
                "#EXT-X-STREAM-INF:BANDWIDTH=50000,AUDIO=\"aud\"\nvideo.m3u8\n",
            ),
            HLS_FIXTURE_URL,
        )
        .unwrap() else {
            panic!("expected a Master Playlist");
        };
        let default = select_audio_rendition(&master.audio, Some("aud"), None)
            .unwrap()
            .unwrap();
        assert_eq!(default.name, "Main");
        assert!(default.uri.is_empty());
        let external = select_audio_rendition(&master.audio, Some("aud"), Some("hls-a-1"))
            .unwrap()
            .unwrap();
        assert!(external.uri.ends_with("alt.m3u8"));
        assert!(select_audio_rendition(&master.audio, None, Some("hls-a-1")).is_err());
    }

    #[test]
    fn aggregate_budget_flushes_before_growth_and_rejects_oversized_fragment() {
        assert!(!aggregate_needs_flush(MAX_HLS_AGGREGATE_BYTES - 3, 3).unwrap());
        assert!(aggregate_needs_flush(MAX_HLS_AGGREGATE_BYTES - 2, 3).unwrap());
        let oversized = aggregate_needs_flush(0, MAX_HLS_AGGREGATE_BYTES + 1).unwrap_err();
        assert!(is_permanent_segment_error(&oversized));
    }

    #[test]
    fn live_does_not_skip_unsupported_ac4_composition_offsets() {
        let error = anyhow::Error::from(UnsupportedAc4CompositionOffset(-1920))
            .context("HLS audio sequence 91");
        assert!(is_permanent_segment_error(&error));
    }

    #[test]
    fn clear_hls_rejects_protected_fmp4_fragment_without_playlist_key() {
        fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut data = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
            data.extend_from_slice(kind);
            data.extend_from_slice(payload);
            data
        }

        let media = playlist(vec![1]);
        let mut downloaded = DownloadedFmp4Segment {
            job: Fmp4Job {
                sequence: 1,
                verify_sample_time: false,
                video: media.segments[0].clone(),
                audio: None,
                subtitle: None,
            },
            venc: mp4_box(b"moof", &mp4_box(b"traf", &[])).into(),
            aenc: None,
            senc: None,
        };
        assert!(downloaded.ensure_clear().is_ok());
        let senc = mp4_box(b"senc", &[0; 8]);
        downloaded.venc = mp4_box(b"moof", &mp4_box(b"traf", &senc)).into();
        assert!(downloaded.ensure_clear().is_err());
    }

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
    fn fmp4_jobs_wait_for_matching_audio_sequence() {
        let selected = SelectedHlsInput {
            video_url: "https://cdn/v.m3u8".to_string(),
            video: playlist(vec![10, 11, 12]),
            audio_url: Some("https://cdn/a.m3u8".to_string()),
            audio: Some(playlist(vec![10])),
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        let (jobs, blocked) = build_fmp4_jobs(&selected, 10, &BTreeMap::new());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].sequence, 10);
        assert!(jobs[0].audio.is_some());
        assert_eq!(blocked, Some(11));
    }

    #[test]
    fn fmp4_jobs_allow_video_only_when_no_audio_track_selected() {
        let selected = SelectedHlsInput {
            video_url: "https://cdn/v.m3u8".to_string(),
            video: playlist(vec![10, 11]),
            audio_url: None,
            audio: None,
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        let (jobs, blocked) = build_fmp4_jobs(&selected, 10, &BTreeMap::new());
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.audio.is_none()));
        assert!(blocked.is_none());
    }

    #[test]
    fn fmp4_vod_checks_independent_sequence_numbers_against_measured_origins() {
        let mut video = playlist(vec![10, 11, 12]);
        video.end_list = true;
        let mut audio = playlist(vec![900, 901, 902]);
        audio.end_list = true;
        let mut selected = SelectedHlsInput {
            video_url: video.uri.clone(),
            video,
            audio_url: Some(audio.uri.clone()),
            audio: Some(audio),
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        // Both reference fragments start at the same media time.
        let offsets = BTreeMap::from([(0, 0.0)]);
        let (jobs, blocked) = build_fmp4_jobs(&selected, 11, &offsets);
        assert_eq!(
            jobs.iter()
                .map(|job| job.audio.as_ref().unwrap().sequence)
                .collect::<Vec<_>>(),
            [901, 902]
        );
        assert!(blocked.is_none());
        assert!(jobs.iter().all(|job| job.verify_sample_time));
        selected.audio.as_mut().unwrap().segments[1].duration = 3.0;
        let (jobs, blocked) = build_fmp4_jobs(&selected, 10, &offsets);
        assert_eq!(jobs.len(), 1);
        assert_eq!(blocked, Some(11));
    }

    #[test]
    fn fmp4_final_sliding_window_uses_media_time_not_index_or_sequence() {
        let mut video = playlist(vec![102, 103]);
        video.end_list = true;
        let mut audio = playlist(vec![500, 501, 502]);
        audio.end_list = true;
        let selected = SelectedHlsInput {
            video_url: video.uri.clone(),
            video,
            audio_url: Some(audio.uri.clone()),
            audio: Some(audio),
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        // Audio retained one earlier six-second segment.
        let (jobs, blocked) = build_fmp4_jobs(&selected, 102, &BTreeMap::from([(0, 6.0)]));
        assert!(blocked.is_none());
        assert!(jobs.iter().all(|job| job.verify_sample_time));
        assert_eq!(
            jobs.iter()
                .map(|job| job.audio.as_ref().unwrap().sequence)
                .collect::<Vec<_>>(),
            [501, 502]
        );
        assert_eq!(
            build_fmp4_jobs(&selected, 102, &BTreeMap::new()).1,
            Some(102)
        );
    }

    #[test]
    fn fmp4_shared_origins_preserve_either_tracks_lead() {
        assert_eq!(
            shared_fmp4_decode_origins(2_100, 1_000, 96_000, 48_000),
            (2_000, 96_000)
        );
        assert_eq!(
            shared_fmp4_decode_origins(2_000, 1_000, 100_800, 48_000),
            (2_000, 96_000)
        );
        assert_eq!(
            shared_fmp4_decode_origins(2_000, 1_000, 96_000, 48_000),
            (2_000, 96_000)
        );
    }

    #[test]
    fn live_renditions_match_dates_and_epochs_not_media_sequence() {
        let selected = SelectedHlsInput {
            video_url: HLS_FIXTURE_URL.into(),
            video: parsed_media(concat!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n",
                "#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:00:10Z\n",
                "#EXTINF:6,\nv1.m4s\n#EXTINF:6,\nv2.m4s\n"
            )),
            audio_url: Some(HLS_FIXTURE_URL.into()),
            audio: Some(parsed_media(concat!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n",
                "#EXT-X-PROGRAM-DATE-TIME:1970-01-01T01:00:04+01:00\n",
                "#EXTINF:6,\na0.m4s\n#EXTINF:6,\na1.m4s\n#EXTINF:6,\na2.m4s\n"
            ))),
            expected_in_band_audio: false,
            subtitle_url: Some(HLS_FIXTURE_URL.into()),
            subtitle: Some(parsed_media(concat!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n",
                "#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:00:10Z\n",
                "#EXTINF:6,\ns1.vtt\n#EXTINF:6,\ns2.vtt\n"
            ))),
        };
        let (jobs, blocked) = build_fmp4_jobs(&selected, 10, &BTreeMap::new());
        assert!(blocked.is_none());
        assert_eq!(
            jobs.iter()
                .map(|j| j.audio.as_ref().unwrap().sequence)
                .collect::<Vec<_>>(),
            [901, 902]
        );
        assert!(jobs.iter().all(|job| !job.verify_sample_time));
        assert_eq!(
            build_ts_jobs(&selected, 10)
                .iter()
                .map(|j| j.subtitle.as_ref().unwrap().sequence)
                .collect::<Vec<_>>(),
            [500, 501]
        );
    }

    #[test]
    fn live_equal_sequences_do_not_override_conflicting_time_anchors() {
        let mut video = playlist(vec![10, 11]);
        let mut audio = playlist(vec![10, 11]);
        video.segments[0].program_date_time = Some(100.0);
        audio.segments[0].program_date_time = Some(106.0);
        let mut selected = SelectedHlsInput {
            video_url: HLS_FIXTURE_URL.into(),
            video,
            audio_url: Some(HLS_FIXTURE_URL.into()),
            audio: Some(audio),
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        let (jobs, blocked) = build_fmp4_jobs(&selected, 10, &BTreeMap::new());
        assert!(jobs.is_empty());
        assert_eq!(blocked, Some(10));
        selected.video.segments[0].program_date_time = None;
        let (jobs, blocked) = build_fmp4_jobs(&selected, 10, &BTreeMap::new());
        assert!(blocked.is_none());
        assert!(jobs.iter().all(|j| j.verify_sample_time));
        selected.audio.as_mut().unwrap().segments[0].discontinuity_sequence = 1;
        assert_eq!(build_fmp4_jobs(&selected, 10, &BTreeMap::new()).1, Some(10));
    }

    #[test]
    fn refreshed_signed_urls_keep_the_sequence_cursor_without_url_history() {
        let mut selected = SelectedHlsInput {
            video_url: "https://cdn/v.m3u8".into(),
            video: playlist(vec![10, 11, 12]),
            audio_url: None,
            audio: None,
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        for s in &mut selected.video.segments {
            s.uri.push_str("?fresh-signature");
        }
        assert_eq!(
            build_ts_jobs(&selected, 12)
                .into_iter()
                .map(|job| job.video.sequence)
                .collect::<Vec<_>>(),
            [12]
        );
        assert_eq!(
            build_fmp4_jobs(&selected, 12, &BTreeMap::new())
                .0
                .into_iter()
                .map(|job| job.sequence)
                .collect::<Vec<_>>(),
            [12]
        );
        assert_eq!(hls_expired_cursor(&selected.video, 8), Some(10));
        assert_eq!(hls_expired_cursor(&selected.video, 12), None);
    }

    #[test]
    fn ts_separate_audio_and_unimplemented_ranges_are_explicit_errors() {
        let mut selected = SelectedHlsInput {
            video_url: "https://cdn/v.m3u8".into(),
            video: playlist(vec![10, 11, 12]),
            audio_url: Some("https://cdn/a.m3u8".into()),
            audio: Some(playlist(vec![900])),
            expected_in_band_audio: false,
            subtitle_url: None,
            subtitle: None,
        };
        assert!(ensure_ts_audio_layout(&selected).is_err());
        selected.audio = None;
        assert!(ensure_ts_audio_layout(&selected).is_ok());
        selected.video = parsed_media(concat!(
            "#EXTM3U\n#EXT-X-MAP:URI=\"shared.mp4\",BYTERANGE=\"118@0\"\n",
            "#EXTINF:6,\n#EXT-X-BYTERANGE:204@118\nshared.mp4\n",
        ));
        assert!(ensure_hls_input_layout(&selected).is_err());
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
            has_encryption: false,
            has_multiple_maps: false,
            has_byte_ranges: false,
            segments: sequences
                .into_iter()
                .map(|sequence| HlsSegment {
                    sequence,
                    uri: format!("https://cdn/{sequence}.m4s"),
                    duration: 6.0,
                    key: HlsSegmentKey::None,
                    discontinuity: false,
                    discontinuity_sequence: 0,
                    program_date_time: None,
                })
                .collect(),
        }
    }
}
