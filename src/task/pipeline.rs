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
use crate::mp4::sample::parse_media_segment;
use crate::mp4::{DoviConfig, ParamSets, TrackEncryption};
use crate::mpd::parser::SegmentRef;
use crate::mpd::{parse_mpd, Representation, TrackKind};
use crate::subtitle::SubtitleAccumulator;
use crate::ts::muxer::{AudioUnit, TsMuxer};
use crate::ts::{AudioCodec, VideoCodec, VideoRange};

use super::{key_resolver, live_tuning, manager::Task};

const SHORT_SEG_THRESHOLD_S: f64 = live_tuning::SHORT_SEG_THRESHOLD_S;
const AGGREGATE_TARGET_S: f64 = live_tuning::AGGREGATE_TARGET_S;
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
pub async fn run_pipeline(task: Arc<Task>, dl: crate::fetch::SharedDownloader) {
    let cancel = task.cancel_token();
    task.set_paused(false);
    task.set_state(super::TaskState::Running);
    let sel = task.sel.clone();
    if let Err(e) = run_inner(&task, &dl, &sel).await {
        // 取消导致的错误不算 Error
        if cancel.is_cancelled() {
            finalize_cancelled(&task);
        } else {
            *task.error_msg.lock() = format!("{e:#}");
            task.set_stage("出错");
            task.set_state(super::TaskState::Error);
            tracing::error!(task = %task.id, "pipeline error: {e:#}");
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

/// 被取消后根据 paused 标志决定落到 暂停 还是 停止 状态。
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
    // 1. 拉取并解析 MPD
    task.set_stage("解析 MPD");
    let mpd_text = dl.get_text(&sel.mpd_url).await?;
    let mpd = parse_mpd(&mpd_text, &sel.mpd_url)?;

    let video = mpd
        .representations
        .iter()
        .find(|r| r.id == sel.video_rep_id && r.kind == TrackKind::Video)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("video rep {} not found", sel.video_rep_id))?;

    let audio = match &sel.audio_rep_id {
        Some(aid) => mpd
            .representations
            .iter()
            .find(|r| &r.id == aid && r.kind == TrackKind::Audio)
            .cloned(),
        None => None,
    };
    let subtitle = match &sel.subtitle_rep_id {
        Some(sid) if sel.enable_subtitles => mpd
            .representations
            .iter()
            .find(|r| &r.id == sid && r.kind == TrackKind::Subtitle)
            .cloned(),
        _ => None,
    };
    let source_segment_secs = representation_segment_duration_secs(&video);
    task.configure_fetch_tuning(live_tuning::is_short_segment(source_segment_secs));

    // 2. 下载 video init，按 codec 解析参数集 / dvcC / tenc(KID) / VideoRange
    task.set_stage("下载 init 段");
    let vinit = dl.get(&video.init_url).await?;
    let video_codec = VideoCodec::from_codecs(&video.codecs);
    let params = match video_codec {
        VideoCodec::Hevc => ParamSets::find_in_init(&vinit).unwrap_or_default(),
        VideoCodec::H264 => crate::mp4::avcc::find_avcc_in_init(&vinit).unwrap_or_default(),
    };
    let dovi = DoviConfig::find_in_init(&vinit);
    let audio_codec = audio.as_ref().map(|a| detect_audio_codec(&a.codecs));

    // VideoRange 判定：DV → PQ；否则从 SPS VUI transfer_characteristics 推断（HDR10=PQ/HLG/SDR）。
    let video_range = detect_video_range(video_codec, dovi.is_some(), &params);
    tracing::info!(task=%task.id, "codec={:?} dovi={:?} range={:?}", video_codec, dovi.map(|d| (d.profile, d.bl_compatibility_id)), video_range);

    let vtenc = TrackEncryption::find_in_init(&vinit);
    let vkid = effective_mpd_kid(sel.key_mode, vtenc.as_ref(), Some(&video));

    // 音频 key：下载 audio init 读其 tenc KID 匹配（含 constant IV 处理）；AAC 时提取 ADTS 配置
    let mut aac_cfg: Option<crate::mp4::aac::AacConfig> = None;
    let mut atenc: Option<TrackEncryption> = None;
    if let Some(a) = &audio {
        if let Ok(ainit) = dl.get(&a.init_url).await {
            if audio_codec == Some(AudioCodec::AacAdts) {
                aac_cfg = crate::mp4::aac::AacConfig::find_in_init(&ainit);
                tracing::info!(task=%task.id, "AAC config = {:?}", aac_cfg);
            }
            atenc = TrackEncryption::find_in_init(&ainit);
        }
    }
    let akid = effective_mpd_kid(sel.key_mode, atenc.as_ref(), audio.as_ref());
    let mut stenc: Option<TrackEncryption> = None;
    if let Some(s) = &subtitle {
        if let Ok(sinit) = dl.get(&s.init_url).await {
            stenc = TrackEncryption::find_in_init(&sinit);
        }
    }
    let skid = effective_mpd_kid(sel.key_mode, stenc.as_ref(), subtitle.as_ref());

    task.set_stage(if sel.key_mode.is_dynamic() {
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
    let mut vdec = build_mpd_video_decryptor(task, &keystore, vtenc.as_ref(), vkid.as_deref())?;

    let mut adec = build_mpd_audio_decryptor(task, &keystore, atenc.as_ref(), akid.as_deref());
    let mut sdec = build_mpd_subtitle_decryptor(task, &keystore, stenc.as_ref(), skid.as_deref());

    // 4. 计算时钟（扫描视频首段的最小 cts，用于 B 帧 composition 平移）
    let min_cts = scan_min_cts(dl, &video).await.unwrap_or(0);
    let clock = ClockState::new(min_cts, video.timescale, 90_000);

    let audio_timescale = audio.as_ref().map(|a| a.timescale).unwrap_or(48000);
    let mut muxer = TsMuxer::new(
        &params,
        video_codec,
        dovi,
        audio_codec,
        audio_timescale,
        clock,
    );

    // 5. 段处理（static: 一次处理全部；dynamic/live: 循环重拉 MPD 取增量段）
    task.set_stage("转封装中");
    let is_live = mpd.is_dynamic;
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

        // 取出 number >= next_number 的新段（视频）及其对应音频段。
        // 音频匹配策略：
        //   - $Time$ 模板：音视频 timescale 不同，不能按 number 直接相等；
        //     也不再按数组下标硬配，因为直播刷新时两个窗口可能短暂错位。
        //     改成按“段起始秒数”找最近音频段。
        //   - $Number$ 模板：按 number 精确匹配（修复直播刷新错位 bug）。
        let is_time_tpl =
            video.media_template.contains("$Time$") && !video.media_template.contains("$Number$");
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
                    video.timescale,
                    &mut agg_vaus,
                    &mut agg_aaus,
                    &mut agg_dur_ts,
                    &mut agg_audio_dur_ts,
                    &mut subtitle_acc,
                    &mut pending_discontinuity,
                    audio_required,
                );
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
                    if task.is_dynamic && audio_wait_refreshes >= AUDIO_MATCH_MAX_WAIT_REFRESHES {
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
                            video.timescale,
                            &mut agg_vaus,
                            &mut agg_aaus,
                            &mut agg_dur_ts,
                            &mut agg_audio_dur_ts,
                            &mut subtitle_acc,
                            &mut pending_discontinuity,
                            audio_required,
                        );
                        next_number = blocked.skip_to;
                        task.resume_number.store(next_number, O::Relaxed);
                        audio_wait_seg = None;
                        audio_wait_refreshes = 0;
                    }
                }
            }
        } else {
            let audio_by_number: Option<std::collections::HashMap<u64, (String, u64)>> =
                if is_time_tpl {
                    None
                } else {
                    audio.as_ref().map(|a| {
                        a.segments
                            .iter()
                            .map(|s| (s.number, (s.url.clone(), s.duration_ts)))
                            .collect()
                    })
                };

            for v in video.segments.iter().filter(|v| v.number >= next_number) {
                let audio_match: Option<(String, u64)> = if is_time_tpl {
                    audio
                        .as_ref()
                        .and_then(|a| match_audio_segment_by_time(v, video.timescale, a))
                        .map(|s| (s.url.clone(), s.duration_ts))
                } else {
                    audio_by_number
                        .as_ref()
                        .and_then(|m| m.get(&v.number))
                        .cloned()
                };
                let subtitle_match = subtitle
                    .as_ref()
                    .and_then(|s| match_timed_segment_for_video(v, video.timescale, s))
                    .map(|s| (s.url.clone(), s.duration_ts));

                if audio_required && audio_match.is_none() {
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
                    if task.is_dynamic
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
                            video.timescale,
                            &mut agg_vaus,
                            &mut agg_aaus,
                            &mut agg_dur_ts,
                            &mut agg_audio_dur_ts,
                            &mut subtitle_acc,
                            &mut pending_discontinuity,
                            audio_required,
                        );
                        next_number = v.number.saturating_add(1);
                        task.resume_number.store(next_number, O::Relaxed);
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
                });
            }
        }
        if !blocked_on_audio {
            audio_wait_seg = None;
            audio_wait_refreshes = 0;
        }

        if !new_jobs.is_empty() {
            let batch_last = new_jobs.last().map(|j| j.num).unwrap_or(next_number);
            let (av, aa, processed_until) = process_batch(
                task,
                dl,
                &vdec,
                &adec,
                &sdec,
                video_codec,
                aac_cfg,
                &mut muxer,
                video.timescale,
                subtitle.as_ref().map(|s| s.timescale).unwrap_or(1_000),
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
            )
            .await?;
            acc_v_dts = av;
            acc_a_dts = aa;
            // next_number 用「实际处理到的位置」，而非批前预算值。
            // 被取消时 process_batch 提前返回，processed_until 只到已完成的段，
            // 避免把 resume_number 错误地跳到批末尾（否则恢复时无段可处理→误判完成）。
            next_number = processed_until;
            let _ = batch_last;
            // 持久化进度到 task，供停止/暂停后重启续传
            task.resume_number.store(next_number, O::Relaxed);
            task.acc_v_dts.store(acc_v_dts, O::Relaxed);
            task.acc_a_dts.store(acc_a_dts, O::Relaxed);
        }

        if !is_live {
            break; // VOD 处理完即结束
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
            .get_text(&sel.mpd_url)
            .await
            .and_then(|t| parse_mpd(&t, &sel.mpd_url))
        {
            Ok(fresh) => {
                if let Some(v) = fresh
                    .representations
                    .iter()
                    .find(|r| r.id == sel.video_rep_id)
                    .cloned()
                {
                    video = v;
                    // 直播 total_segments 随 MPD 刷新动态变化（timeShiftBuffer 窗口滑动）
                    task.total_segments
                        .store(video.segments.len() as u64, O::Relaxed);
                }
                if let Some(aid) = &sel.audio_rep_id {
                    audio = fresh.representations.iter().find(|r| &r.id == aid).cloned();
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
                if v_init_changed || a_init_changed || s_init_changed {
                    mark_mpd_epoch_boundary(
                        task,
                        &mut muxer,
                        video.timescale,
                        &mut agg_vaus,
                        &mut agg_aaus,
                        &mut agg_dur_ts,
                        &mut agg_audio_dur_ts,
                        &mut subtitle_acc,
                        &mut pending_discontinuity,
                        audio_required,
                    );
                    current_v_init_url = video.init_url.clone();
                    current_a_init_url = audio.as_ref().map(|a| a.init_url.clone());
                    current_s_init_url = subtitle.as_ref().map(|s| s.init_url.clone());
                    last_v_tfdt = None;
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
                        &mut aac_cfg,
                    )
                    .await?;
                    if key_changed(current_v_kid.as_ref(), new_keys.video_kid.as_ref())
                        || key_changed(current_a_kid.as_ref(), new_keys.audio_kid.as_ref())
                        || key_changed(current_s_kid.as_ref(), new_keys.subtitle_kid.as_ref())
                    {
                        flush_mpd_aggregate(
                            task,
                            &mut muxer,
                            video.timescale,
                            &mut agg_vaus,
                            &mut agg_aaus,
                            &mut agg_dur_ts,
                            &mut agg_audio_dur_ts,
                            &mut subtitle_acc,
                            &mut pending_discontinuity,
                        );
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
            video.timescale,
        )
    {
        flush_mpd_aggregate(
            task,
            &mut muxer,
            video.timescale,
            &mut agg_vaus,
            &mut agg_aaus,
            &mut agg_dur_ts,
            &mut agg_audio_dur_ts,
            &mut subtitle_acc,
            &mut pending_discontinuity,
        );
    } else if !agg_vaus.is_empty() {
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
    aac_cfg: Option<crate::mp4::aac::AacConfig>,
    muxer: &mut TsMuxer,
    video_timescale: u32,
    subtitle_timescale: u32,
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
) -> crate::Result<(u64, u64, u64)> {
    // 记录批内首段号，用于在未处理任何段时返回正确的 next 位置
    let batch_first = jobs.first().map(|j| j.num).unwrap_or(0);
    // 实际处理到的「下一个待处理段号」（被取消时只到已完成处）
    let mut next_processed = batch_first;
    // 下载层允许乱序完成，处理层用 job_order + pending map 恢复原段序。
    // 这样早期慢段不会阻止后续分片继续下载，但 DTS/PTS 和 mux 仍严格顺序推进。
    let job_order: Vec<u64> = jobs.iter().map(|j| j.num).collect();
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
                )
                .await
            };
            let audio = async move {
                match aurl {
                    Some(u) => fetch_validated_media(
                        &task_audio,
                        &dl_audio,
                        &u,
                        audio_dur_ts,
                        "audio",
                        num,
                    )
                    .await
                    .map(Some),
                    None => Ok(None),
                }
            };
            let subtitle = async move {
                match surl {
                    Some(u) => match task_subtitle.fetch_media(&dl_subtitle, &u).await {
                        Ok(data) => Ok(Some(data)),
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
                    dur_ts: video_dur_ts,
                }),
                Err(error) => Err(SegmentFetchFailure { num, error }),
            }
        }
    }))
    .buffer_unordered(fetch_concurrency);
    futures::pin_mut!(download_stream);
    let cancel = task.cancel_token();
    let mut pending: BTreeMap<u64, DownloadedSegment> = BTreeMap::new();
    let mut failed: BTreeSet<u64> = BTreeSet::new();
    let mut next_job_index = 0usize;

    while let Some(item) = download_stream.next().await {
        if cancel.is_cancelled() {
            break;
        }
        match item {
            Ok(segment) => {
                pending.insert(segment.num, segment);
            }
            Err(failure) => {
                if task.is_dynamic {
                    tracing::warn!(
                        task = %task.id,
                        seg = failure.num,
                        "segment fetch/validation failed; will retry after refresh: {:#}",
                        failure.error
                    );
                    failed.insert(failure.num);
                } else {
                    return Err(anyhow::anyhow!(
                        "segment #{} fetch/validation failed: {:#}",
                        failure.num,
                        failure.error
                    ));
                }
            }
        }

        while let Some(expected_num) = job_order.get(next_job_index).copied() {
            if failed.contains(&expected_num) {
                return Ok((acc_v_dts, acc_a_dts, next_processed));
            }
            let Some(segment) = pending.remove(&expected_num) else {
                break;
            };
            if cancel.is_cancelled() {
                pending.insert(expected_num, segment);
                break;
            }
            process_downloaded_segment(
                task,
                vdec,
                adec,
                sdec,
                video_codec,
                aac_cfg,
                muxer,
                video_timescale,
                subtitle_timescale,
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
            next_job_index += 1;
            next_processed = expected_num + 1;
            // 细粒度持久化进度：停止/暂停后精确从下一段续传
            task.resume_number.store(next_processed, Ordering::Relaxed);
            task.acc_v_dts.store(acc_v_dts, Ordering::Relaxed);
            task.acc_a_dts.store(acc_a_dts, Ordering::Relaxed);
        }
    }

    Ok((acc_v_dts, acc_a_dts, next_processed))
}

