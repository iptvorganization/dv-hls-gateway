//! 每任务流水线：MPD轮询 → 并发下载 → CENC解密 → TS封装 → 切片 → 滚动m3u8 → GC。
//!
//! 当前实现：分片乱序并发下载、按段序重排后解密封装。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use crate::clock::ClockState;
use crate::crypto::cenc::{Decryptor, KeyStore};
use crate::hevc::annexb::AccessUnit;
use crate::mp4::protection::{ensure_clear_fragment, ensure_clear_init, ProtectedInput};
use crate::mp4::sample::{parse_media_segment_for_track, ParsedSegment, SampleDefaults};
use crate::mp4::tracks::{tracks_from_init, InitTrack};
use crate::mp4::{DoviConfig, ParamSets, TrackEncryption};
use crate::mpd::parser::SegmentRef;
use crate::mpd::{parse_mpd, Representation, TrackKind};
use crate::subtitle::SubtitleAccumulator;
use crate::ts::muxer::{AudioUnit, TsMuxer};
use crate::ts::{AudioCodec, VideoCodec, VideoRange};

use super::{audio::Fmp4AudioConfig, key_resolver, live_tuning, manager::Task};

const SHORT_SEG_THRESHOLD_S: f64 = live_tuning::SHORT_SEG_THRESHOLD_S;
const AGGREGATE_TARGET_S: f64 = live_tuning::AGGREGATE_TARGET_S;
const MAX_AUS_PER_AGGREGATE: usize = 262_144;
const MAX_AGGREGATE_PAYLOAD: usize = 128 * 1024 * 1024;
const MAX_AGGREGATE_WHILE_WAITING_FOR_AUDIO_S: f64 = 3.0 * AGGREGATE_TARGET_S;
const MEDIA_CONTENT_ATTEMPTS: usize = 6;
const MEDIA_CONTENT_RETRY_BASE_MS: u64 = 250;
const MIN_MEDIA_COVERAGE_NUM: u64 = 9;
const MIN_MEDIA_COVERAGE_DEN: u64 = 10;
const AUDIO_MATCH_MAX_WAIT_REFRESHES: u32 = 4;
const SHORT_SOURCE_MIN_SEGMENTS: usize = 2;
const SHORT_SOURCE_MIN_SHORT_RATIO_NUM: usize = 3;
const SHORT_SOURCE_MIN_SHORT_RATIO_DEN: usize = 4;
const SHORT_GROUP_AUDIO_EDGE_SLACK_S: f64 = 0.120;