struct SegmentJob {
    num: u64,
    vurl: String,
    aurl: Option<String>,
    surl: Option<String>,
    video_dur_ts: u64,
    audio_dur_ts: Option<u64>,
}

struct SegmentFetchFailure {
    num: u64,
    error: anyhow::Error,
}

struct DownloadedSegment {
    num: u64,
    venc: bytes::Bytes,
    aenc: Option<bytes::Bytes>,
    senc: Option<bytes::Bytes>,
    dur_ts: u64,
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
            match audio.and_then(|a| short_source_group_audio_matches(video, group, a)) {
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

        for (pos, v) in group.iter().enumerate() {
            let audio_match = audio_matches
                .as_ref()
                .and_then(|matches| matches.get(pos))
                .map(|seg| (seg.url.clone(), seg.duration_ts));
            let subtitle_match = subtitle
                .and_then(|s| match_timed_segment_for_video(v, video.timescale, s))
                .map(|seg| (seg.url.clone(), seg.duration_ts));
            jobs.push(SegmentJob {
                num: v.number,
                vurl: v.url.clone(),
                aurl: audio_match.as_ref().map(|(url, _)| url.clone()),
                surl: subtitle_match.as_ref().map(|(url, _)| url.clone()),
                video_dur_ts: v.duration_ts,
                audio_dur_ts: audio_match.map(|(_, dur)| dur),
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
    audio: &'a Representation,
) -> Option<Vec<&'a SegmentRef>> {
    let mut matches = Vec::with_capacity(group.len());
    for video_seg in group {
        matches.push(match_audio_segment_for_video(video, video_seg, audio)?);
    }
    short_source_audio_covers_group(video, group, audio, &matches).then_some(matches)
}

fn match_audio_segment_for_video<'a>(
    video: &Representation,
    video_seg: &SegmentRef,
    audio: &'a Representation,
) -> Option<&'a SegmentRef> {
    if is_time_template(video) {
        match_audio_segment_by_time(video_seg, video.timescale, audio)
    } else {
        audio
            .segments
            .iter()
            .find(|seg| seg.number == video_seg.number)
    }
}

fn short_source_audio_covers_group(
    video: &Representation,
    group: &[&SegmentRef],
    audio: &Representation,
    audio_matches: &[&SegmentRef],
) -> bool {
    let Some((group_start, group_end)) = segment_group_interval(group, video.timescale) else {
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
        let start = segment_start_secs(seg, audio.timescale);
        let end = start + segment_duration_secs(seg, audio.timescale);
        let clipped_start = group_start.max(start - SHORT_GROUP_AUDIO_EDGE_SLACK_S);
        let clipped_end = group_end.min(end + SHORT_GROUP_AUDIO_EDGE_SLACK_S);
        if clipped_end > clipped_start {
            intervals.push((clipped_start, clipped_end));
        }
    }

    let covered = merged_interval_duration(intervals);
    covered * MIN_MEDIA_COVERAGE_DEN as f64 >= group_duration * MIN_MEDIA_COVERAGE_NUM as f64
}

fn segment_group_interval(group: &[&SegmentRef], timescale: u32) -> Option<(f64, f64)> {
    let first = group.first()?;
    let last = group.last()?;
    let start = segment_start_secs(first, timescale);
    let end = segment_start_secs(last, timescale) + segment_duration_secs(last, timescale);
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
) -> crate::Result<bytes::Bytes> {
    let mut last_reason = String::new();
    for attempt in 1..=MEDIA_CONTENT_ATTEMPTS {
        let data = task.fetch_media(dl, url).await?;
        match validate_media_coverage(&data, expected_duration_ts) {
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

fn validate_media_coverage(data: &[u8], expected_duration_ts: Option<u64>) -> Result<(), String> {
    let parsed =
        parse_media_segment(data).ok_or_else(|| "missing fMP4 sample table".to_string())?;
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

#[allow(clippy::too_many_arguments)]
fn process_downloaded_segment(
    task: &Arc<Task>,
    vdec: &Option<Decryptor>,
    adec: &Option<Decryptor>,
    sdec: &Option<Decryptor>,
    video_codec: VideoCodec,
    aac_cfg: Option<crate::mp4::aac::AacConfig>,
    muxer: &mut TsMuxer,
    video_timescale: u32,
    subtitle_timescale: u32,
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
) -> crate::Result<()> {
    let DownloadedSegment {
        num,
        venc,
        aenc,
        senc,
        dur_ts,
    } = segment;

    // 累计下载字节（速度展示）
    let downloaded = venc.len() as u64
        + aenc.as_ref().map(|a| a.len() as u64).unwrap_or(0)
        + senc.as_ref().map(|s| s.len() as u64).unwrap_or(0);
    task.bytes_done.fetch_add(downloaded, Ordering::Relaxed);

    // 视频：解密(若加密) + 构造 AU
    let mut vparsed = parse_media_segment(&venc)
        .ok_or_else(|| anyhow::anyhow!("parse video seg #{num} failed"))?;
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
        );
        *acc_v_dts = 0;
        *acc_a_dts = 0;
    }
    *last_v_tfdt = vparsed.base_media_decode_time;
    // 时间线锚点：优先用源 tfdt 把每段 DTS 锚定到绝对时基（消除独立累加器的取整漂移）。
    // 首段记 origin，之后段起始 dts = tfdt - origin。tfdt 缺失（非 CMAF）则回退累加。
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
        None => *acc_v_dts,
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
            video_codec,
        ));
    }
    // 回退累加器：始终延续段尾位置（tfdt 路径下不被消费，但保持续传时连续）。
    *acc_v_dts += vparsed
        .samples
        .iter()
        .map(|s| s.duration as u64)
        .sum::<u64>();