/// 流水线入口：解析 MPD、选轨、循环下载解密封装切片，直到取消或 VOD 结束。
pub(super) async fn run_pipeline(
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
    // 1. 拉取并解析 MPD
    task.set_stage("解析 MPD");
    let manifest = dl.get_manifest(&sel.mpd_url).await?;
    let mpd = parse_mpd(&manifest.text, &manifest.url)?;

    let video = mpd
        .select_representation(&sel.video_rep_id, TrackKind::Video)?
        .clone();
    if !mpd.is_dynamic && video.segments.is_empty() {
        anyhow::bail!("DASH VOD has no video segments in its Period");
    }

    let audio = sel
        .audio_rep_id
        .as_ref()
        .map(|id| mpd.select_representation(id, TrackKind::Audio).cloned())
        .transpose()?;
    let subtitle = sel
        .subtitle_rep_id
        .as_ref()
        .filter(|_| sel.enable_subtitles)
        .map(|id| mpd.select_representation(id, TrackKind::Subtitle).cloned())
        .transpose()?;
    if sel.key_mode.is_clear() {
        ensure_clear_mpd_manifest(&video, audio.as_ref(), subtitle.as_ref())?;
    }
    let source_segment_secs = representation_segment_duration_secs(&video);
    task.configure_fetch_tuning(live_tuning::is_short_segment(source_segment_secs));

    // 2. 下载 video init，按 codec 解析参数集 / dvcC / tenc(KID) / VideoRange
    task.set_stage("下载 init 段");
    let vinit = dl.get(&video.init_url).await?;
    let video_codec = VideoCodec::from_codecs(&video.codecs);
    let (video_init, vtenc) = read_dash_video_init(&vinit, video_codec, sel.key_mode)?;
    let DashVideoInit {
        media: mut video_track,
        params,
        dovi,
    } = video_init;
    let video_media_timescale = video_track.timescale;

    // VideoRange 判定：DV → PQ；否则从 SPS VUI transfer_characteristics 推断（HDR10=PQ/HLG/SDR）。
    let video_range = detect_video_range(video_codec, dovi.is_some(), &params);
    tracing::info!(task=%task.id, "codec={:?} dovi={:?} range={:?}", video_codec, dovi.map(|d| (d.profile, d.bl_compatibility_id)), video_range);

    let vkid = effective_mpd_kid(sel.key_mode, vtenc.as_ref(), Some(&video));

    // Select the actual audio track and validate its codec configuration before
    // constructing the MPEG-TS PMT; a manifest label alone is not sufficient.
    let mut atenc = None;
    let mut audio_track = None;
    if let Some(a) = &audio {
        let init = dl
            .get(&a.init_url)
            .await
            .map_err(|e| e.context("DASH audio init"))?;
        let (track, encryption) = read_dash_audio_init(&init, sel.key_mode)?;
        atenc = encryption;
        audio_track = Some(track);
        tracing::info!(task=%task.id, codec=?track.config.codec(), "DASH audio codec");
    }
    let audio_codec = audio_track.map(|track| track.config.codec());
    let audio_media_timescale = audio_track.map_or(48_000, |track| track.media.timescale);
    let akid = effective_mpd_kid(sel.key_mode, atenc.as_ref(), audio.as_ref());
    let mut stenc: Option<TrackEncryption> = None;
    let mut subtitle_media_timescale = None;
    if let Some(s) = subtitle.as_ref().filter(|s| !s.init_url.is_empty()) {
        match dl.get(&s.init_url).await {
            Ok(sinit) => {
                stenc = mpd_init_encryption(sel.key_mode, &sinit, &sinit, "DASH subtitle init")?;
                subtitle_media_timescale = media_timescale_from_init(&sinit);
            }
            Err(e) if sel.key_mode.is_clear() => {
                return Err(e.context("Clear mode could not validate DASH subtitle init"));
            }
            Err(_) => {}
        }
    }
    let skid = effective_mpd_kid(sel.key_mode, stenc.as_ref(), subtitle.as_ref());

    task.set_stage(if sel.key_mode.is_clear() {
        "无需 KEY"
    } else if sel.key_mode.is_dynamic() {
        "动态获取 KEY"
    } else {
        "解析 KEY"
    });
    let required_kids = required_mpd_kids(
        sel.key_mode,
        &video,
        vtenc.as_ref(),
        audio.as_ref(),
        atenc.as_ref(),
        subtitle.as_ref(),
        stenc.as_ref(),
    );
    let mut keystore = key_resolver::resolve_key_store(
        sel.key_mode,
        &sel.keys,
        required_kids.iter().map(String::as_str),
    )
    .await?;

    // 视频 key：按 video init 的 tenc KID 匹配（未加密则跳过解密）
    let mut vdec = build_mpd_video_decryptor(
        task,
        sel.key_mode,
        &keystore,
        vtenc.as_ref(),
        vkid.as_deref(),
    )?;

    let mut adec = build_mpd_audio_decryptor(
        task,
        sel.key_mode,
        &keystore,
        atenc.as_ref(),
        akid.as_deref(),
    );
    let mut sdec = build_mpd_subtitle_decryptor(
        task,
        sel.key_mode,
        &keystore,
        stenc.as_ref(),
        skid.as_deref(),
    );

    // 4. 计算时钟（扫描视频首段的最小 cts，用于 B 帧 composition 平移）
    let min_cts = scan_min_cts(dl, &video, video_track, sel.key_mode)
        .await?
        .unwrap_or(0);
    let clock = ClockState::new(min_cts, video_media_timescale, 90_000);
    let mut muxer = TsMuxer::new(
        &params,
        video_codec,
        dovi,
        audio_codec,
        audio_media_timescale,
        clock,
    );

    // 5. 段处理（static: 一次处理全部；dynamic/live: 循环重拉 MPD 取增量段）
    task.set_stage("转封装中");
    let mut is_live = mpd.is_dynamic;
    // 从 task 恢复累计 DTS（停止/暂停后重启时延续，保持时间戳连续）
    let mut acc_v_dts: u64 = task.acc_v_dts.load(std::sync::atomic::Ordering::Relaxed);
    let mut acc_a_dts: u64 = task.acc_a_dts.load(std::sync::atomic::Ordering::Relaxed);
    // 已处理到的视频段号：
    // - 续传（resume>0）：从断点继续
    // - 直播首次接入：贴近 live edge —— 跳过 timeShiftBuffer 历史段，从倒数第 N 段开始，
    //   起播延迟降到 ~N 个段周期（HLS 建议离 edge 留 ≥3 段缓冲防卡顿）
    // - 点播首次：从头
    let resume = task
        .resume_number
        .load(std::sync::atomic::Ordering::Relaxed);
    let first_num = video.segments.first().map(|s| s.number).unwrap_or(0);
    let live_segment_secs = source_segment_secs;
    let publish_delay = live_tuning::publish_delay_segments(
        is_live,
        live_tuning::is_short_segment(live_segment_secs),
    );
    task.hls.set_publish_delay_segments(publish_delay);
    let live_backfill_segments =
        live_tuning::initial_live_backfill_segments(live_segment_secs, publish_delay);
    let mut next_number =
        initial_next_number(&video, mpd.is_dynamic, resume, live_backfill_segments)
            .unwrap_or(first_num);
    // 当前可处理的段集合（static 一次给全；live 每轮重算增量）
    let mut video = video;
    let mut audio = audio;
    let mut subtitle = subtitle;
    let cancel = task.cancel_token();
    use std::sync::atomic::Ordering as O;

    // 短分片聚合缓冲区：跨 batch 累积 AUs，只到 8s 才 flush。
    // 由 process_batch 填充、run_inner 做最终残量 flush（任务结束/取消时）。
    let mut agg_vaus: Vec<AccessUnit> = Vec::new();
    let mut agg_aaus: Vec<AudioUnit> = Vec::new();
    let mut agg_dur_ts: u64 = 0;
    let mut agg_audio_dur_ts: u64 = 0;
    let mut pending_discontinuity = false;
    let mut current_v_kid = vkid;
    let mut current_a_kid = akid;
    let mut current_s_kid = skid;
    let mut current_v_init_url = video.init_url.clone();
    let mut current_a_init_url = audio.as_ref().map(|a| a.init_url.clone());
    let mut current_s_init_url = subtitle.as_ref().map(|s| s.init_url.clone());
    let mut last_v_tfdt: Option<u64> = None;
    let audio_required = sel.audio_rep_id.is_some();
    let mut subtitle_acc = SubtitleAccumulator::new();
    let mut audio_wait_seg: Option<u64> = None;
    let mut audio_wait_refreshes = 0u32;

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let cycle_started = std::time::Instant::now();

        // 分片号用于视频续传；跨轨匹配使用同一播放时间轴。
        let short_group_size = is_live.then(|| short_source_group_size(&video)).flatten();
        let mut new_jobs: Vec<SegmentJob> = Vec::new();
        let mut blocked_on_audio = false;
        if let Some(group_size) = short_group_size {
            let plan = build_short_source_live_jobs(
                &video,
                audio.as_ref(),
                subtitle.as_ref(),
                audio_required,
                next_number,
                group_size,
            );
            new_jobs = plan.jobs;
            if plan.skipped_incomplete_head {
                tracing::warn!(
                    task = %task.id,
                    group_size,
                    "short-source live window skipped incomplete head group before publishing next complete group"
                );
                mark_mpd_epoch_boundary(
                    task,
                    &mut muxer,
                    video_media_timescale,
                    &mut agg_vaus,
                    &mut agg_aaus,
                    &mut agg_dur_ts,
                    &mut agg_audio_dur_ts,
                    &mut subtitle_acc,
                    &mut pending_discontinuity,
                    audio_required,
                )?;
                save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
            }
            if new_jobs.is_empty() {
                if let Some(blocked) = plan.blocked_on_audio {
                    blocked_on_audio = true;
                    if audio_wait_seg == Some(blocked.seg) {
                        audio_wait_refreshes = audio_wait_refreshes.saturating_add(1);
                    } else {
                        audio_wait_seg = Some(blocked.seg);
                        audio_wait_refreshes = 1;
                    }
                    tracing::debug!(
                        task = %task.id,
                        seg = blocked.seg,
                        group_size,
                        wait_refreshes = audio_wait_refreshes,
                        audio_rep = audio.as_ref().map(|a| a.id.as_str()).unwrap_or("<none>"),
                        audio_first = audio.as_ref().and_then(|a| a.segments.first()).map(|s| s.number),
                        audio_last = audio.as_ref().and_then(|a| a.segments.last()).map(|s| s.number),
                        "short-source audio group not complete yet; keeping previous HLS window"
                    );
                    if is_live && audio_wait_refreshes >= AUDIO_MATCH_MAX_WAIT_REFRESHES {
                        tracing::warn!(
                            task = %task.id,
                            seg = blocked.seg,
                            skip_to = blocked.skip_to,
                            wait_refreshes = audio_wait_refreshes,
                            "short-source audio group stayed incomplete; skipping group to keep output moving"
                        );
                        mark_mpd_epoch_boundary(
                            task,
                            &mut muxer,
                            video_media_timescale,
                            &mut agg_vaus,
                            &mut agg_aaus,
                            &mut agg_dur_ts,
                            &mut agg_audio_dur_ts,
                            &mut subtitle_acc,
                            &mut pending_discontinuity,
                            audio_required,
                        )?;
                        next_number = blocked.skip_to;
                        save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
                        audio_wait_seg = None;
                        audio_wait_refreshes = 0;
                    }
                }
            }
        } else {
            let audio_timing = audio.as_ref().map(TimedTrack::new);
            let subtitle_timing = subtitle.as_ref().map(TimedTrack::new);

            for v in video.segments.iter().filter(|v| v.number >= next_number) {
                let audio_match = audio_timing
                    .as_ref()
                    .and_then(|a| a.match_video(v, video.timescale))
                    .map(|s| (s.url.clone(), s.duration_ts));
                let subtitle_match = subtitle_timing
                    .as_ref()
                    .and_then(|s| s.match_video(v, video.timescale))
                    .map(|s| (s.url.clone(), s.duration_ts));

                if audio_required && audio_match.is_none() {
                    if !is_live {
                        anyhow::bail!(
                            "DASH VOD selected audio does not cover video segment #{}",
                            v.number
                        );
                    }
                    blocked_on_audio = true;
                    if audio_wait_seg == Some(v.number) {
                        audio_wait_refreshes = audio_wait_refreshes.saturating_add(1);
                    } else {
                        audio_wait_seg = Some(v.number);
                        audio_wait_refreshes = 1;
                    }
                    tracing::debug!(
                        task = %task.id,
                        seg = v.number,
                        wait_refreshes = audio_wait_refreshes,
                        audio_rep = audio.as_ref().map(|a| a.id.as_str()).unwrap_or("<none>"),
                        audio_first = audio.as_ref().and_then(|a| a.segments.first()).map(|s| s.number),
                        audio_last = audio.as_ref().and_then(|a| a.segments.last()).map(|s| s.number),
                        "audio segment not available yet; waiting for next manifest refresh"
                    );
                    if is_live
                        && new_jobs.is_empty()
                        && audio_wait_refreshes >= AUDIO_MATCH_MAX_WAIT_REFRESHES
                    {
                        tracing::warn!(
                            task = %task.id,
                            seg = v.number,
                            wait_refreshes = audio_wait_refreshes,
                            "audio segment never appeared for live MPD segment; skipping to keep output moving"
                        );
                        mark_mpd_epoch_boundary(
                            task,
                            &mut muxer,
                            video_media_timescale,
                            &mut agg_vaus,
                            &mut agg_aaus,
                            &mut agg_dur_ts,
                            &mut agg_audio_dur_ts,
                            &mut subtitle_acc,
                            &mut pending_discontinuity,
                            audio_required,
                        )?;
                        next_number = v.number.saturating_add(1);
                        save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
                        audio_wait_seg = None;
                        audio_wait_refreshes = 0;
                    }
                    break;
                }

                let (aurl, audio_dur_ts) = match audio_match {
                    Some((url, dur)) => (Some(url), Some(dur)),
                    None => (None, None),
                };
                let surl = subtitle_match.map(|(url, _)| url);

                new_jobs.push(SegmentJob {
                    num: v.number,
                    vurl: v.url.clone(),
                    aurl,
                    surl,
                    video_dur_ts: v.duration_ts,
                    audio_dur_ts,
                    skip_to_on_failure: is_live.then(|| v.number.saturating_add(1)),
                });
            }
        }
        if !blocked_on_audio {
            audio_wait_seg = None;
            audio_wait_refreshes = 0;
        }

        let mut skipped_live_gap = false;
        if !new_jobs.is_empty() {
            // SegmentTemplate duration uses the manifest's clock; coverage,
            // DTS and muxing below use the media track's mdhd clock.
            for job in &mut new_jobs {
                job.video_dur_ts =
                    rescale_duration(job.video_dur_ts, video.timescale, video_media_timescale)?;
                if let (Some(duration), Some(a)) = (&mut job.audio_dur_ts, &audio) {
                    *duration = rescale_duration(*duration, a.timescale, audio_media_timescale)?;
                }
            }
            let batch_last = new_jobs.last().map(|j| j.num).unwrap_or(next_number);
            let progress = process_batch(
                task,
                dl,
                &vdec,
                &adec,
                &sdec,
                video_codec,
                video_track,
                audio_track,
                &mut muxer,
                video_media_timescale,
                subtitle_media_timescale
                    .or_else(|| subtitle.as_ref().map(|s| s.timescale))
                    .unwrap_or(1_000),
                SampleWindow::from_representation(
                    &video,
                    video_media_timescale,
                    video_track.media_offset,
                )?,
                audio
                    .as_ref()
                    .map(|rep| {
                        SampleWindow::from_representation(
                            rep,
                            audio_media_timescale,
                            audio_track.map_or(0, |track| track.media.media_offset),
                        )
                    })
                    .transpose()?,
                new_jobs,
                acc_v_dts,
                acc_a_dts,
                &mut agg_vaus,
                &mut agg_aaus,
                &mut agg_dur_ts,
                &mut agg_audio_dur_ts,
                &mut subtitle_acc,
                &mut pending_discontinuity,
                &mut last_v_tfdt,
                audio_required,
                is_live,
            )
            .await?;
            acc_v_dts = progress.acc_v_dts;
            acc_a_dts = progress.acc_a_dts;
            // next_number 用「实际处理到的位置」，而非批前预算值。
            // 被取消时 process_batch 提前返回，processed_until 只到已完成的段，
            // 避免把 resume_number 错误地跳到批末尾（否则恢复时无段可处理→误判完成）。
            next_number = progress.next_number;
            if let Some(skip) = progress.skipped_failure {
                tracing::warn!(
                    task = %task.id,
                    failed_seg = skip.failed_num,
                    skip_to = skip.skip_to,
                    "live MPD segment failed; skipping stale head range to stay near live edge"
                );
                drop_mpd_live_gap_boundary(
                    task,
                    video_media_timescale,
                    &mut agg_vaus,
                    &mut agg_aaus,
                    &mut agg_dur_ts,
                    &mut agg_audio_dur_ts,
                    &mut subtitle_acc,
                    &mut pending_discontinuity,
                );
                last_v_tfdt = None;
                skipped_live_gap = true;
                save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
            }
            let _ = batch_last;
        }

        if !is_live {
            break; // VOD 处理完即结束
        }

        if skipped_live_gap && video.segments.iter().any(|s| s.number >= next_number) {
            continue;
        }

        // live：等待约一个段时长后重拉 MPD 取增量
        task.set_stage("等待直播更新");
        let seg_dur = video
            .segments
            .last()
            .map(|s| s.duration_ts)
            .unwrap_or(video.timescale as u64);
        let seg_secs = seg_dur as f64 / video.timescale.max(1) as f64;
        let wait = live_tuning::media_refresh_wait(seg_secs, cycle_started.elapsed());
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }
        task.set_stage("转封装中");

        // 重拉 MPD
        match dl
            .get_manifest(&sel.mpd_url)
            .await
            .and_then(|m| parse_mpd(&m.text, &m.url))
        {
            Ok(fresh) => {
                // A live DASH origin may publish one final static snapshot.
                // Drain its remaining segments before the supervisor publishes ENDLIST.
                is_live = fresh.is_dynamic;
                if let Some(v) = fresh
                    .representations
                    .iter()
                    .find(|r| r.id == sel.video_rep_id && r.kind == TrackKind::Video)
                    .cloned()
                {
                    video = v;
                    // 直播 total_segments 随 MPD 刷新动态变化（timeShiftBuffer 窗口滑动）
                    task.total_segments
                        .store(video.segments.len() as u64, O::Relaxed);
                }
                if let Some(aid) = &sel.audio_rep_id {
                    audio = fresh
                        .representations
                        .iter()
                        .find(|r| &r.id == aid && r.kind == TrackKind::Audio)
                        .cloned();
                }
                if let Some(sid) = &sel.subtitle_rep_id {
                    subtitle = fresh
                        .representations
                        .iter()
                        .find(|r| &r.id == sid && r.kind == TrackKind::Subtitle)
                        .cloned();
                }
                let v_init_changed = current_v_init_url != video.init_url;
                let a_init_changed =
                    current_a_init_url != audio.as_ref().map(|a| a.init_url.clone());
                let s_init_changed =
                    current_s_init_url != subtitle.as_ref().map(|s| s.init_url.clone());
                if sel.key_mode.is_clear() {
                    ensure_clear_mpd_manifest(&video, audio.as_ref(), subtitle.as_ref())?;
                }
                let replacement_video = if v_init_changed {
                    let init = dl.get(&video.init_url).await?;
                    let (replacement, _) = read_dash_video_init(&init, video_codec, sel.key_mode)?;
                    anyhow::ensure!(
                        (video.codecs.is_empty()
                            || VideoCodec::from_codecs(&video.codecs) == video_codec)
                            && replacement.media.timescale == video_media_timescale,
                        "DASH video init changed codec or timescale; restart the task"
                    );
                    Some(replacement)
                } else {
                    None
                };
                if let Some(a) = audio.as_ref().filter(|_| a_init_changed) {
                    let init = dl.get(&a.init_url).await?;
                    let (track, _) = read_dash_audio_init(&init, sel.key_mode)?;
                    anyhow::ensure!(
                        track.media.timescale == audio_media_timescale
                            && Some(track.config.codec()) == audio_codec,
                        "DASH audio init changed codec or timescale; restart the task"
                    );
                    audio_track = Some(track);
                }
                if let Some(s) = subtitle
                    .as_ref()
                    .filter(|s| s_init_changed && !s.init_url.is_empty())
                {
                    let init = dl.get(&s.init_url).await?;
                    if sel.key_mode.is_clear() {
                        ensure_clear_init(&init, "DASH subtitle init")?;
                    }
                    subtitle_media_timescale = media_timescale_from_init(&init);
                } else if s_init_changed {
                    subtitle_media_timescale = None;
                }
                if v_init_changed || a_init_changed || s_init_changed {
                    mark_mpd_epoch_boundary(
                        task,
                        &mut muxer,
                        video_media_timescale,
                        &mut agg_vaus,
                        &mut agg_aaus,
                        &mut agg_dur_ts,
                        &mut agg_audio_dur_ts,
                        &mut subtitle_acc,
                        &mut pending_discontinuity,
                        audio_required,
                    )?;
                    save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
                    current_v_init_url = video.init_url.clone();
                    current_a_init_url = audio.as_ref().map(|a| a.init_url.clone());
                    current_s_init_url = subtitle.as_ref().map(|s| s.init_url.clone());
                    last_v_tfdt = None;
                }
                if let Some(replacement) = replacement_video {
                    // Flush the old epoch above before replacing its codec metadata.
                    video_track = replacement.media;
                    muxer = TsMuxer::new(
                        &replacement.params,
                        video_codec,
                        replacement.dovi,
                        audio_codec,
                        audio_media_timescale,
                        clock,
                    );
                }
                if sel.key_mode.is_dynamic() {
                    let new_keys = refresh_mpd_dynamic_decryptors(
                        task,
                        dl,
                        &video,
                        audio.as_ref(),
                        subtitle.as_ref(),
                        audio_codec,
                        &mut keystore,
                        &mut vdec,
                        &mut adec,
                        &mut sdec,
                        &mut audio_track,
                    )
                    .await?;
                    if key_changed(current_v_kid.as_ref(), new_keys.video_kid.as_ref())
                        || key_changed(current_a_kid.as_ref(), new_keys.audio_kid.as_ref())
                        || key_changed(current_s_kid.as_ref(), new_keys.subtitle_kid.as_ref())
                    {
                        flush_mpd_aggregate(
                            task,
                            &mut muxer,
                            video_media_timescale,
                            &mut agg_vaus,
                            &mut agg_aaus,
                            &mut agg_dur_ts,
                            &mut agg_audio_dur_ts,
                            &mut subtitle_acc,
                            &mut pending_discontinuity,
                        )?;
                        save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
                    }
                    current_v_kid = new_keys.video_kid;
                    current_a_kid = new_keys.audio_kid;
                    current_s_kid = new_keys.subtitle_kid;
                }
            }
            Err(e) => tracing::warn!(task=%task.id, "refresh mpd failed: {e:#}"),
        }
    }

    // 最终 flush：任务结束时残留不足 8s 的累积段也输出。
    // 正常运行时不会到这里（直播 cancels_token 循环 break），此时 flush 残留。
    if !agg_vaus.is_empty()
        && aggregate_audio_ready(
            audio_required,
            agg_audio_dur_ts,
            muxer.audio_timescale,
            agg_dur_ts,
            video_media_timescale,
        )
    {
        flush_mpd_aggregate(
            task,
            &mut muxer,
            video_media_timescale,
            &mut agg_vaus,
            &mut agg_aaus,
            &mut agg_dur_ts,
            &mut agg_audio_dur_ts,
            &mut subtitle_acc,
            &mut pending_discontinuity,
        )?;
        save_mpd_progress(task, next_number, acc_v_dts, acc_a_dts);
    } else if !agg_vaus.is_empty() {
        if !is_live && !cancel.is_cancelled() {
            anyhow::bail!("DASH VOD final aggregate has incomplete audio coverage");
        }
        tracing::warn!(
            task = %task.id,
            "dropping final aggregate because audio coverage is incomplete"
        );
    }

    Ok(())
}