    // 音频：解密(若加密) + 构造 AU（AAC 需加 ADTS 头；EC-3/AC-3 裸透传）
    let mut aaus = Vec::new();
    let mut aau_durations = Vec::new();
    let mut seg_a_dur_ts = 0u64;
    if let Some(aenc) = aenc {
        let mut aparsed = parse_media_segment(&aenc)
            .ok_or_else(|| anyhow::anyhow!("parse audio seg #{num} failed"))?;
        if aparsed.samples.is_empty() {
            return Err(anyhow::anyhow!("audio seg #{num} has no samples"));
        }
        if let Some(adec) = adec {
            adec.decrypt_segment(&mut aparsed.mdat, &aparsed.samples);
        }
        // 音频时间线锚定到「音频自身的 tfdt」：每段起点 = a_tfdt - a_origin。
        // 音频帧是连续的 EC-3 流，其 tfdt 是源标注的绝对解码时间；直接据此定位，
        // 音频 PTS 恒等于源标注，内容连续、绝不重叠/重复。
        //
        // 不再强制对齐到「视频段边界」：实测源的真实 A/V offset 并非恒定
        //（曾观测到 82ms→50ms 的整帧级跳变 + 缓慢漂移），若按首段 offset 把音频
        // 硬塞到视频段边界，源 offset 变化时音频 PTS 就与音频实际内容错位整帧，
        // 在段衔接处表现为「音频重复播放（一句话重说）」。各自锚定 tfdt 后，
        // A/V offset 自然跟随源（与原始 DASH 播放器一致），既不重复也不漂移。
        // tfdt 缺失（非 CMAF）则回退到音频自累加（与旧行为一致）。
        let mut seg_a_pts = match aparsed.base_media_decode_time {
            Some(a_tfdt) => {
                let a_origin = task.origin_a_tfdt.load(Ordering::Relaxed);
                let a_origin = if a_origin == u64::MAX {
                    task.origin_a_tfdt.store(a_tfdt, Ordering::Relaxed);
                    a_tfdt
                } else {
                    a_origin
                };
                a_tfdt.saturating_sub(a_origin)
            }
            None => *acc_a_dts,
        };
        for s in &aparsed.samples {
            let (x, y) = s.data_range;
            let y = y.min(aparsed.mdat.len());
            let pts = seg_a_pts;
            seg_a_pts += s.duration as u64;
            let frame = &aparsed.mdat[x..y];
            let data = match aac_cfg {
                Some(cfg) => cfg.wrap_adts(frame), // AAC → ADTS
                None => frame.to_vec(),            // EC-3/AC-3 裸透传
            };
            aaus.push(AudioUnit { data, pts });
            aau_durations.push(s.duration as u64);
        }
        seg_a_dur_ts = aparsed
            .samples
            .iter()
            .map(|s| s.duration as u64)
            .sum::<u64>();
        *acc_a_dts += seg_a_dur_ts;
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
        let expected_v = first_v + *acc_dur_ts;
        let v_off = expected_v as i64 - vaus[0].dts as i64;
        if v_off != 0 {
            for au in &mut vaus {
                au.dts = (au.dts as i64 + v_off).max(0) as u64;
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

    // 累积 AUs：短分片垒起来，封成一个合段再输出
    acc_vaus.extend(vaus);
    acc_aaus.extend(aaus);
    *acc_dur_ts += dur_ts;
    *agg_audio_dur_ts += retained_a_dur_ts;
    let single_dur = dur_ts as f64 / video_timescale as f64;
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
        );
    }
    tracing::debug!(task = %task.id, seg = num, "segment muxed");
    Ok(())
}

fn detect_audio_codec(codecs: &str) -> AudioCodec {
    let c = codecs.to_ascii_lowercase();
    if c.contains("ec-3") || c.contains("ec3") {
        AudioCodec::Ec3
    } else if c.contains("ac-3") || c.contains("ac3") {
        AudioCodec::Ac3
    } else {
        AudioCodec::AacAdts
    }
}

fn effective_mpd_kid(
    key_mode: super::KeyMode,
    tenc: Option<&TrackEncryption>,
    rep: Option<&Representation>,
) -> Option<String> {
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
    keystore: &KeyStore,
    vtenc: Option<&TrackEncryption>,
    vkid: Option<&str>,
) -> crate::Result<Option<Decryptor>> {
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
    keystore: &KeyStore,
    atenc: Option<&TrackEncryption>,
    akid: Option<&str>,
) -> Option<Decryptor> {
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
    keystore: &KeyStore,
    stenc: Option<&TrackEncryption>,
    skid: Option<&str>,
) -> Option<Decryptor> {
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
    aac_cfg: &mut Option<crate::mp4::aac::AacConfig>,
) -> crate::Result<MpdKeyState> {
    task.set_stage("检查动态 KEY");
    let vinit = dl.get(&video.init_url).await?;
    let vtenc = TrackEncryption::find_in_init(&vinit);
    let vkid = effective_mpd_kid(super::KeyMode::Dynamic, vtenc.as_ref(), Some(video));

    let mut atenc = None;
    if let Some(a) = audio {
        let ainit = dl.get(&a.init_url).await?;
        if audio_codec == Some(AudioCodec::AacAdts) {
            *aac_cfg = crate::mp4::aac::AacConfig::find_in_init(&ainit);
        }
        atenc = TrackEncryption::find_in_init(&ainit);
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

    *vdec = build_mpd_video_decryptor(task, keystore, vtenc.as_ref(), vkid.as_deref())?;
    *adec = build_mpd_audio_decryptor(task, keystore, atenc.as_ref(), akid.as_deref());
    *sdec = build_mpd_subtitle_decryptor(task, keystore, stenc.as_ref(), skid.as_deref());
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

fn match_audio_segment_by_time<'a>(
    video_seg: &SegmentRef,
    video_timescale: u32,
    audio_rep: &'a Representation,
) -> Option<&'a SegmentRef> {
    let v_start = segment_start_secs(video_seg, video_timescale);
    let v_dur = segment_duration_secs(video_seg, video_timescale);
    let v_end = v_start + v_dur;
    let v_mid = v_start + v_dur * 0.5;
    let edge_slack = 0.080;

    if let Some((seg, overlap)) = audio_rep
        .segments
        .iter()
        .filter_map(|seg| {
            let a_start = segment_start_secs(seg, audio_rep.timescale);
            let a_end = a_start + segment_duration_secs(seg, audio_rep.timescale);
            let overlap = v_end.min(a_end) - v_start.max(a_start);
            (overlap > 0.0).then_some((seg, overlap))
        })
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    {
        if overlap >= (v_dur * 0.20).min(0.400) {
            return Some(seg);
        }
    }

    if let Some(seg) = audio_rep.segments.iter().find(|seg| {
        let a_start = segment_start_secs(seg, audio_rep.timescale);
        let a_end = a_start + segment_duration_secs(seg, audio_rep.timescale);
        v_mid >= a_start - edge_slack && v_mid <= a_end + edge_slack
    }) {
        return Some(seg);
    }

    let best = audio_rep.segments.iter().min_by(|a, b| {
        let da = (segment_start_secs(a, audio_rep.timescale) - v_start).abs();
        let db = (segment_start_secs(b, audio_rep.timescale) - v_start).abs();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let best_delta = (segment_start_secs(best, audio_rep.timescale) - v_start).abs();
    let a_dur = segment_duration_secs(best, audio_rep.timescale);
    let tolerance = (v_dur.max(a_dur) * 0.60).max(0.350);
    (best_delta <= tolerance).then_some(best)
}

fn match_timed_segment_for_video<'a>(
    video_seg: &SegmentRef,
    video_timescale: u32,
    rep: &'a Representation,
) -> Option<&'a SegmentRef> {
    match_audio_segment_by_time(video_seg, video_timescale, rep)
}

fn is_time_template(rep: &Representation) -> bool {
    rep.media_template.contains("$Time$") && !rep.media_template.contains("$Number$")
}

fn segment_start_secs(seg: &SegmentRef, timescale: u32) -> f64 {
    seg.number as f64 / timescale.max(1) as f64
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
) {
    if acc_vaus.is_empty() {
        return;
    }
    let ts_bytes = muxer.mux_segment(acc_vaus, acc_aaus);
    let dur = *acc_dur_ts as f64 / video_timescale.max(1) as f64;
    let subtitle_body = subtitle_acc.take_body();
    if *pending_discontinuity {
        task.hls
            .push_segment_discontinuity(dur, bytes::Bytes::from(ts_bytes));
        if task.subtitles_enabled {
            task.subtitles
                .push_segment_discontinuity(dur, subtitle_body);
        }
        *pending_discontinuity = false;
    } else {
        task.hls.push_segment(dur, bytes::Bytes::from(ts_bytes));
        if task.subtitles_enabled {
            task.subtitles.push_segment(dur, subtitle_body);
        }
    }
    task.segments_done.fetch_add(1, Ordering::Relaxed);
    acc_vaus.clear();
    acc_aaus.clear();
    *acc_dur_ts = 0;
    *agg_audio_dur_ts = 0;
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
) {
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
        );
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
async fn scan_min_cts(dl: &crate::fetch::SharedDownloader, video: &Representation) -> Option<i64> {
    let first = video.segments.first()?;
    let data = dl.get(&first.url).await.ok()?;
    let parsed = parse_media_segment(&data)?;
    parsed.samples.iter().map(|s| s.cts_offset as i64).min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{SourceKind, TrackSelection};

    fn seg(number: u64, duration_ts: u64) -> SegmentRef {
        SegmentRef {
            number,
            url: format!("seg-{number}.m4s"),
            duration_ts,
        }
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
            media_template: "v-$Time$.m4s".into(),
            start_number: 1,
            segments,
            total_duration_ts: 0,
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
            media_template: "a-$Time$.m4s".into(),
            start_number: 1,
            segments,
            total_duration_ts: 0,
        }
    }

    #[test]
    fn aggregate_audio_ready_rejects_short_audio() {
        assert!(aggregate_audio_ready(true, 460_800, 48_000, 5_760, 600));
        assert!(!aggregate_audio_ready(true, 368_640, 48_000, 5_760, 600));
        assert!(aggregate_audio_ready(false, 0, 48_000, 5_760, 600));
    }

    #[test]
    fn mpd_audio_codec_detection_distinguishes_ac3_ec3_and_aac() {
        assert_eq!(detect_audio_codec("ac-3"), AudioCodec::Ac3);
        assert_eq!(detect_audio_codec("ec-3"), AudioCodec::Ec3);
        assert_eq!(detect_audio_codec("mp4a.40.2"), AudioCodec::AacAdts);
    }

    #[test]
    fn time_template_audio_match_uses_seconds_not_index() {
        let video = seg(1_069_949_867_522, 1_152);
        let audio = audio_rep(
            48_000,
            vec![
                seg(85_595_989_401_792 - 92_160, 92_160),
                seg(85_595_989_401_792, 92_160),
                seg(85_595_989_401_792 + 92_160, 92_160),
            ],
        );
        let matched = match_audio_segment_by_time(&video, 600, &audio).unwrap();
        assert_eq!(matched.number, 85_595_989_401_792);
    }