/// 处理一批段（并发预取下载 + 顺序解密封装），返回更新后的 (acc_v_dts, acc_a_dts)。
#[allow(clippy::too_many_arguments)]
async fn process_batch(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    vdec: &Option<Decryptor>,
    adec: &Option<Decryptor>,
    sdec: &Option<Decryptor>,
    video_codec: VideoCodec,
    video_track: DashMediaTrack,
    audio_track: Option<DashAudioTrack>,
    muxer: &mut TsMuxer,
    video_timescale: u32,
    subtitle_timescale: u32,
    video_window: SampleWindow,
    audio_window: Option<SampleWindow>,
    jobs: Vec<SegmentJob>,
    mut acc_v_dts: u64,
    mut acc_a_dts: u64,
    // 聚合缓冲区：跨 batch 累积 AUs，只在达到 8s 阈值时才 flush 成一个 TS 段。
    // 不再在单批结束时 flush 残留（会导致时长逐段递增的小尾巴段，播放器卡住）。
    acc_vaus: &mut Vec<AccessUnit>,
    acc_aaus: &mut Vec<AudioUnit>,
    acc_dur_ts: &mut u64,
    agg_audio_dur_ts: &mut u64,
    subtitle_acc: &mut SubtitleAccumulator,
    pending_discontinuity: &mut bool,
    last_v_tfdt: &mut Option<u64>,
    audio_required: bool,
    allow_live_skip: bool,
) -> crate::Result<BatchProgress> {
    // 记录批内首段号，用于在未处理任何段时返回正确的 next 位置
    let batch_first = jobs.first().map(|j| j.num).unwrap_or(0);
    // 实际处理到的「下一个待处理段号」（被取消时只到已完成处）
    let mut next_processed = batch_first;
    // Don't let a slow head download accumulate all of a VOD in the reorder
    // buffer. Keep a concurrent but bounded ordered window.
    let dl2 = dl.clone();
    let task2 = task.clone();
    let fetch_concurrency = task.segment_fetch_concurrency();
    let download_stream = futures::stream::iter(jobs.into_iter().map(move |job| {
        let dl = dl2.clone();
        let task = task2.clone();
        async move {
            let SegmentJob {
                num,
                vurl,
                aurl,
                surl,
                video_dur_ts,
                audio_dur_ts,
                skip_to_on_failure,
            } = job;
            let dl_video = dl.clone();
            let dl_audio = dl.clone();
            let dl_subtitle = dl.clone();
            let task_video = task.clone();
            let task_audio = task.clone();
            let task_subtitle = task.clone();
            let video = async move {
                fetch_validated_media(
                    &task_video,
                    &dl_video,
                    &vurl,
                    Some(video_dur_ts),
                    "video",
                    num,
                    video_track,
                )
                .await
            };
            let audio = async move {
                match aurl {
                    Some(u) => {
                        let track = audio_track.ok_or_else(|| {
                            anyhow::anyhow!("DASH audio has no validated configuration")
                        })?;
                        fetch_validated_media(
                            &task_audio,
                            &dl_audio,
                            &u,
                            audio_dur_ts,
                            "audio",
                            num,
                            track.media,
                        )
                        .await
                        .map(Some)
                    }
                    None => Ok(None),
                }
            };
            let subtitle = async move {
                match surl {
                    Some(u) => match task_subtitle.fetch_media(&dl_subtitle, &u).await {
                        Ok(data) => {
                            if task_subtitle.sel.key_mode.is_clear() {
                                ensure_clear_fragment(&data, "DASH subtitle")?;
                            }
                            Ok(Some(data))
                        }
                        Err(e) => {
                            tracing::warn!(
                                task = %task_subtitle.id,
                                seg = num,
                                url = %u,
                                "subtitle segment fetch failed; publishing empty subtitle: {e:#}"
                            );
                            Ok(None)
                        }
                    },
                    None => Ok(None),
                }
            };
            match tokio::try_join!(video, audio, subtitle) {
                Ok((venc, aenc, senc)) => Ok(DownloadedSegment {
                    num,
                    venc,
                    aenc,
                    senc,
                }),
                Err(error) => Err(SegmentFetchFailure {
                    num,
                    skip_to_on_failure,
                    error,
                }),
            }
        }
    }))
    .buffered(fetch_concurrency);
    futures::pin_mut!(download_stream);
    let cancel = task.cancel_token();
    while let Some(item) = download_stream.next().await {
        if cancel.is_cancelled() {
            break;
        }
        let segment = match item {
            Ok(segment) => segment,
            Err(failure) => {
                if !allow_live_skip || failure.error.is::<ProtectedInput>() {
                    return Err(failure.error.context(format!(
                        "DASH segment #{} fetch/validation failed",
                        failure.num
                    )));
                }
                let skipped_failure = failure
                    .skip_to_on_failure
                    .filter(|skip_to| *skip_to > next_processed)
                    .map(|skip_to| SkippedFailure {
                        failed_num: failure.num,
                        skip_to,
                    });
                tracing::warn!(
                    task = %task.id,
                    seg = failure.num,
                    "live segment fetch/validation failed: {:#}",
                    failure.error
                );
                return Ok(BatchProgress {
                    acc_v_dts,
                    acc_a_dts,
                    next_number: skipped_failure.map_or(next_processed, |skip| skip.skip_to),
                    skipped_failure,
                });
            }
        };
        let num = segment.num;
        let published = process_downloaded_segment(
            task,
            vdec,
            adec,
            sdec,
            video_codec,
            video_track,
            audio_track,
            muxer,
            video_timescale,
            subtitle_timescale,
            video_window,
            audio_window,
            segment,
            &mut acc_v_dts,
            &mut acc_a_dts,
            acc_vaus,
            acc_aaus,
            acc_dur_ts,
            agg_audio_dur_ts,
            subtitle_acc,
            pending_discontinuity,
            last_v_tfdt,
            audio_required,
        )?;
        next_processed = num + 1; // MPD parser requires room for the cursor.
        if published {
            save_mpd_progress(task, next_processed, acc_v_dts, acc_a_dts);
        }
    }

    Ok(BatchProgress {
        acc_v_dts,
        acc_a_dts,
        next_number: next_processed,
        skipped_failure: None,
    })
}

/// Persist only published sequence boundaries (or an intentional live skip).
/// Pending aggregates are not durable and must be retried after a forced stop.
fn save_mpd_progress(task: &Task, next_number: u64, video_dts: u64, audio_dts: u64) {
    task.acc_v_dts.store(video_dts, Ordering::Relaxed);
    task.acc_a_dts.store(audio_dts, Ordering::Relaxed);
    task.resume_number.store(next_number, Ordering::Relaxed);
}

struct BatchProgress {
    acc_v_dts: u64,
    acc_a_dts: u64,
    next_number: u64,
    skipped_failure: Option<SkippedFailure>,
}

#[derive(Clone, Copy)]
struct SkippedFailure {
    failed_num: u64,
    skip_to: u64,
}

struct SegmentJob {
    num: u64,
    vurl: String,
    aurl: Option<String>,
    surl: Option<String>,
    video_dur_ts: u64,
    audio_dur_ts: Option<u64>,
    skip_to_on_failure: Option<u64>,
}

struct SegmentFetchFailure {
    num: u64,
    skip_to_on_failure: Option<u64>,
    error: anyhow::Error,
}

struct DownloadedSegment {
    num: u64,
    venc: bytes::Bytes,
    aenc: Option<bytes::Bytes>,
    senc: Option<bytes::Bytes>,
}

struct ShortSourceJobPlan {
    jobs: Vec<SegmentJob>,
    blocked_on_audio: Option<ShortSourceAudioBlock>,
    skipped_incomplete_head: bool,
}

struct ShortSourceAudioBlock {
    seg: u64,
    skip_to: u64,
}

fn short_source_group_size(rep: &Representation) -> Option<usize> {
    let count = rep.segments.len();
    if count < SHORT_SOURCE_MIN_SEGMENTS {
        return None;
    }

    let mut total = 0.0;
    let mut short = 0usize;
    for seg in &rep.segments {
        let dur = segment_duration_secs(seg, rep.timescale);
        if !dur.is_finite() || dur <= 0.0 {
            return None;
        }
        if dur < SHORT_SEG_THRESHOLD_S {
            short += 1;
        }
        total += dur;
    }

    let avg = total / count as f64;
    if avg >= SHORT_SEG_THRESHOLD_S
        || short * SHORT_SOURCE_MIN_SHORT_RATIO_DEN < count * SHORT_SOURCE_MIN_SHORT_RATIO_NUM
    {
        return None;
    }

    Some(((AGGREGATE_TARGET_S / avg).round() as usize).max(2))
}

fn build_short_source_live_jobs(
    video: &Representation,
    audio: Option<&Representation>,
    subtitle: Option<&Representation>,
    audio_required: bool,
    next_number: u64,
    group_size: usize,
) -> ShortSourceJobPlan {
    let groups = short_source_video_groups(video, next_number, group_size);
    let audio_timing = audio.map(TimedTrack::new);
    let subtitle_timing = subtitle.map(TimedTrack::new);
    let mut jobs = Vec::new();
    let mut blocked_on_audio = None;
    let mut skipped_incomplete_head = false;

    for (idx, group) in groups.iter().enumerate() {
        let is_tail = idx + 1 == groups.len();
        if group.len() != group_size {
            if !is_tail {
                skipped_incomplete_head = true;
                continue;
            }
            break;
        }

        let audio_matches = if audio_required {
            match audio_timing
                .as_ref()
                .and_then(|a| short_source_group_audio_matches(video, group, a))
            {
                Some(matches) => Some(matches),
                None => {
                    if !is_tail {
                        skipped_incomplete_head = true;
                        continue;
                    }
                    if blocked_on_audio.is_none() {
                        blocked_on_audio = Some(ShortSourceAudioBlock {
                            seg: group.first().map(|s| s.number).unwrap_or(next_number),
                            skip_to: group
                                .last()
                                .map(|s| s.number.saturating_add(1))
                                .unwrap_or(next_number),
                        });
                    }
                    break;
                }
            }
        } else {
            None
        };

        let skip_to_on_failure = group
            .last()
            .map(|seg| seg.number.saturating_add(1))
            .unwrap_or(next_number);
        for (pos, v) in group.iter().enumerate() {
            let audio_match = audio_matches
                .as_ref()
                .and_then(|matches| matches.get(pos))
                .map(|seg| (seg.url.clone(), seg.duration_ts));
            let subtitle_match = subtitle_timing
                .as_ref()
                .and_then(|s| s.match_video(v, video.timescale))
                .map(|seg| (seg.url.clone(), seg.duration_ts));
            jobs.push(SegmentJob {
                num: v.number,
                vurl: v.url.clone(),
                aurl: audio_match.as_ref().map(|(url, _)| url.clone()),
                surl: subtitle_match.as_ref().map(|(url, _)| url.clone()),
                video_dur_ts: v.duration_ts,
                audio_dur_ts: audio_match.map(|(_, dur)| dur),
                skip_to_on_failure: Some(skip_to_on_failure),
            });
        }
    }

    if !jobs.is_empty() {
        blocked_on_audio = None;
    }

    ShortSourceJobPlan {
        jobs,
        blocked_on_audio,
        skipped_incomplete_head,
    }
}

fn short_source_video_groups<'a>(
    video: &'a Representation,
    next_number: u64,
    group_size: usize,
) -> Vec<Vec<&'a SegmentRef>> {
    let nominal_duration_ts = nominal_segment_duration_ts(video).unwrap_or(1);
    let time_template = is_time_template(video);
    let mut groups: BTreeMap<u64, Vec<&SegmentRef>> = BTreeMap::new();

    for (idx, seg) in video.segments.iter().enumerate() {
        if seg.number < next_number {
            continue;
        }
        let group_id = short_source_group_id(
            video,
            idx,
            seg,
            group_size,
            nominal_duration_ts,
            time_template,
        );
        groups.entry(group_id).or_default().push(seg);
    }

    groups.into_values().collect()
}

fn short_source_group_id(
    video: &Representation,
    idx: usize,
    seg: &SegmentRef,
    group_size: usize,
    nominal_duration_ts: u64,
    time_template: bool,
) -> u64 {
    let ordinal = if time_template {
        seg.number / nominal_duration_ts.max(1)
    } else {
        seg.number
            .saturating_sub(video.start_number)
            .max(idx as u64)
    };
    ordinal / group_size.max(1) as u64
}

fn nominal_segment_duration_ts(rep: &Representation) -> Option<u64> {
    if rep.segments.is_empty() {
        return None;
    }
    let total = rep
        .segments
        .iter()
        .map(|seg| seg.duration_ts as u128)
        .sum::<u128>();
    Some(((total + rep.segments.len() as u128 / 2) / rep.segments.len() as u128).max(1) as u64)
}

fn short_source_group_audio_matches<'a>(
    video: &Representation,
    group: &[&SegmentRef],
    audio: &TimedTrack<'a>,
) -> Option<Vec<&'a SegmentRef>> {
    let mut matches = Vec::with_capacity(group.len());
    for video_seg in group {
        matches.push(audio.match_video(video_seg, video.timescale)?);
    }
    short_source_audio_covers_group(video, group, audio.timescale, &matches).then_some(matches)
}

fn short_source_audio_covers_group(
    video: &Representation,
    group: &[&SegmentRef],
    audio_timescale: u32,
    audio_matches: &[&SegmentRef],
) -> bool {
    let Some((group_start, group_end)) = segment_group_interval(video, group) else {
        return false;
    };
    let group_duration = group_end - group_start;
    if group_duration <= 0.0 {
        return false;
    }

    let mut seen = BTreeSet::new();
    let mut intervals = Vec::new();
    for seg in audio_matches {
        if !seen.insert(seg.number) {
            continue;
        }
        let start = seg.presentation_start_secs;
        let end = start + segment_duration_secs(seg, audio_timescale);
        let clipped_start = group_start.max(start - SHORT_GROUP_AUDIO_EDGE_SLACK_S);
        let clipped_end = group_end.min(end + SHORT_GROUP_AUDIO_EDGE_SLACK_S);
        if clipped_end > clipped_start {
            intervals.push((clipped_start, clipped_end));
        }
    }

    let covered = merged_interval_duration(intervals);
    covered * MIN_MEDIA_COVERAGE_DEN as f64 >= group_duration * MIN_MEDIA_COVERAGE_NUM as f64
}

fn segment_group_interval(rep: &Representation, group: &[&SegmentRef]) -> Option<(f64, f64)> {
    let first = group.first()?;
    let last = group.last()?;
    let start = first.presentation_start_secs;
    let end = segment_end_secs(last, rep.timescale);
    Some((start, end))
}

fn merged_interval_duration(mut intervals: Vec<(f64, f64)>) -> f64 {
    if intervals.is_empty() {
        return 0.0;
    }
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut covered = 0.0;
    let (mut current_start, mut current_end) = intervals[0];
    for (start, end) in intervals.into_iter().skip(1) {
        if start <= current_end {
            current_end = current_end.max(end);
        } else {
            covered += current_end - current_start;
            current_start = start;
            current_end = end;
        }
    }
    covered + current_end - current_start
}