    #[test]
    fn time_template_audio_match_uses_interval_overlap_for_variable_audio() {
        let video = seg(17_833_422_517_292_555, 20_020_000);
        let audio = audio_rep(
            10_000_000,
            vec![
                seg(17_833_422_505_215_777, 23_466_667),
                seg(17_833_422_528_682_444, 9_386_667),
            ],
        );
        let matched = match_audio_segment_by_time(&video, 10_000_000, &audio).unwrap();
        assert_eq!(matched.number, 17_833_422_505_215_777);
    }

    #[test]
    fn time_template_audio_match_prefers_best_overlap_over_midpoint() {
        let video = seg(17_833_435_129_892_555, 20_020_000);
        let audio = audio_rep(
            10_000_000,
            vec![
                seg(17_833_435_117_389_111, 22_400_000),
                seg(17_833_435_139_789_111, 11_200_000),
            ],
        );
        let matched = match_audio_segment_by_time(&video, 10_000_000, &audio).unwrap();
        assert_eq!(matched.number, 17_833_435_139_789_111);
    }

    #[test]
    fn short_source_live_jobs_publish_only_complete_telus_style_groups() {
        let dur = 20_020_000;
        let video = video_rep(10_000_000, (0..6).map(|i| seg(i * dur, dur)).collect());
        let audio = audio_rep(10_000_000, (0..4).map(|i| seg(i * dur, dur)).collect());

        let group_size = short_source_group_size(&video).unwrap();
        let plan = build_short_source_live_jobs(&video, Some(&audio), None, true, 0, group_size);

        assert_eq!(group_size, 4);
        assert_eq!(plan.jobs.len(), 4);
        assert_eq!(
            plan.jobs.iter().map(|j| j.num).collect::<Vec<_>>(),
            vec![0, dur, dur * 2, dur * 3]
        );
        assert!(plan.blocked_on_audio.is_none());
        assert!(!plan.skipped_incomplete_head);
    }