async fn fetch_validated_media(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    url: &str,
    expected_duration_ts: Option<u64>,
    kind: &str,
    num: u64,
    track: DashMediaTrack,
) -> crate::Result<bytes::Bytes> {
    let mut last_reason = String::new();
    for attempt in 1..=MEDIA_CONTENT_ATTEMPTS {
        let data = task.fetch_media(dl, url).await?;
        if task.sel.key_mode.is_clear() {
            ensure_clear_fragment(&data, kind)?;
        }
        match validate_media_coverage(&data, expected_duration_ts, track) {
            Ok(()) => return Ok(data),
            Err(reason) => {
                last_reason = reason;
                if attempt < MEDIA_CONTENT_ATTEMPTS {
                    let delay = media_content_retry_delay(attempt);
                    tracing::debug!(
                        task = %task.id,
                        seg = num,
                        kind,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        reason = %last_reason,
                        "media segment not complete yet; retrying"
                    );
                    let cancel = task.cancel_token();
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            return Err(anyhow::anyhow!("cancelled while waiting for {kind} seg #{num}"));
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    Err(anyhow::anyhow!(
        "{kind} seg #{num} incomplete after {} attempts: {}",
        MEDIA_CONTENT_ATTEMPTS,
        last_reason
    ))
}

fn validate_media_coverage(
    data: &[u8],
    expected_duration_ts: Option<u64>,
    track: DashMediaTrack,
) -> Result<(), String> {
    let parsed = track
        .parse(data)
        .ok_or_else(|| "missing fMP4 sample table for selected track".to_string())?;
    if parsed.samples.is_empty() {
        return Err("no samples".to_string());
    }
    if let Some(expected) = expected_duration_ts.filter(|v| *v > 0) {
        let actual = parsed
            .samples
            .iter()
            .map(|s| s.duration as u64)
            .sum::<u64>();
        if actual.saturating_mul(MIN_MEDIA_COVERAGE_DEN)
            < expected.saturating_mul(MIN_MEDIA_COVERAGE_NUM)
        {
            return Err(format!(
                "duration {actual}/{expected} below coverage threshold"
            ));
        }
    }
    Ok(())
}

fn media_content_retry_delay(attempt: usize) -> Duration {
    Duration::from_millis((MEDIA_CONTENT_RETRY_BASE_MS * attempt as u64).min(1_000))
}

/// Whole-access-unit limits expressed in the native media clock, including the
/// input's simple edit-list offset. The upper bound is exclusive.
#[derive(Clone, Copy)]
struct SampleWindow {
    start: i128,
    end: Option<i128>,
    initial_edit_without_pto: bool,
}

struct VideoExtent {
    samples: std::ops::Range<usize>,
    decode_span: u64,
    presentation_span: u64,
}

impl SampleWindow {
    fn from_representation(
        rep: &Representation,
        media_timescale: u32,
        edit_media_time: i64,
    ) -> crate::Result<Self> {
        let timescale = u128::from(rep.timescale);
        let pto = u128::from(rep.presentation_time_offset) * u128::from(media_timescale);
        let anchor_whole = pto / timescale;
        let anchor_rem = pto % timescale;
        let start = i128::try_from(pto.div_ceil(timescale))? + i128::from(edit_media_time);
        let end = rep
            .period_duration
            .map(|duration| {
                // Round once after adding the fractional PTO and Period
                // duration. A whole sample starting at this tick is outside.
                const NANO: u128 = 1_000_000_000;
                let len = duration.as_nanos() * u128::from(media_timescale);
                let value = anchor_whole
                    + len / NANO
                    + (anchor_rem * NANO + (len % NANO) * timescale).div_ceil(timescale * NANO);
                Ok::<_, anyhow::Error>(i128::try_from(value)? + i128::from(edit_media_time))
            })
            .transpose()?;
        Ok(Self {
            start,
            end,
            initial_edit_without_pto: rep.presentation_time_offset == 0 && edit_media_time > 0,
        })
    }

    fn base(self, parsed: &ParsedSegment, track: &str, num: u64) -> crate::Result<i128> {
        match parsed.base_media_decode_time {
            Some(base) => Ok(i128::from(base)),
            None if self.start == 0 && self.end.is_none() => Ok(0),
            None => anyhow::bail!("DASH {track} segment #{num} has no tfdt for Period mapping"),
        }
    }

    fn video_extent(self, parsed: &ParsedSegment, num: u64) -> crate::Result<VideoExtent> {
        let base = self.base(parsed, "video", num)?;
        let mut min = None::<i128>;
        let mut max = None::<i128>;
        let mut keep = None::<std::ops::Range<usize>>;
        let mut decode_end = 0;
        for (i, (sample, &offset)) in parsed
            .samples
            .iter()
            .zip(&parsed.decode_time_offsets)
            .enumerate()
        {
            let pts = base + i128::from(offset) + i128::from(sample.cts_offset);
            let end = pts + i128::from(sample.duration);
            if end <= self.start || self.end.is_some_and(|limit| pts >= limit) {
                continue;
            }
            anyhow::ensure!(
                pts >= self.start && self.end.is_none_or(|limit| end <= limit),
                "DASH video segment #{num} crosses a Period boundary within an access unit; lossless cropping is unsupported"
            );
            if let Some(range) = &mut keep {
                // Reordered frames cannot be removed from the middle of the
                // decode sequence; retained AUs may depend on them.
                anyhow::ensure!(
                    range.end == i,
                    "DASH video segment #{num} has non-contiguous Period samples; lossless cropping is unsupported"
                );
                range.end = i + 1;
            } else {
                keep = Some(i..i + 1);
            }
            min = Some(min.map_or(pts, |old| old.min(pts)));
            max = Some(max.map_or(end, |old| old.max(end)));
            decode_end =
                decode_end.max(offset.checked_add(u64::from(sample.duration)).ok_or_else(
                    || anyhow::anyhow!("DASH video segment #{num} duration overflow"),
                )?);
        }
        let presentation_span = min
            .zip(max)
            .and_then(|(start, end)| u64::try_from(end - start).ok())
            .filter(|duration| *duration > 0)
            .ok_or_else(|| anyhow::anyhow!("DASH video segment #{num} has no timed samples"))?;
        let samples =
            keep.ok_or_else(|| anyhow::anyhow!("DASH video segment #{num} has no Period samples"))?;
        Ok(VideoExtent {
            decode_span: decode_end - parsed.decode_time_offsets[samples.start],
            presentation_span,
            samples,
        })
    }

    fn audio_samples(
        self,
        parsed: &ParsedSegment,
        num: u64,
        codec: AudioCodec,
    ) -> crate::Result<std::ops::Range<usize>> {
        let base = self.base(parsed, "audio", num)?;
        let mut keep = None::<std::ops::Range<usize>>;
        for (i, (sample, &offset)) in parsed
            .samples
            .iter()
            .zip(&parsed.decode_time_offsets)
            .enumerate()
        {
            let pts = base + i128::from(offset) + i128::from(sample.cts_offset);
            let end = pts + i128::from(sample.duration);
            // Use complete audio access units only. A trailing padding frame
            // can straddle the upper edge; omit it rather than advertise a
            // duration that requires trimming the compressed payload.
            if end <= self.start || self.end.is_some_and(|limit| end > limit) {
                continue;
            }
            // AC-3/E-AC-3 encoders can signal a sub-frame initial delay
            // with a single rate-1 edit. TS cannot convey the sample trim;
            // keep that source AU whole, allowing less than one AU of audio
            // pre-roll. Never round an MPD PTO, later or reordered boundary.
            let encoder_preroll = self.initial_edit_without_pto
                && matches!(codec, AudioCodec::Ac3 | AudioCodec::Ec3)
                && i == 0
                && base == 0
                && offset == 0
                && sample.cts_offset == 0;
            anyhow::ensure!(
                pts >= self.start || encoder_preroll,
                "DASH audio segment #{num} needs an unsupported partial leading crop"
            );
            if let Some(range) = &mut keep {
                anyhow::ensure!(
                    range.end == i,
                    "DASH audio segment #{num} has non-contiguous Period samples"
                );
                range.end = i + 1;
            } else {
                keep = Some(i..i + 1);
            }
        }
        keep.ok_or_else(|| {
            anyhow::anyhow!("DASH audio segment #{num} has no samples in its Period")
        })
    }
}

/// Common DASH init segments are unedited, or carry one rate-1 edit shifting
/// first PTS (e.g. FFmpeg's B-frame reordering or AAC priming). TS cannot
/// express an arbitrary edit list, so fail explicitly for that layout.
fn edit_media_offset(init: &[u8]) -> crate::Result<i64> {
    use crate::mp4::boxes::{find_box, CONTAINERS};
    let Some(edit) = find_box(init, b"elst", CONTAINERS) else {
        return Ok(0);
    };
    let read = || -> Option<(u32, i64, [u8; 4])> {
        let count = u32::from_be_bytes(edit.get(4..8)?.try_into().ok()?);
        if count == 0 {
            return Some((0, 0, [0, 1, 0, 0]));
        }
        let (time, rate) = match *edit.first()? {
            0 => (
                i64::from(i32::from_be_bytes(edit.get(12..16)?.try_into().ok()?)),
                edit.get(16..20)?,
            ),
            1 => (
                i64::from_be_bytes(edit.get(16..24)?.try_into().ok()?),
                edit.get(24..28)?,
            ),
            _ => return None,
        };
        Some((count, time, rate.try_into().ok()?))
    };
    let (count, time, rate) = read().ok_or_else(|| anyhow::anyhow!("invalid DASH edit list"))?;
    anyhow::ensure!(
        count <= 1 && time >= 0 && rate == [0, 1, 0, 0],
        "DASH media requires unsupported edit-list rendering"
    );
    Ok(time)
}

#[allow(clippy::too_many_arguments)]
fn process_downloaded_segment(
    task: &Arc<Task>,
    vdec: &Option<Decryptor>,
    adec: &Option<Decryptor>,
    sdec: &Option<Decryptor>,
    video_codec: VideoCodec,
    video_track: DashMediaTrack,
    audio_track: Option<DashAudioTrack>,
    muxer: &mut TsMuxer,
    video_timescale: u32,
    subtitle_timescale: u32,
    video_window: SampleWindow,
    audio_window: Option<SampleWindow>,
    segment: DownloadedSegment,
    acc_v_dts: &mut u64,
    acc_a_dts: &mut u64,
    acc_vaus: &mut Vec<AccessUnit>,
    acc_aaus: &mut Vec<AudioUnit>,
    acc_dur_ts: &mut u64,
    agg_audio_dur_ts: &mut u64,
    subtitle_acc: &mut SubtitleAccumulator,
    pending_discontinuity: &mut bool,
    last_v_tfdt: &mut Option<u64>,
    audio_required: bool,
) -> crate::Result<bool> {
    let DownloadedSegment {
        num,
        venc,
        aenc,
        senc,
    } = segment;

    // 累计下载字节（速度展示）
    let downloaded = venc.len() as u64
        + aenc.as_ref().map(|a| a.len() as u64).unwrap_or(0)
        + senc.as_ref().map(|s| s.len() as u64).unwrap_or(0);
    task.bytes_done.fetch_add(downloaded, Ordering::Relaxed);

    // 视频：解密(若加密) + 构造 AU
    let mut vparsed = video_track
        .parse(&venc)
        .ok_or_else(|| anyhow::anyhow!("parse video seg #{num} failed"))?;
    let video_extent = video_window.video_extent(&vparsed, num)?;
    let first_video_offset = vparsed.decode_time_offsets[video_extent.samples.start];
    if let Some(vdec) = vdec {
        vdec.decrypt_segment(&mut vparsed.mdat, &vparsed.samples);
    }
    if mpd_tfdt_rewound(*last_v_tfdt, vparsed.base_media_decode_time) {
        mark_mpd_epoch_boundary(
            task,
            muxer,
            video_timescale,
            acc_vaus,
            acc_aaus,
            acc_dur_ts,
            agg_audio_dur_ts,
            subtitle_acc,
            pending_discontinuity,
            audio_required,
        )?;
        save_mpd_progress(task, num, 0, 0);
        *acc_v_dts = 0;
        *acc_a_dts = 0;
    }
    *last_v_tfdt = vparsed.base_media_decode_time;
    // 时间线锚点：优先用源 tfdt 把每段 DTS 锚定到绝对时基（消除独立累加器的取整漂移）。
    // 首段记录第一个保留样本的 DTS；对裁掉前导 IDR 组的分片，也不能将时间起点留在裁掉的帧上。
    let seg_v_dts = match vparsed.base_media_decode_time {
        Some(tfdt) => {
            let first_dts = tfdt
                .checked_add(first_video_offset)
                .ok_or_else(|| anyhow::anyhow!("video segment #{num} first DTS overflow"))?;
            let origin = task.origin_v_tfdt.load(Ordering::Relaxed);
            let origin = if origin == u64::MAX {
                task.origin_v_tfdt.store(first_dts, Ordering::Relaxed);
                first_dts
            } else {
                origin
            };
            first_dts.saturating_sub(origin)
        }
        None => *acc_v_dts,
    };
    let leading_crop = video_extent.samples.start != 0;
    let mut vaus = Vec::with_capacity(video_extent.samples.len());
    for (s, offset) in vparsed.samples[video_extent.samples.clone()]
        .iter()
        .zip(&vparsed.decode_time_offsets[video_extent.samples])
    {
        let (a, b) = s.data_range;
        let frame = vparsed
            .mdat
            .get(a..b)
            .ok_or_else(|| anyhow::anyhow!("video segment #{num} sample outside mdat"))?;
        let dts = seg_v_dts
            .checked_add(offset - first_video_offset)
            .ok_or_else(|| anyhow::anyhow!("video segment #{num} DTS overflow"))?;
        let au = AccessUnit::from_sample(frame, dts, s.cts_offset as i64, video_codec);
        anyhow::ensure!(
            !au.nals.is_empty(),
            "invalid video access unit in segment #{num}"
        );
        if leading_crop && vaus.is_empty() {
            let closed_start = au.nals.iter().any(|nal| {
                let Some(&header) = nal.first() else {
                    return false;
                };
                match video_codec {
                    VideoCodec::H264 => {
                        crate::hevc::h264::is_irap(crate::hevc::h264::nal_type(header))
                    }
                    VideoCodec::Hevc => matches!(
                        crate::hevc::nal::nal_type(header),
                        crate::hevc::nal::IDR_W_RADL | crate::hevc::nal::IDR_N_LP
                    ),
                }
            });
            anyhow::ensure!(
                closed_start,
                "DASH video segment #{num} needs a leading crop before a non-IDR access unit; lossless cropping is unsupported"
            );
        }
        vaus.push(au);
    }
    // 回退累加器：始终延续段尾位置（tfdt 路径下不被消费，但保持续传时连续）。
    *acc_v_dts = seg_v_dts
        .checked_add(video_extent.decode_span)
        .ok_or_else(|| anyhow::anyhow!("video segment #{num} decode span overflow"))?;

    // 音频：按既有流程取得样本，再封装成对应的 TS 音频帧。
    let mut aaus = Vec::new();
    let mut aau_durations = Vec::new();
    let mut seg_a_dur_ts = 0u64;
    if let Some(aenc) = aenc {
        let track = audio_track
            .ok_or_else(|| anyhow::anyhow!("DASH audio has no validated configuration"))?;
        let mut aparsed = track
            .media
            .parse(&aenc)
            .ok_or_else(|| anyhow::anyhow!("parse audio seg #{num} failed"))?;
        let audio_window =
            audio_window.ok_or_else(|| anyhow::anyhow!("audio Period unavailable"))?;
        let audio_samples = audio_window.audio_samples(&aparsed, num, track.config.codec())?;
        let first_offset = aparsed.decode_time_offsets[audio_samples.start];
        if let Some(adec) = adec {
            adec.decrypt_segment(&mut aparsed.mdat, &aparsed.samples);
        }
        // 音频时间线锚定到「音频自身的 tfdt」：每段起点 = a_tfdt - a_origin。
        // tfdt 是连续音频流由源标注的绝对解码时间；直接据此定位，
        // 音频 PTS 恒等于源标注，内容连续、绝不重叠/重复。
        //
        // 不再强制对齐到「视频段边界」：实测源的真实 A/V offset 并非恒定
        //（曾观测到 82ms→50ms 的整帧级跳变 + 缓慢漂移），若按首段 offset 把音频
        // 硬塞到视频段边界，源 offset 变化时音频 PTS 就与音频实际内容错位整帧，
        // 在段衔接处表现为「音频重复播放（一句话重说）」。各自锚定 tfdt 后，
        // A/V offset 自然跟随源（与原始 DASH 播放器一致），既不重复也不漂移。
        // tfdt 缺失（非 CMAF）则回退到音频自累加（与旧行为一致）。
        let seg_a_pts = match aparsed.base_media_decode_time {
            Some(a_tfdt) => {
                let first_dts = a_tfdt
                    .checked_add(first_offset)
                    .ok_or_else(|| anyhow::anyhow!("audio segment #{num} DTS overflow"))?;
                let a_origin = task.origin_a_tfdt.load(Ordering::Relaxed);
                let a_origin = if a_origin == u64::MAX {
                    task.origin_a_tfdt.store(first_dts, Ordering::Relaxed);
                    first_dts
                } else {
                    a_origin
                };
                first_dts.saturating_sub(a_origin)
            }
            None => *acc_a_dts,
        };
        for (s, offset) in aparsed.samples[audio_samples.clone()]
            .iter()
            .zip(&aparsed.decode_time_offsets[audio_samples])
        {
            let (x, y) = s.data_range;
            let frame = aparsed
                .mdat
                .get(x..y)
                .ok_or_else(|| anyhow::anyhow!("audio segment #{num} sample outside mdat"))?;
            let pts = seg_a_pts
                .checked_add(offset.saturating_sub(first_offset))
                .ok_or_else(|| anyhow::anyhow!("audio segment #{num} PTS overflow"))?;
            let data = track
                .config
                .pack_sample(frame, s.cts_offset)
                .map_err(|e| e.context(format!("DASH audio segment #{num}")))?;
            aaus.push(AudioUnit { data, pts });
            aau_durations.push(s.duration as u64);
        }
        seg_a_dur_ts = aau_durations.iter().sum();
        *acc_a_dts = aaus
            .last()
            .zip(aau_durations.last())
            .and_then(|(au, duration)| au.pts.checked_add(*duration))
            .ok_or_else(|| anyhow::anyhow!("audio segment #{num} decode span overflow"))?;
    } else if audio_required {
        return Err(anyhow::anyhow!("audio seg #{num} missing"));
    }

    // 时间戳连续化：聚合时若子段 DTS/PTS 与预期下一帧位置有偏移（跨源段 PTS 底座
    // 不连续），则对子段全体 AU 加偏移量消除间隙。否则合段内会有 ~源段长 的空隙，
    // 播放器读到间隙就卡住不播。
    if !acc_vaus.is_empty() && !vaus.is_empty() {
        // 视频偏移量 = 预期下一帧 DTS - 子段首帧 DTS
        // 预期 = 首个累积段首帧 DTS + 累积时长（源 timescale）
        let first_v = acc_vaus.first().map(|a| a.dts).unwrap_or(0);
        let expected_v = first_v
            .checked_add(*acc_dur_ts)
            .ok_or_else(|| anyhow::anyhow!("DASH aggregate DTS overflow"))?;
        let v_off = i128::from(expected_v) - i128::from(vaus[0].dts);
        if v_off != 0 {
            for au in &mut vaus {
                au.dts = u64::try_from(i128::from(au.dts) + v_off)?;
            }
        }
    }
    let mut retained_a_dur_ts = seg_a_dur_ts;
    if !aaus.is_empty() {
        retained_a_dur_ts = trim_or_close_audio_gap(
            task,
            num,
            acc_aaus,
            *agg_audio_dur_ts,
            &mut aaus,
            &mut aau_durations,
            muxer.audio_timescale,
        );
    }

    let au_count = acc_vaus.len() + vaus.len() + acc_aaus.len() + aaus.len();
    let payload_sizes = acc_vaus
        .iter()
        .chain(&vaus)
        .flat_map(|au| au.nals.iter().map(Vec::len))
        .chain(acc_aaus.iter().chain(&aaus).map(|au| au.data.len()));
    anyhow::ensure!(
        au_count <= MAX_AUS_PER_AGGREGATE
            && payload_fits_budget(payload_sizes, MAX_AGGREGATE_PAYLOAD),
        "DASH output aggregate exceeds the pending access unit memory budget"
    );
    // 累积 AUs：短分片垒起来，封成一个合段再输出
    acc_vaus.extend(vaus);
    acc_aaus.extend(aaus);
    *acc_dur_ts += video_extent.presentation_span;
    *agg_audio_dur_ts += retained_a_dur_ts;
    let single_dur = video_extent.presentation_span as f64 / video_timescale as f64;
    if let Some(senc) = senc {
        if let Err(e) =
            subtitle_acc.append_fragment(&senc, single_dur, subtitle_timescale, sdec.as_ref(), None)
        {
            tracing::warn!(task = %task.id, seg = num, "subtitle conversion failed: {e:#}");
            subtitle_acc.append_empty(single_dur);
        }
    } else {
        subtitle_acc.append_empty(single_dur);
    }
    let acc_dur_secs = *acc_dur_ts as f64 / video_timescale as f64;

    // flush 条件：累积 ≥ 聚合目标(8s) 或 当前单段已足够长(≥4s)
    // 不等 batch 结束——残留由上层 run_inner 最终 flush，实现跨 batch 连续累积
    let duration_ready = acc_dur_secs >= AGGREGATE_TARGET_S || single_dur >= SHORT_SEG_THRESHOLD_S;
    let audio_ready = aggregate_audio_ready(
        audio_required,
        *agg_audio_dur_ts,
        muxer.audio_timescale,
        *acc_dur_ts,
        video_timescale,
    );
    let should_flush = duration_ready && audio_ready;
    if duration_ready && !audio_ready {
        anyhow::ensure!(
            acc_dur_secs < MAX_AGGREGATE_WHILE_WAITING_FOR_AUDIO_S,
            "DASH selected audio did not cover the bounded pending output aggregate"
        );
        tracing::warn!(
            task = %task.id,
            seg = num,
            video_ms = aggregate_duration_ms(*acc_dur_ts, video_timescale),
            audio_ms = aggregate_duration_ms(*agg_audio_dur_ts, muxer.audio_timescale),
            "holding aggregate because audio coverage is short"
        );
    }
    if should_flush {
        flush_mpd_aggregate(
            task,
            muxer,
            video_timescale,
            acc_vaus,
            acc_aaus,
            acc_dur_ts,
            agg_audio_dur_ts,
            subtitle_acc,
            pending_discontinuity,
        )?;
    }
    tracing::debug!(task = %task.id, seg = num, "segment muxed");
    Ok(should_flush)
}

fn payload_fits_budget(mut sizes: impl Iterator<Item = usize>, limit: usize) -> bool {
    sizes
        .try_fold(limit, |remaining, size| remaining.checked_sub(size))
        .is_some()
}

#[derive(Clone, Copy)]
struct DashMediaTrack {
    id: u32,
    timescale: u32,
    defaults: SampleDefaults,
    media_offset: i64,
}

impl DashMediaTrack {
    fn from_init_track(track: InitTrack<'_>) -> crate::Result<Self> {
        Ok(Self {
            id: track.track_id,
            timescale: track.timescale,
            defaults: track.defaults,
            media_offset: edit_media_offset(track.trak)?,
        })
    }

    fn parse(self, data: &[u8]) -> Option<ParsedSegment> {
        parse_media_segment_for_track(data, self.id, None, self.defaults)
    }
}

fn dash_init_track<'a>(
    init: &'a [u8],
    handler: &[u8; 4],
    kind: &str,
) -> crate::Result<InitTrack<'a>> {
    let mut matching = tracks_from_init(init)
        .ok_or_else(|| anyhow::anyhow!("DASH {kind} init has malformed track metadata"))?
        .into_iter()
        .filter(|track| &track.handler == handler);
    let selected = matching
        .next()
        .ok_or_else(|| anyhow::anyhow!("DASH init has no {kind} track"))?;
    anyhow::ensure!(
        matching.next().is_none(),
        "DASH {kind} init declares multiple {kind} tracks"
    );
    Ok(selected)
}

struct DashVideoInit {
    media: DashMediaTrack,
    params: ParamSets,
    dovi: Option<DoviConfig>,
}

fn read_dash_video_init(
    init: &[u8],
    codec: VideoCodec,
    key_mode: super::KeyMode,
) -> crate::Result<(DashVideoInit, Option<TrackEncryption>)> {
    let track = dash_init_track(init, b"vide", "video")?;
    let encryption = mpd_init_encryption(key_mode, init, track.trak, "DASH video init")?;
    let params = match codec {
        VideoCodec::Hevc => ParamSets::find_in_init(track.trak).unwrap_or_default(),
        VideoCodec::H264 => crate::mp4::avcc::find_avcc_in_init(track.trak).unwrap_or_default(),
    };
    let media = DashMediaTrack::from_init_track(track)?;
    let dovi = DoviConfig::find_in_init(track.trak);
    Ok((
        DashVideoInit {
            media,
            params,
            dovi,
        },
        encryption,
    ))
}

#[derive(Clone, Copy)]
struct DashAudioTrack {
    media: DashMediaTrack,
    config: Fmp4AudioConfig,
}

fn read_dash_audio_init(
    init: &[u8],
    key_mode: super::KeyMode,
) -> crate::Result<(DashAudioTrack, Option<TrackEncryption>)> {
    let track = dash_init_track(init, b"soun", "audio")?;
    let encryption = mpd_init_encryption(key_mode, init, track.trak, "DASH audio init")?;
    let config =
        Fmp4AudioConfig::from_init(track.trak).map_err(|e| e.context("DASH audio init"))?;
    let media = DashMediaTrack::from_init_track(track)?;
    Ok((DashAudioTrack { media, config }, encryption))
}

fn media_timescale_from_init(init: &[u8]) -> Option<u32> {
    crate::mp4::mdhd::timescale_from_init(init).filter(|scale| *scale != 0)
}

fn rescale_duration(ticks: u64, from: u32, to: u32) -> crate::Result<u64> {
    let scaled = (u128::from(ticks) * u128::from(to)).div_ceil(u128::from(from));
    u64::try_from(scaled).map_err(|_| anyhow::anyhow!("DASH media duration exceeds tick range"))
}

fn ensure_clear_mpd_manifest(
    video: &Representation,
    audio: Option<&Representation>,
    subtitle: Option<&Representation>,
) -> crate::Result<()> {
    for rep in [Some(video), audio, subtitle].into_iter().flatten() {
        if rep.has_content_protection || !rep.manifest_kids.is_empty() {
            return Err(ProtectedInput::new(&rep.id, "MPD ContentProtection").into());
        }
    }
    Ok(())
}

fn mpd_init_encryption(
    key_mode: super::KeyMode,
    init: &[u8],
    selected_track: &[u8],
    track: &str,
) -> crate::Result<Option<TrackEncryption>> {
    if key_mode.is_clear() {
        ensure_clear_init(init, track)?;
        return Ok(None);
    }
    Ok(TrackEncryption::find_in_init(selected_track))
}

fn effective_mpd_kid(
    key_mode: super::KeyMode,
    tenc: Option<&TrackEncryption>,
    rep: Option<&Representation>,
) -> Option<String> {
    if key_mode.is_clear() {
        return None;
    }
    let manifest_kid = rep.and_then(|r| r.manifest_kids.first()).cloned();
    let tenc_kid = tenc.map(|t| t.kid.clone());
    if key_mode.is_dynamic() {
        manifest_kid.or(tenc_kid)
    } else {
        tenc_kid.or(manifest_kid)
    }
}

fn required_mpd_kids(
    key_mode: super::KeyMode,
    video: &Representation,
    vtenc: Option<&TrackEncryption>,
    audio: Option<&Representation>,
    atenc: Option<&TrackEncryption>,
    subtitle: Option<&Representation>,
    stenc: Option<&TrackEncryption>,
) -> Vec<String> {
    if key_mode.is_clear() {
        return Vec::new();
    }
    let mut kids = Vec::new();
    extend_required_mpd_rep_kids(&mut kids, key_mode, Some(video), vtenc);
    if let Some(audio) = audio {
        extend_required_mpd_rep_kids(&mut kids, key_mode, Some(audio), atenc);
    }
    if let Some(subtitle) = subtitle {
        extend_required_mpd_rep_kids(&mut kids, key_mode, Some(subtitle), stenc);
    }
    kids
}

fn extend_required_mpd_rep_kids(
    target: &mut Vec<String>,
    key_mode: super::KeyMode,
    rep: Option<&Representation>,
    tenc: Option<&TrackEncryption>,
) {
    if key_mode.is_clear() {
        return;
    }
    let manifest_kids = rep.map(|r| r.manifest_kids.as_slice()).unwrap_or(&[]);
    let tenc_kid = tenc.map(|t| t.kid.clone());
    if key_mode.is_dynamic() {
        if manifest_kids.is_empty() {
            extend_unique_kids(target, tenc_kid);
        } else {
            extend_unique_kids(target, manifest_kids.iter().cloned());
        }
    } else if let Some(kid) = tenc_kid {
        extend_unique_kids(target, [kid]);
    } else {
        extend_unique_kids(target, manifest_kids.iter().cloned());
    }
}

fn extend_unique_kids(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn build_mpd_video_decryptor(
    task: &Arc<Task>,
    key_mode: super::KeyMode,
    keystore: &KeyStore,
    vtenc: Option<&TrackEncryption>,
    vkid: Option<&str>,
) -> crate::Result<Option<Decryptor>> {
    if key_mode.is_clear() {
        tracing::info!(task=%task.id, "video clear mode, no decryption");
        return Ok(None);
    }
    match vkid
        .and_then(|k| keystore.get(k))
        .or_else(|| keystore.get(""))
    {
        Some(vkey) => {
            tracing::info!(task=%task.id, "video KID={:?} matched key", vkid);
            Ok(Some(Decryptor::new(vkey)))
        }
        None if vtenc.is_none() => {
            tracing::info!(task=%task.id, "video unencrypted, no decryption");
            Ok(None)
        }
        None => Err(anyhow::anyhow!("no key matches video KID {:?}", vkid)),
    }
}

fn build_mpd_audio_decryptor(
    task: &Arc<Task>,
    key_mode: super::KeyMode,
    keystore: &KeyStore,
    atenc: Option<&TrackEncryption>,
    akid: Option<&str>,
) -> Option<Decryptor> {
    if key_mode.is_clear() {
        tracing::info!(task=%task.id, "audio clear mode, no decryption");
        return None;
    }
    if let Some(akey) = akid
        .and_then(|k| keystore.get(k))
        .or_else(|| keystore.get(""))
    {
        tracing::info!(task=%task.id, "audio KID={:?} matched key", akid);
        Some(match atenc.and_then(|t| t.constant_iv) {
            Some(civ) => Decryptor::with_constant_iv(akey, civ),
            None => Decryptor::new(akey),
        })
    } else {
        if atenc.is_some() {
            tracing::warn!(task=%task.id, "no key matches audio KID {:?}, skip audio", akid);
        }
        None
    }
}

fn build_mpd_subtitle_decryptor(
    task: &Arc<Task>,
    key_mode: super::KeyMode,
    keystore: &KeyStore,
    stenc: Option<&TrackEncryption>,
    skid: Option<&str>,
) -> Option<Decryptor> {
    if key_mode.is_clear() {
        tracing::info!(task=%task.id, "subtitle clear mode, no decryption");
        return None;
    }
    if let Some(skey) = skid
        .and_then(|k| keystore.get(k))
        .or_else(|| keystore.get(""))
    {
        tracing::info!(task=%task.id, "subtitle KID={:?} matched key", skid);
        Some(match stenc.and_then(|t| t.constant_iv) {
            Some(civ) => Decryptor::with_constant_iv(skey, civ),
            None => Decryptor::new(skey),
        })
    } else {
        if stenc.is_some() {
            tracing::warn!(task=%task.id, "no key matches subtitle KID {:?}, subtitles may be empty", skid);
        }
        None
    }
}

struct MpdKeyState {
    video_kid: Option<String>,
    audio_kid: Option<String>,
    subtitle_kid: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn refresh_mpd_dynamic_decryptors(
    task: &Arc<Task>,
    dl: &crate::fetch::SharedDownloader,
    video: &Representation,
    audio: Option<&Representation>,
    subtitle: Option<&Representation>,
    audio_codec: Option<AudioCodec>,
    keystore: &mut KeyStore,
    vdec: &mut Option<Decryptor>,
    adec: &mut Option<Decryptor>,
    sdec: &mut Option<Decryptor>,
    audio_track: &mut Option<DashAudioTrack>,
) -> crate::Result<MpdKeyState> {
    task.set_stage("检查动态 KEY");
    let vinit = dl.get(&video.init_url).await?;
    let video_track = dash_init_track(&vinit, b"vide", "video")?;
    let vtenc = TrackEncryption::find_in_init(video_track.trak);
    let vkid = effective_mpd_kid(super::KeyMode::Dynamic, vtenc.as_ref(), Some(video));

    let mut atenc = None;
    if let Some(a) = audio {
        let ainit = dl.get(&a.init_url).await?;
        let (track, encryption) = read_dash_audio_init(&ainit, super::KeyMode::Dynamic)?;
        atenc = encryption;
        anyhow::ensure!(
            Some(track.config.codec()) == audio_codec
                && audio_track
                    .is_some_and(|previous| previous.media.timescale == track.media.timescale),
            "DASH audio init changed codec or timescale; restart the task"
        );
        *audio_track = Some(track);
    }
    let akid = effective_mpd_kid(super::KeyMode::Dynamic, atenc.as_ref(), audio);
    let mut stenc = None;
    if let Some(s) = subtitle {
        let sinit = dl.get(&s.init_url).await?;
        stenc = TrackEncryption::find_in_init(&sinit);
    }
    let skid = effective_mpd_kid(super::KeyMode::Dynamic, stenc.as_ref(), subtitle);
    let required_kids = required_mpd_kids(
        super::KeyMode::Dynamic,
        video,
        vtenc.as_ref(),
        audio,
        atenc.as_ref(),
        subtitle,
        stenc.as_ref(),
    );
    key_resolver::fetch_missing_dynamic_keys(keystore, required_kids.iter().map(String::as_str))
        .await?;

    *vdec = build_mpd_video_decryptor(
        task,
        super::KeyMode::Dynamic,
        keystore,
        vtenc.as_ref(),
        vkid.as_deref(),
    )?;
    *adec = build_mpd_audio_decryptor(
        task,
        super::KeyMode::Dynamic,
        keystore,
        atenc.as_ref(),
        akid.as_deref(),
    );
    *sdec = build_mpd_subtitle_decryptor(
        task,
        super::KeyMode::Dynamic,
        keystore,
        stenc.as_ref(),
        skid.as_deref(),
    );
    task.set_stage("转封装中");
    Ok(MpdKeyState {
        video_kid: vkid,
        audio_kid: akid,
        subtitle_kid: skid,
    })
}

fn trim_or_close_audio_gap(
    task: &Arc<Task>,
    segment_num: u64,
    acc_aaus: &[AudioUnit],
    agg_audio_dur_ts: u64,
    aaus: &mut Vec<AudioUnit>,
    durations: &mut Vec<u64>,
    audio_timescale: u32,
) -> u64 {
    if acc_aaus.is_empty() || aaus.is_empty() {
        return durations.iter().sum();
    }

    let first_a = acc_aaus.first().map(|a| a.pts).unwrap_or(0);
    let expected_a = first_a.saturating_add(agg_audio_dur_ts);
    let first_new = aaus[0].pts;

    if first_new > expected_a {
        let gap = first_new - expected_a;
        let frame = durations
            .first()
            .copied()
            .filter(|v| *v > 0)
            .unwrap_or_else(|| (audio_timescale / 50).max(1) as u64);
        let max_gap = frame.saturating_mul(2);
        if gap <= max_gap {
            for au in aaus.iter_mut() {
                au.pts = au.pts.saturating_sub(gap);
            }
        }
    }

    let drop_count = aaus.iter().take_while(|au| au.pts < expected_a).count();
    if drop_count > 0 {
        let dropped_dur: u64 = durations.iter().take(drop_count).sum();
        tracing::debug!(
            task = %task.id,
            seg = segment_num,
            dropped_audio_frames = drop_count,
            dropped_audio_ms = aggregate_duration_ms(dropped_dur, audio_timescale),
            "trimmed overlapping audio frames while aggregating"
        );
        aaus.drain(0..drop_count);
        durations.drain(0..drop_count);
    }

    durations.iter().sum()
}

/// Index each companion track once per scheduling pass, independently of its URL
/// numbering scheme or current live-window boundaries.
struct TimedTrack<'a> {
    timescale: u32,
    segments: Vec<&'a SegmentRef>,
    max_end_through: Vec<f64>,
}

impl<'a> TimedTrack<'a> {
    fn new(rep: &'a Representation) -> Self {
        let mut segments: Vec<_> = rep.segments.iter().collect();
        if !segments.is_sorted_by(|a, b| a.presentation_start_secs <= b.presentation_start_secs) {
            segments.sort_by(|a, b| {
                a.presentation_start_secs
                    .total_cmp(&b.presentation_start_secs)
            });
        }
        // A prefix maximum finds intervals spanning the requested start even
        // when a later, shorter interval ends first.
        let max_end_through = segments
            .iter()
            .scan(f64::NEG_INFINITY, |max_end, s| {
                *max_end = max_end.max(segment_end_secs(s, rep.timescale));
                Some(*max_end)
            })
            .collect();
        Self {
            timescale: rep.timescale,
            segments,
            max_end_through,
        }
    }

    fn candidates(&self, start: f64, end: f64) -> &[&'a SegmentRef] {
        let lo = self
            .max_end_through
            .partition_point(|max_end| *max_end < start);
        let hi = self
            .segments
            .partition_point(|s| s.presentation_start_secs <= end);
        self.segments.get(lo..hi).unwrap_or_default()
    }

    fn match_video(&self, video: &SegmentRef, video_timescale: u32) -> Option<&'a SegmentRef> {
        let v_start = video.presentation_start_secs;
        let v_dur = segment_duration_secs(video, video_timescale);
        let v_end = v_start + v_dur;
        let v_mid = v_start + v_dur * 0.5;
        let edge_slack = 0.080;

        if let Some((seg, overlap)) = self
            .candidates(v_start, v_end)
            .iter()
            .copied()
            .filter_map(|seg| {
                let overlap = v_end.min(segment_end_secs(seg, self.timescale))
                    - v_start.max(seg.presentation_start_secs);
                (overlap > 0.0).then_some((seg, overlap))
            })
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
        {
            if overlap >= (v_dur * 0.20).min(0.400) {
                return Some(seg);
            }
        }

        if let Some(seg) = self
            .candidates(v_mid - edge_slack, v_mid + edge_slack)
            .iter()
            .copied()
            .find(|seg| {
                v_mid >= seg.presentation_start_secs - edge_slack
                    && v_mid <= segment_end_secs(seg, self.timescale) + edge_slack
            })
        {
            return Some(seg);
        }

        let next = self
            .segments
            .partition_point(|s| s.presentation_start_secs < v_start);
        let best = [
            next.checked_sub(1).and_then(|i| self.segments.get(i)),
            self.segments.get(next),
        ]
        .into_iter()
        .flatten()
        .copied()
        .min_by(|a, b| {
            let da = (a.presentation_start_secs - v_start).abs();
            let db = (b.presentation_start_secs - v_start).abs();
            da.total_cmp(&db)
        })?;
        let best_delta = (best.presentation_start_secs - v_start).abs();
        let a_dur = segment_duration_secs(best, self.timescale);
        let tolerance = (v_dur.max(a_dur) * 0.60).max(0.350);
        (best_delta <= tolerance).then_some(best)
    }
}

fn is_time_template(rep: &Representation) -> bool {
    rep.time_addressed
}

fn segment_end_secs(seg: &SegmentRef, timescale: u32) -> f64 {
    seg.presentation_start_secs + segment_duration_secs(seg, timescale)
}

fn segment_duration_secs(seg: &SegmentRef, timescale: u32) -> f64 {
    seg.duration_ts as f64 / timescale.max(1) as f64
}

fn aggregate_audio_ready(
    audio_required: bool,
    audio_dur_ts: u64,
    audio_timescale: u32,
    video_dur_ts: u64,
    video_timescale: u32,
) -> bool {
    if !audio_required || video_dur_ts == 0 {
        return true;
    }
    if audio_dur_ts == 0 {
        return false;
    }
    let audio_in_video_ts =
        audio_dur_ts as u128 * video_timescale.max(1) as u128 / audio_timescale.max(1) as u128;
    audio_in_video_ts * MIN_MEDIA_COVERAGE_DEN as u128
        >= video_dur_ts as u128 * MIN_MEDIA_COVERAGE_NUM as u128
}

fn aggregate_duration_ms(duration_ts: u64, timescale: u32) -> u64 {
    (duration_ts as u128 * 1_000 / timescale.max(1) as u128) as u64
}

#[allow(clippy::too_many_arguments)]
fn flush_mpd_aggregate(
    task: &Arc<Task>,
    muxer: &mut TsMuxer,
    video_timescale: u32,
    acc_vaus: &mut Vec<AccessUnit>,
    acc_aaus: &mut Vec<AudioUnit>,
    acc_dur_ts: &mut u64,
    agg_audio_dur_ts: &mut u64,
    subtitle_acc: &mut SubtitleAccumulator,
    pending_discontinuity: &mut bool,
) -> crate::Result<()> {
    let Some(segment_pts) = muxer.segment_pts_90k(acc_vaus) else {
        return Ok(());
    };
    let ts_bytes = muxer.mux_segment(acc_vaus, acc_aaus);
    let dur = *acc_dur_ts as f64 / video_timescale.max(1) as f64;
    let subtitle_body = subtitle_acc.take_body();
    let discontinuity = *pending_discontinuity;
    let subtitle =
        task.subtitles_enabled
            .then_some((task.subtitles.as_ref(), subtitle_body, segment_pts));
    task.hls
        .push_segment_pair(dur, bytes::Bytes::from(ts_bytes), discontinuity, subtitle)?;
    *pending_discontinuity = false;
    task.segments_done.fetch_add(1, Ordering::Relaxed);
    acc_vaus.clear();
    acc_aaus.clear();
    *acc_dur_ts = 0;
    *agg_audio_dur_ts = 0;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn mark_mpd_epoch_boundary(
    task: &Arc<Task>,
    muxer: &mut TsMuxer,
    video_timescale: u32,
    acc_vaus: &mut Vec<AccessUnit>,
    acc_aaus: &mut Vec<AudioUnit>,
    acc_dur_ts: &mut u64,
    agg_audio_dur_ts: &mut u64,
    subtitle_acc: &mut SubtitleAccumulator,
    pending_discontinuity: &mut bool,
    audio_required: bool,
) -> crate::Result<()> {
    if aggregate_audio_ready(
        audio_required,
        *agg_audio_dur_ts,
        muxer.audio_timescale,
        *acc_dur_ts,
        video_timescale,
    ) {
        flush_mpd_aggregate(
            task,
            muxer,
            video_timescale,
            acc_vaus,
            acc_aaus,
            acc_dur_ts,
            agg_audio_dur_ts,
            subtitle_acc,
            pending_discontinuity,
        )?;
    } else if !acc_vaus.is_empty() {
        tracing::warn!(
            task = %task.id,
            "dropping incomplete aggregate at MPD discontinuity boundary"
        );
        acc_vaus.clear();
        acc_aaus.clear();
        subtitle_acc.clear();
        *acc_dur_ts = 0;
        *agg_audio_dur_ts = 0;
    }
    *pending_discontinuity = true;
    task.origin_v_tfdt.store(u64::MAX, Ordering::Relaxed);
    task.origin_a_tfdt.store(u64::MAX, Ordering::Relaxed);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drop_mpd_live_gap_boundary(
    task: &Arc<Task>,
    video_timescale: u32,
    acc_vaus: &mut Vec<AccessUnit>,
    acc_aaus: &mut Vec<AudioUnit>,
    acc_dur_ts: &mut u64,
    agg_audio_dur_ts: &mut u64,
    subtitle_acc: &mut SubtitleAccumulator,
    pending_discontinuity: &mut bool,
) {
    if !acc_vaus.is_empty() {
        tracing::warn!(
            task = %task.id,
            dropped_ms = aggregate_duration_ms(*acc_dur_ts, video_timescale),
            "dropping partial live aggregate before resyncing to newer MPD range"
        );
        acc_vaus.clear();
        acc_aaus.clear();
        subtitle_acc.clear();
        *acc_dur_ts = 0;
        *agg_audio_dur_ts = 0;
    }
    *pending_discontinuity = true;
    task.origin_v_tfdt.store(u64::MAX, Ordering::Relaxed);
    task.origin_a_tfdt.store(u64::MAX, Ordering::Relaxed);
}

fn mpd_tfdt_rewound(last: Option<u64>, current: Option<u64>) -> bool {
    matches!((last, current), (Some(prev), Some(now)) if now < prev)
}

fn key_changed(previous: Option<&String>, current: Option<&String>) -> bool {
    matches!((previous, current), (Some(prev), Some(curr)) if prev != curr)
}

fn representation_segment_duration_secs(rep: &Representation) -> f64 {
    rep.segments
        .last()
        .or_else(|| rep.segments.first())
        .map(|s| s.duration_ts as f64 / rep.timescale.max(1) as f64)
        .unwrap_or(0.0)
}

fn initial_next_number(
    video: &Representation,
    is_live: bool,
    resume: u64,
    live_backfill_segments: u64,
) -> Option<u64> {
    let first_num = video.segments.first()?.number;
    if resume > first_num {
        return Some(resume);
    }
    if !is_live {
        return Some(first_num);
    }

    // Segment number may be an absolute $Time$ timestamp, so "last - N" is not a
    // valid way to move back N segments. Pick by index from the current live window.
    let backfill = usize::try_from(live_backfill_segments).unwrap_or(usize::MAX);
    let idx = video.segments.len().saturating_sub(backfill.max(1));
    video
        .segments
        .get(idx)
        .map(|s| s.number)
        .or(Some(first_num))
}

/// 判定 VideoRange：DV→PQ；否则从 SPS VUI transfer_characteristics 推断。
/// 覆盖 SDR / HDR10(PQ) / HLG / DV5 / DV8。
fn detect_video_range(codec: VideoCodec, is_dv: bool, params: &ParamSets) -> VideoRange {
    if is_dv {
        return VideoRange::Pq; // DV5/DV8 都以 PQ 承载
    }
    // 从首个 SPS 解析 VUI
    let sps = params.sps.first();
    if let Some(sps) = sps {
        let r = match codec {
            VideoCodec::Hevc => crate::hevc::sps::hevc_sps_range(sps),
            VideoCodec::H264 => crate::hevc::sps::h264_sps_range(sps),
        };
        if let Some(r) = r {
            return r;
        }
    }
    VideoRange::Sdr
}

/// 下载视频首段，扫描最小 cts offset（源 timescale，有符号），用于 B 帧 composition 平移。
async fn scan_min_cts(
    dl: &crate::fetch::SharedDownloader,
    video: &Representation,
    track: DashMediaTrack,
    key_mode: super::KeyMode,
) -> crate::Result<Option<i64>> {
    let Some(first) = video.segments.first() else {
        return Ok(None);
    };
    let Ok(data) = dl.get(&first.url).await else {
        return Ok(None);
    };
    if key_mode.is_clear() {
        ensure_clear_fragment(&data, "DASH video")?;
    }
    Ok(track
        .parse(&data)
        .and_then(|parsed| parsed.samples.iter().map(|s| s.cts_offset as i64).min()))
}

#[cfg(test)]
mod clear_mode_tests {
    use super::*;

    #[test]
    fn clear_mpd_checks_selected_tracks_even_without_a_manifest_kid() {
        let manifest = r#"<MPD type="static">
          <Period>
            <AdaptationSet contentType="video">
              <SegmentTemplate initialization="v-init.mp4" media="v-$Number$.m4s">
                <SegmentTimeline><S t="0" d="2"/></SegmentTimeline>
              </SegmentTemplate>
              <Representation id="v" codecs="avc1" bandwidth="1"/>
            </AdaptationSet>
            <AdaptationSet contentType="audio">
              <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"/>
              <SegmentTemplate initialization="a-init.mp4" media="a-$Number$.m4s">
                <SegmentTimeline><S t="0" d="2"/></SegmentTimeline>
              </SegmentTemplate>
              <Representation id="a" codecs="mp4a.40.2" bandwidth="1"/>
            </AdaptationSet>
          </Period>
        </MPD>"#;
        let mpd = parse_mpd(manifest, "https://example.invalid/manifest.mpd").unwrap();
        let [video, audio] = mpd.representations.as_slice() else {
            panic!("expected a video and an audio track");
        };
        assert!(audio.manifest_kids.is_empty());
        assert!(ensure_clear_mpd_manifest(video, None, None).is_ok());
        let err = ensure_clear_mpd_manifest(video, Some(audio), None).unwrap_err();
        assert!(err.is::<ProtectedInput>());
    }

    #[test]
    fn mpd_duration_is_rescaled_to_track_media_ticks() {
        assert_eq!(
            rescale_duration(2_000_000, 1_000_000, 12_800).unwrap(),
            25_600
        );
        assert_eq!(
            rescale_duration(1_500_000, 1_000_000, 48_000).unwrap(),
            72_000
        );
        assert!(rescale_duration(u64::MAX, 1, 1000).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{SourceKind, TrackSelection};

    fn synthetic_segment(base_media_decode_time: u64, frames: &[(u64, u32, i32)]) -> ParsedSegment {
        ParsedSegment {
            base_media_decode_time: Some(base_media_decode_time),
            mdat: Vec::new(),
            decode_time_offsets: frames.iter().map(|&(dts, _, _)| dts).collect(),
            samples: frames
                .iter()
                .map(
                    |&(_, duration, cts_offset)| crate::mp4::sample::SampleInfo {
                        data_range: (0, 0),
                        duration,
                        cts_offset,
                        iv: Vec::new(),
                        subsamples: Vec::new(),
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn aggregate_payload_budget_is_bounded_and_checked() {
        assert!(payload_fits_budget([1, 2, 3].into_iter(), 6));
        assert!(!payload_fits_budget([1, 2, 3].into_iter(), 5));
        assert!(!payload_fits_budget([2, usize::MAX].into_iter(), 10));
    }

    #[test]
    fn period_retains_only_whole_access_units_and_measures_their_extent() {
        let frames = [(0, 1000, 0), (1000, 1000, 0), (2000, 1000, 0)];
        let segment = synthetic_segment(500, &frames);
        let window = SampleWindow {
            start: 1500,
            end: Some(2500),
            initial_edit_without_pto: false,
        };
        let kept = window.video_extent(&segment, 23).unwrap();
        assert_eq!(kept.samples, 1..2);
        assert_eq!(kept.presentation_span, 1000);
        assert_eq!(kept.decode_span, 1000);
        assert_eq!(
            window.audio_samples(&segment, 23, AudioCodec::Ac3).unwrap(),
            1..2
        );
    }

    #[test]
    fn period_rejects_a_partial_video_boundary_instead_of_overrunning() {
        let frames = [(0, 1000, 0), (1000, 1000, 0), (2000, 1000, 0)];
        let segment = synthetic_segment(0, &frames);
        let window = SampleWindow {
            start: 1000,
            end: Some(2500),
            initial_edit_without_pto: false,
        };
        assert!(window
            .video_extent(&segment, 23)
            .err()
            .unwrap()
            .to_string()
            .contains("within an access unit"));
        assert_eq!(
            window.audio_samples(&segment, 23, AudioCodec::Ec3).unwrap(),
            1..2
        );
    }

    #[test]
    fn period_cannot_remove_a_reordered_reference_from_the_middle() {
        let segment =
            synthetic_segment(0, &[(0, 1000, 0), (1000, 1000, 1000), (2000, 1000, -1000)]);
        let window = SampleWindow {
            start: 0,
            end: Some(2000),
            initial_edit_without_pto: false,
        };
        assert!(window
            .video_extent(&segment, 23)
            .err()
            .unwrap()
            .to_string()
            .contains("non-contiguous"));
    }

    #[test]
    fn dash_ac3_and_ec3_keep_the_first_whole_au_across_a_small_encoder_edit() {
        let mut rep = audio_rep(48_000, Vec::new());
        rep.period_duration = Some(Duration::from_millis(96));
        let enc_edit = SampleWindow::from_representation(&rep, 48_000, 256).unwrap();
        let frames = [
            (0, 1536, 0),
            (1536, 1536, 0),
            (3072, 1536, 0),
            (4608, 1536, 0),
        ];
        let audio = synthetic_segment(0, &frames);
        for codec in [AudioCodec::Ac3, AudioCodec::Ec3] {
            assert_eq!(enc_edit.audio_samples(&audio, 1, codec).unwrap(), 0..3);
            assert!(enc_edit
                .audio_samples(&synthetic_segment(128, &frames), 1, codec)
                .is_err());
        }
        assert!(enc_edit.audio_samples(&audio, 1, AudioCodec::Ac4).is_err());
        // Same musical cut imposed by the MPD instead of an initial edit
        // still requires a real partial trim; do not silently change it.
        rep.presentation_time_offset = 256;
        let pto_cut = SampleWindow::from_representation(&rep, 48_000, 0).unwrap();
        assert!(pto_cut.audio_samples(&audio, 1, AudioCodec::Ac3).is_err());
    }

    fn atom(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        [
            &((8 + payload.len()) as u32).to_be_bytes()[..],
            typ,
            payload,
        ]
        .concat()
    }

    fn mock_dash_trak(id: u32, handler: &[u8; 4], scale: u32, edit: i32, marker: u8) -> Vec<u8> {
        let tkhd = atom(b"tkhd", &[&[0; 12], &id.to_be_bytes()[..]].concat());
        let mdhd = atom(b"mdhd", &[&[0; 12], &scale.to_be_bytes()[..]].concat());
        let hdlr = atom(b"hdlr", &[&[0; 8], &handler[..]].concat());
        let avcc = atom(b"avcC", &[1, 0x42, 0, 0, 0xff, 0xe1, 0, 2, 0x67, marker, 0]);
        let mdia = atom(b"mdia", &[mdhd, hdlr, avcc].concat());
        let elst = atom(
            b"elst",
            &[
                &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0],
                &edit.to_be_bytes()[..],
                &[0, 1, 0, 0],
            ]
            .concat(),
        );
        let mut tenc = [marker; 24];
        tenc[..8].copy_from_slice(&[0, 0, 0, 0, 0, 0, 1, 8]);
        atom(
            b"trak",
            &[tkhd, mdia, atom(b"edts", &elst), atom(b"tenc", &tenc)].concat(),
        )
    }

    #[test]
    fn dash_video_init_and_fragment_read_only_the_selected_video_track() {
        let audio = mock_dash_trak(7, b"soun", 48_000, 256, 0x11);
        let video = mock_dash_trak(3, b"vide", 12_800, 512, 0x22);
        let moov = atom(b"moov", &[audio, video].concat());
        let (init, encryption) =
            read_dash_video_init(&moov, VideoCodec::H264, super::super::KeyMode::Static).unwrap();
        assert_eq!(init.media.id, 3);
        assert_eq!(init.media.timescale, 12_800);
        assert_eq!(init.media.media_offset, 512);
        assert_eq!(init.params.sps, vec![vec![0x67, 0x22]]);
        assert_eq!(encryption.unwrap().kid, "22".repeat(16));

        let traf = |id: u32, dur: u32, data_offset: i32| {
            let tfhd = atom(b"tfhd", &[&[0, 2, 0, 0], &id.to_be_bytes()[..]].concat());
            let trun = atom(
                b"trun",
                &[
                    &[0, 0, 3, 1, 0, 0, 0, 1],
                    &data_offset.to_be_bytes()[..],
                    &dur.to_be_bytes()[..],
                    &[0, 0, 0, 1],
                ]
                .concat(),
            );
            atom(b"traf", &[tfhd, trun].concat())
        };
        let moof = |base| {
            atom(
                b"moof",
                &[traf(7, 1536, base), traf(3, 512, base + 1)].concat(),
            )
        };
        let fragment = [moof(moof(0).len() as i32 + 8), atom(b"mdat", b"av")].concat();
        let parsed = init.media.parse(&fragment).unwrap();
        assert_eq!(parsed.samples[0].duration, 512);
        assert_eq!(parsed.mdat, b"v");
    }

    fn seg(number: u64, duration_ts: u64, presentation_start_secs: f64) -> SegmentRef {
        SegmentRef {
            number,
            url: format!("seg-{number}.m4s"),
            duration_ts,
            presentation_start_secs,
        }
    }

    fn timed_seg(number: u64, duration_ts: u64, timescale: u32) -> SegmentRef {
        seg(number, duration_ts, number as f64 / f64::from(timescale))
    }

    fn timed_segments(count: u64, duration_ts: u64, timescale: u32) -> Vec<SegmentRef> {
        (0..count)
            .map(|i| timed_seg(i * duration_ts, duration_ts, timescale))
            .collect()
    }

    fn video_rep(timescale: u32, segments: Vec<SegmentRef>) -> Representation {
        Representation {
            id: "v".into(),
            kind: TrackKind::Video,
            codecs: "hvc1".into(),
            bandwidth: 1,
            width: 1920,
            height: 1080,
            lang: String::new(),
            timescale,
            init_url: String::new(),
            manifest_kids: Vec::new(),
            has_content_protection: false,
            media_template: "v-$Time$.m4s".into(),
            start_number: 1,
            segments,
            total_duration_ts: 0,
            time_addressed: true,
            presentation_time_offset: 0,
            period_duration: None,
        }
    }

    fn audio_rep(timescale: u32, segments: Vec<SegmentRef>) -> Representation {
        Representation {
            id: "a".into(),
            kind: TrackKind::Audio,
            codecs: "ec-3".into(),
            bandwidth: 1,
            width: 0,
            height: 0,
            lang: "de".into(),
            timescale,
            init_url: String::new(),
            manifest_kids: Vec::new(),
            has_content_protection: false,
            media_template: "a-$Time$.m4s".into(),
            start_number: 1,
            segments,
            total_duration_ts: 0,
            time_addressed: true,
            presentation_time_offset: 0,
            period_duration: None,
        }
    }

    #[test]
    fn aggregate_audio_ready_rejects_short_audio() {
        assert!(aggregate_audio_ready(true, 460_800, 48_000, 5_760, 600));
        assert!(!aggregate_audio_ready(true, 368_640, 48_000, 5_760, 600));
        assert!(aggregate_audio_ready(false, 0, 48_000, 5_760, 600));
    }

    #[test]
    fn time_template_audio_match_uses_seconds_not_index() {
        let video = timed_seg(1_069_949_867_522, 1_152, 600);
        let audio = audio_rep(
            48_000,
            vec![
                timed_seg(85_595_989_401_792 - 92_160, 92_160, 48_000),
                timed_seg(85_595_989_401_792, 92_160, 48_000),
                timed_seg(85_595_989_401_792 + 92_160, 92_160, 48_000),
            ],
        );
        let matched = TimedTrack::new(&audio).match_video(&video, 600).unwrap();
        assert_eq!(matched.number, 85_595_989_401_792);
    }

    #[test]
    fn time_template_audio_match_uses_interval_overlap_for_variable_audio() {
        let video = timed_seg(17_833_422_517_292_555, 20_020_000, 10_000_000);
        let audio = audio_rep(
            10_000_000,
            vec![
                timed_seg(17_833_422_505_215_777, 23_466_667, 10_000_000),
                timed_seg(17_833_422_528_682_444, 9_386_667, 10_000_000),
            ],
        );
        let matched = TimedTrack::new(&audio)
            .match_video(&video, 10_000_000)
            .unwrap();
        assert_eq!(matched.number, 17_833_422_505_215_777);
    }

    #[test]
    fn time_template_audio_match_prefers_best_overlap_over_midpoint() {
        let video = timed_seg(17_833_435_129_892_555, 20_020_000, 10_000_000);
        let audio = audio_rep(
            10_000_000,
            vec![
                timed_seg(17_833_435_117_389_111, 22_400_000, 10_000_000),
                timed_seg(17_833_435_139_789_111, 11_200_000, 10_000_000),
            ],
        );
        let matched = TimedTrack::new(&audio)
            .match_video(&video, 10_000_000)
            .unwrap();
        assert_eq!(matched.number, 17_833_435_139_789_111);
    }

    #[test]
    fn short_source_live_jobs_publish_only_complete_telus_style_groups() {
        let dur = 20_020_000;
        let video = video_rep(10_000_000, timed_segments(6, dur, 10_000_000));
        let audio = audio_rep(10_000_000, timed_segments(4, dur, 10_000_000));

        let group_size = short_source_group_size(&video).unwrap();
        let plan = build_short_source_live_jobs(&video, Some(&audio), None, true, 0, group_size);

        assert_eq!(group_size, 4);
        assert_eq!(plan.jobs.len(), 4);
        assert_eq!(
            plan.jobs.iter().map(|j| j.num).collect::<Vec<_>>(),
            vec![0, dur, dur * 2, dur * 3]
        );
        assert!(plan
            .jobs
            .iter()
            .all(|j| j.skip_to_on_failure == Some(dur * 3 + 1)));
        assert!(plan.blocked_on_audio.is_none());
        assert!(!plan.skipped_incomplete_head);
    }

    #[test]
    fn short_source_live_jobs_hold_tail_until_audio_group_is_complete() {
        let dur = 20_020_000;
        let video = video_rep(10_000_000, timed_segments(4, dur, 10_000_000));
        let audio = audio_rep(10_000_000, timed_segments(2, dur, 10_000_000));

        let plan = build_short_source_live_jobs(&video, Some(&audio), None, true, 0, 4);

        assert!(plan.jobs.is_empty());
        assert_eq!(plan.blocked_on_audio.as_ref().map(|b| b.seg), Some(0));
        assert_eq!(
            plan.blocked_on_audio.as_ref().map(|b| b.skip_to),
            Some(dur * 3 + 1)
        );
        assert!(!plan.skipped_incomplete_head);
    }

    #[test]
    fn short_source_live_jobs_skip_incomplete_head_before_complete_group() {
        let dur = 20_020_000;
        let video = video_rep(10_000_000, timed_segments(8, dur, 10_000_000));
        let audio = audio_rep(10_000_000, timed_segments(8, dur, 10_000_000));

        let plan = build_short_source_live_jobs(&video, Some(&audio), None, true, dur, 4);

        assert_eq!(plan.jobs.len(), 4);
        assert_eq!(
            plan.jobs.iter().map(|j| j.num).collect::<Vec<_>>(),
            vec![dur * 4, dur * 5, dur * 6, dur * 7]
        );
        assert!(plan
            .jobs
            .iter()
            .all(|j| j.skip_to_on_failure == Some(dur * 7 + 1)));
        assert!(plan.skipped_incomplete_head);
        assert!(plan.blocked_on_audio.is_none());
    }

    #[test]
    fn short_source_number_template_coverage_uses_presentation_time_not_raw_number() {
        let start = 464_619_009;
        let mut video = video_rep(200, vec![seg(start, 768, 0.0), seg(start + 1, 768, 3.84)]);
        video.media_template = "v-$Number$.m4s".into();
        video.start_number = start;
        let mut audio = audio_rep(
            48_000,
            vec![seg(start, 184_320, 0.0), seg(start + 1, 184_320, 3.84)],
        );
        audio.media_template = "a-$Number$.m4s".into();
        audio.start_number = start;

        let group = video.segments.iter().collect::<Vec<_>>();
        let matched =
            short_source_group_audio_matches(&video, &group, &TimedTrack::new(&audio)).unwrap();

        assert_eq!(
            matched.iter().map(|s| s.number).collect::<Vec<_>>(),
            vec![start, start + 1]
        );
    }

    #[test]
    fn number_template_audio_history_does_not_shift_playback_timeline() {
        let mpd = parse_mpd(
            r#"<MPD type="dynamic">
                <Period start="PT20S">
                    <AdaptationSet contentType="video">
                        <SegmentTemplate timescale="1000" startNumber="100"
                            presentationTimeOffset="8000" initialization="v-init.mp4" media="v-$Number$.m4s">
                            <SegmentTimeline><S t="10000" d="2000" r="3" /></SegmentTimeline>
                        </SegmentTemplate>
                        <Representation id="v" codecs="avc1" />
                    </AdaptationSet>
                    <AdaptationSet contentType="audio">
                        <SegmentTemplate timescale="48000" startNumber="99"
                            presentationTimeOffset="384000" initialization="a-init.mp4" media="a-$Number$.m4s">
                            <SegmentTimeline><S t="384000" d="96000" r="4" /></SegmentTimeline>
                        </SegmentTemplate>
                        <Representation id="a" codecs="mp4a.40.2" />
                    </AdaptationSet>
                    <AdaptationSet contentType="text">
                        <SegmentTemplate timescale="10" startNumber="301"
                            presentationTimeOffset="20" initialization="s-init.mp4" media="s-$Number$.m4s">
                            <SegmentTimeline><S t="40" d="20" r="3" /></SegmentTimeline>
                        </SegmentTemplate>
                        <Representation id="s" codecs="wvtt" />
                    </AdaptationSet>
                </Period>
            </MPD>"#,
            "https://example.invalid/main.mpd",
        )
        .unwrap();
        let find = |id: &str| mpd.representations.iter().find(|r| r.id == id).unwrap();
        let plan =
            build_short_source_live_jobs(find("v"), Some(find("a")), Some(find("s")), true, 100, 4);
        assert_eq!(plan.jobs.len(), 4);
        assert!(plan.blocked_on_audio.is_none());
        for (i, job) in plan.jobs.iter().enumerate() {
            assert_eq!(job.num, 100 + i as u64);
            assert_eq!(
                job.aurl,
                Some(format!("https://example.invalid/a-{}.m4s", 100 + i))
            );
            assert_eq!(
                job.surl,
                Some(format!("https://example.invalid/s-{}.m4s", 301 + i))
            );
        }
    }

    #[test]
    fn live_initial_backfill_uses_segment_count_for_time_template_numbers() {
        let video = Representation {
            id: "v".into(),
            kind: TrackKind::Video,
            codecs: "hvc1".into(),
            bandwidth: 1,
            width: 1,
            height: 1,
            lang: String::new(),
            timescale: 10_000_000,
            init_url: String::new(),
            manifest_kids: Vec::new(),
            has_content_protection: false,
            media_template: "v-$Time$.m4s".into(),
            start_number: 1,
            segments: vec![
                timed_seg(17832777776145666, 20_020_000, 10_000_000),
                timed_seg(17832777796165666, 20_020_000, 10_000_000),
                timed_seg(17832777816185666, 20_020_000, 10_000_000),
                timed_seg(17832777836205666, 20_020_000, 10_000_000),
                timed_seg(17832777856225666, 20_020_000, 10_000_000),
            ],
            total_duration_ts: 0,
            time_addressed: true,
            presentation_time_offset: 0,
            period_duration: None,
        };

        assert_eq!(
            initial_next_number(&video, true, 0, 3),
            Some(17832777816185666)
        );
    }

    #[test]
    fn trims_overlapping_audio_frames_before_aggregate_append() {
        let task = Arc::new(Task {
            id: "test".into(),
            name: "test".into(),
            run_mode: crate::task::RunMode::Always,
            mpd_url: String::new(),
            video_rep_id: String::new(),
            audio_rep_id: Some("a".into()),
            subtitles_enabled: false,
            subtitle_rep_id: None,
            subtitle_lang: String::new(),
            source_kind: SourceKind::Mpd,
            codecs: String::new(),
            width: 0,
            height: 0,
            bandwidth: 0,
            is_dynamic: true,
            state: std::sync::atomic::AtomicU8::new(0),
            error_msg: crate::task::manager::parking_lot_lite::Mutex::new(String::new()),
            stage: crate::task::manager::parking_lot_lite::Mutex::new(String::new()),
            segments_done: std::sync::atomic::AtomicU64::new(0),
            bytes_done: std::sync::atomic::AtomicU64::new(0),
            total_segments: std::sync::atomic::AtomicU64::new(0),
            resume_number: std::sync::atomic::AtomicU64::new(0),
            acc_v_dts: std::sync::atomic::AtomicU64::new(0),
            acc_a_dts: std::sync::atomic::AtomicU64::new(0),
            origin_v_tfdt: std::sync::atomic::AtomicU64::new(u64::MAX),
            origin_a_tfdt: std::sync::atomic::AtomicU64::new(u64::MAX),
            hls: Arc::new(crate::segment::HlsOutput::new(
                6,
                7,
                String::new(),
                "SDR".into(),
                true,
                "/p/test".into(),
                1,
            )),
            subtitles: Arc::new(crate::segment::SubtitleOutput::new(6, 7, true, 1)),
            fetch_tuning: crate::task::fetch_tuning::AdaptiveFetchTuning::new(),
            cancel: crate::task::manager::parking_lot_lite::Mutex::new(
                tokio_util::sync::CancellationToken::new(),
            ),
            paused: std::sync::atomic::AtomicBool::new(false),
            last_playback_request_secs: std::sync::atomic::AtomicU64::new(0),
            idle_timeout_secs: crate::task::manager::on_demand_idle_timeout_secs(),
            on_demand_auto_paused: std::sync::atomic::AtomicBool::new(false),
            sel: TrackSelection {
                mpd_url: String::new(),
                source_kind: SourceKind::Mpd,
                keys: String::new(),
                key_mode: crate::task::KeyMode::Static,
                video_rep_id: String::new(),
                audio_rep_id: Some("a".into()),
                enable_subtitles: false,
                subtitle_rep_id: None,
            },
        });
        let acc = vec![AudioUnit {
            data: Vec::new(),
            pts: 1_000,
        }];
        let mut next = vec![
            AudioUnit {
                data: Vec::new(),
                pts: 1_064,
            },
            AudioUnit {
                data: Vec::new(),
                pts: 1_096,
            },
            AudioUnit {
                data: Vec::new(),
                pts: 1_128,
            },
        ];
        let mut durations = vec![32, 32, 32];

        let kept = trim_or_close_audio_gap(&task, 42, &acc, 96, &mut next, &mut durations, 1000);

        assert_eq!(
            next.iter().map(|a| a.pts).collect::<Vec<_>>(),
            vec![1_096, 1_128]
        );
        assert_eq!(kept, 64);
    }

    #[test]
    fn closes_small_audio_gap_before_aggregate_append() {
        let task = Arc::new(Task {
            id: "test".into(),
            name: "test".into(),
            run_mode: crate::task::RunMode::Always,
            mpd_url: String::new(),
            video_rep_id: String::new(),
            audio_rep_id: Some("a".into()),
            subtitles_enabled: false,
            subtitle_rep_id: None,
            subtitle_lang: String::new(),
            source_kind: SourceKind::Mpd,
            codecs: String::new(),
            width: 0,
            height: 0,
            bandwidth: 0,
            is_dynamic: true,
            state: std::sync::atomic::AtomicU8::new(0),
            error_msg: crate::task::manager::parking_lot_lite::Mutex::new(String::new()),
            stage: crate::task::manager::parking_lot_lite::Mutex::new(String::new()),
            segments_done: std::sync::atomic::AtomicU64::new(0),
            bytes_done: std::sync::atomic::AtomicU64::new(0),
            total_segments: std::sync::atomic::AtomicU64::new(0),
            resume_number: std::sync::atomic::AtomicU64::new(0),
            acc_v_dts: std::sync::atomic::AtomicU64::new(0),
            acc_a_dts: std::sync::atomic::AtomicU64::new(0),
            origin_v_tfdt: std::sync::atomic::AtomicU64::new(u64::MAX),
            origin_a_tfdt: std::sync::atomic::AtomicU64::new(u64::MAX),
            hls: Arc::new(crate::segment::HlsOutput::new(
                6,
                7,
                String::new(),
                "SDR".into(),
                true,
                "/p/test".into(),
                1,
            )),
            subtitles: Arc::new(crate::segment::SubtitleOutput::new(6, 7, true, 1)),
            fetch_tuning: crate::task::fetch_tuning::AdaptiveFetchTuning::new(),
            cancel: crate::task::manager::parking_lot_lite::Mutex::new(
                tokio_util::sync::CancellationToken::new(),
            ),
            paused: std::sync::atomic::AtomicBool::new(false),
            last_playback_request_secs: std::sync::atomic::AtomicU64::new(0),
            idle_timeout_secs: crate::task::manager::on_demand_idle_timeout_secs(),
            on_demand_auto_paused: std::sync::atomic::AtomicBool::new(false),
            sel: TrackSelection {
                mpd_url: String::new(),
                source_kind: SourceKind::Mpd,
                keys: String::new(),
                key_mode: crate::task::KeyMode::Static,
                video_rep_id: String::new(),
                audio_rep_id: Some("a".into()),
                enable_subtitles: false,
                subtitle_rep_id: None,
            },
        });
        let acc = vec![AudioUnit {
            data: Vec::new(),
            pts: 1_000,
        }];
        let mut next = vec![AudioUnit {
            data: Vec::new(),
            pts: 1_100,
        }];
        let mut durations = vec![32];

        let kept = trim_or_close_audio_gap(&task, 43, &acc, 96, &mut next, &mut durations, 1000);

        assert_eq!(next[0].pts, 1_096);
        assert_eq!(kept, 32);
    }
}