    #[test]
    fn short_source_live_jobs_hold_tail_until_audio_group_is_complete() {
        let dur = 20_020_000;
        let video = video_rep(10_000_000, (0..4).map(|i| seg(i * dur, dur)).collect());
        let audio = audio_rep(10_000_000, (0..2).map(|i| seg(i * dur, dur)).collect());

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
        let video = video_rep(10_000_000, (0..8).map(|i| seg(i * dur, dur)).collect());
        let audio = audio_rep(10_000_000, (0..8).map(|i| seg(i * dur, dur)).collect());

        let plan = build_short_source_live_jobs(&video, Some(&audio), None, true, dur, 4);

        assert_eq!(plan.jobs.len(), 4);
        assert_eq!(
            plan.jobs.iter().map(|j| j.num).collect::<Vec<_>>(),
            vec![dur * 4, dur * 5, dur * 6, dur * 7]
        );
        assert!(plan.skipped_incomplete_head);
        assert!(plan.blocked_on_audio.is_none());
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
            media_template: "v-$Time$.m4s".into(),
            start_number: 1,
            segments: vec![
                seg(17832777776145666, 20_020_000),
                seg(17832777796165666, 20_020_000),
                seg(17832777816185666, 20_020_000),
                seg(17832777836205666, 20_020_000),
                seg(17832777856225666, 20_020_000),
            ],
            total_duration_ts: 0,
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
