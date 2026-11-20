//! 分段存储 + 滚动 m3u8 生成 + GC。
//!
//! 决策：仅内存。.ts 段存内存 (HTTP 直接服务)，无磁盘 IO，性能最佳。
//! 维护 live window（playlist 只发布最近 window 段），但内存额外多留
//! LIVE_GRACE_SEGMENTS 段：某段从发布窗口滚出后仍可被 `get_segment` 命中一小段时间，
//! 兜住「远端冷 CDN 边缘回源 / 攥着旧 playlist 的切台播放器」恰好赶上该段被 GC 的
//! 404 竞态。MEDIA-SEQUENCE 由首个发布段的 seq 推导，与发布窗口严格一致。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

/// 已滚出发布窗口、但仍驻留内存以兜底在途请求的额外段数。
/// 发布窗口外再多留 grace 段：某段从 playlist 滚出后仍能被命中约 grace×段时长
/// （默认 3 段 ≈ 12–18s），消除远端冷边缘回源时段已被 GC 的 404 竞态。
const LIVE_GRACE_SEGMENTS: usize = 3;

const MIB: usize = 1024 * 1024;
const MAX_OUTPUT_SEGMENTS: usize = 50_000;

#[derive(Debug, thiserror::Error)]
#[error("output cache {scope} limit exceeded ({limit} bytes or segments)")]
pub struct OutputLimitExceeded {
    scope: &'static str,
    limit: usize,
}

struct MemoryBudget {
    used: AtomicUsize,
    limit: usize,
    scope: &'static str,
}

impl MemoryBudget {
    fn new(limit: usize, scope: &'static str) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
            scope,
        })
    }

    fn reserve(self: &Arc<Self>, size: usize) -> crate::Result<MemoryLease> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(size).filter(|total| *total <= self.limit)
            })
            .map_err(|_| OutputLimitExceeded {
                scope: self.scope,
                limit: self.limit,
            })?;
        Ok(MemoryLease {
            budget: self.clone(),
            size,
        })
    }
}

struct MemoryLease {
    budget: Arc<MemoryBudget>,
    size: usize,
}

impl Drop for MemoryLease {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.size, Ordering::AcqRel);
    }
}

struct RetainedBytes {
    data: bytes::Bytes,
    _task: MemoryLease,
    _global: MemoryLease,
}

impl AsRef<[u8]> for RetainedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

fn budgeted(data: bytes::Bytes, task: &Arc<MemoryBudget>) -> crate::Result<bytes::Bytes> {
    static GLOBAL: OnceLock<Arc<MemoryBudget>> = OnceLock::new();
    let global = GLOBAL
        .get_or_init(|| MemoryBudget::new(cache_limit("DVHLS_OUTPUT_GLOBAL_MIB", 1024), "global"));
    let size = data.len();
    Ok(bytes::Bytes::from_owner(RetainedBytes {
        data,
        _task: task.reserve(size)?,
        _global: global.reserve(size)?,
    }))
}

fn cache_limit(env: &str, default_mib: usize) -> usize {
    std::env::var(env)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|mib| *mib > 0)
        .and_then(|mib| mib.checked_mul(MIB))
        .unwrap_or(default_mib * MIB)
}

fn check_segment_count(count: usize) -> crate::Result<()> {
    if count >= MAX_OUTPUT_SEGMENTS {
        return Err(OutputLimitExceeded {
            scope: "task segment count",
            limit: MAX_OUTPUT_SEGMENTS,
        }
        .into());
    }
    Ok(())
}

fn window_range(total: usize, window: usize, delay: usize, finished: bool) -> (usize, usize) {
    if window == 0 {
        return (0, total);
    }
    // On natural EOF release *all* buffered tail segments, even when delay exceeds window.
    let (end, size) = if finished {
        (total, window.saturating_add(delay))
    } else {
        (total.saturating_sub(delay), window)
    };
    (end.saturating_sub(size), end)
}

/// 一个已封装好的 TS 段。
#[derive(Clone)]
pub struct TsSegment {
    pub seq: u64,
    pub duration: f64,
    pub data: bytes::Bytes,
    pub discontinuity: bool,
}

#[derive(Clone)]
pub struct SubtitleSegment {
    pub seq: u64,
    pub duration: f64,
    pub body: bytes::Bytes,
    pub discontinuity: bool,
}

/// 一个任务的 HLS 输出（线程安全）。
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct HlsOutput {
    inner: Mutex<Inner>,
    budget: Arc<MemoryBudget>,
    /// live window 大小（保留段数）；0 表示 VOD（不删）。
    window: usize,
    /// 目标段时长（秒，向上取整到整数秒）。在 push_segment 时动态更新为实际最大段时长，
    /// 保证 HLS spec 要求 TARGETDURATION ≥ 所有 EXTINF（尤其聚合段可能超出初始设定）。
    target_duration: AtomicU64,
    /// live 输出时隐藏最新 N 个已产出段，不马上写进 playlist。
    /// 这给播放器和生产端之间留一段完整缓冲，避免一次慢下载/慢封装直接导致断粮。
    publish_delay_segments: AtomicUsize,
    video_range: String,
    is_live: bool,
    /// 段 URL 的前缀（绝对路径），用于 ExoPlayer 兼容。
    base_url: String,
}

struct Inner {
    segments: VecDeque<TsSegment>,
    codecs: Option<String>,
    next_seq: u64,
    finished: bool,
    removed_discontinuities: u64,
    pending_discontinuity: bool,
}

pub struct SubtitleOutput {
    inner: Mutex<SubtitleInner>,
    budget: Arc<MemoryBudget>,
    window: usize,
    target_duration: AtomicU64,
    publish_delay_segments: AtomicUsize,
    is_live: bool,
}

struct SubtitleInner {
    segments: VecDeque<SubtitleSegment>,
    next_seq: u64,
    finished: bool,
    removed_discontinuities: u64,
}

impl HlsOutput {
    pub fn new(
        window: usize,
        target_duration: u64,
        codecs: String,
        video_range: String,
        is_live: bool,
        base_url: String,
        publish_delay_segments: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                segments: VecDeque::new(),
                codecs: (!codecs.is_empty()).then_some(codecs),
                next_seq: 0,
                finished: false,
                removed_discontinuities: 0,
                pending_discontinuity: false,
            }),
            budget: MemoryBudget::new(cache_limit("DVHLS_OUTPUT_TASK_MIB", 384), "task video"),
            window,
            target_duration: AtomicU64::new(target_duration),
            publish_delay_segments: AtomicUsize::new(if is_live {
                publish_delay_segments.clamp(1, 64)
            } else {
                0
            }),
            video_range,
            is_live,
            base_url,
        }
    }

    pub fn set_publish_delay_segments(&self, delay: usize) {
        let delay = if self.is_live { delay.clamp(1, 64) } else { 0 };
        self.publish_delay_segments.store(delay, Ordering::Relaxed);
    }

    /// A refreshed HLS init can describe different codecs while old segments
    /// still remain in the playlist. Do not claim one set for a mixed output.
    pub(crate) fn observe_output_codecs(&self, codecs: Option<String>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.next_seq == 0 {
            inner.codecs = codecs;
        } else if inner.codecs != codecs {
            inner.codecs = None;
        }
    }

    /// 追加一个新段，返回其 seq。触发 GC。
    pub fn push_segment(&self, duration: f64, data: bytes::Bytes) -> crate::Result<u64> {
        self.push_segment_pair(duration, data, false, None)
    }

    /// 追加一个新段，并在 playlist 里把它标记为一个新解码 epoch 的起点。
    pub fn push_segment_discontinuity(
        &self,
        duration: f64,
        data: bytes::Bytes,
    ) -> crate::Result<u64> {
        self.push_segment_pair(duration, data, true, None)
    }

    /// Reserve both renditions before publishing either segment. The media
    /// and subtitle playlists retain identical sequence/discontinuity origins
    /// even when a subtitle exhausts the output cache.
    pub fn push_segment_pair(
        &self,
        duration: f64,
        data: bytes::Bytes,
        discontinuity: bool,
        subtitle: Option<(&SubtitleOutput, String, u64)>,
    ) -> crate::Result<u64> {
        anyhow::ensure!(
            duration.is_finite() && duration > 0.0,
            "invalid output segment duration"
        );
        let formatted = subtitle.map(|(output, body, pts)| {
            (
                output,
                bytes::Bytes::from(format_webvtt_segment(&body, Some(pts))),
            )
        });
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = g.next_seq;
        let next = seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("output sequence overflow"))?;
        let discontinuity = discontinuity || g.pending_discontinuity;
        if self.window > 0 {
            let delay = self.publish_delay_segments.load(Ordering::Relaxed);
            let cap = self
                .window
                .saturating_add(delay)
                .saturating_add(LIVE_GRACE_SEGMENTS);
            while g.segments.len() >= cap {
                if let Some(old) = g.segments.pop_front() {
                    g.removed_discontinuities += u64::from(old.discontinuity);
                }
            }
        }
        check_segment_count(g.segments.len())?;
        let data = budgeted(data, &self.budget)?;
        if let Some((output, text)) = formatted {
            let mut sub = output
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            anyhow::ensure!(
                sub.next_seq == seq,
                "video/subtitle output sequences differ"
            );
            let text = output.reserve_segment(&mut sub, text)?;
            // No fallible operation follows this point for either rendition.
            output.commit_segment(&mut sub, seq, next, duration, text, discontinuity);
            self.commit_segment(&mut g, seq, next, duration, data, discontinuity);
        } else {
            self.commit_segment(&mut g, seq, next, duration, data, discontinuity);
        }
        Ok(seq)
    }

    fn commit_segment(
        &self,
        g: &mut Inner,
        seq: u64,
        next: u64,
        duration: f64,
        data: bytes::Bytes,
        discontinuity: bool,
    ) {
        g.next_seq = next;
        g.pending_discontinuity = false;
        g.segments.push_back(TsSegment {
            seq,
            duration,
            data,
            discontinuity,
        });
        self.target_duration
            .fetch_max(duration.ceil() as u64, Ordering::Relaxed);
    }

    pub(crate) fn mark_next_discontinuity(&self) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.pending_discontinuity |= g.next_seq > 0;
    }

    /// 据发布窗口算出 playlist 的 [start, end) 与 MEDIA-SEQUENCE（首个发布段的 seq）。
    /// 内存里下标 < start 的是 grace 段；下标 >= end 的是 publish_delay 段。
    /// window==0（点播）时发布全部段，起始下标为 0。
    fn publish_range(&self, g: &Inner) -> (usize, usize, u64, u64) {
        let delay = self.publish_delay_segments.load(Ordering::Relaxed);
        let (start, end) = window_range(g.segments.len(), self.window, delay, g.finished);
        let media_seq = g.segments.get(start).map(|s| s.seq).unwrap_or(g.next_seq);
        let disc_seq = g.removed_discontinuities
            + g.segments
                .iter()
                .take(start)
                .filter(|s| s.discontinuity)
                .count() as u64;
        (start, end, media_seq, disc_seq)
    }

    pub fn published_segment_count(&self) -> usize {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (start, end, _, _) = self.publish_range(&g);
        end - start
    }

    /// 按需直播空闲停转时清掉旧窗口，避免下一次唤醒先暴露过期 live 段。
    /// next_seq 保持递增，所以对外 URL 不回退，播放器也不会看到旧 seq 复用。
    pub fn clear_live_segments_keep_sequence(&self) {
        if self.is_live {
            self.clear_segments_keep_sequence();
        }
    }

    pub(crate) fn clear_segments_keep_sequence(&self) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.removed_discontinuities += g.segments.iter().filter(|s| s.discontinuity).count() as u64;
        g.segments.clear();
        g.finished = false;
        g.pending_discontinuity |= g.next_seq > 0;
    }

    /// 标记直播结束（写 ENDLIST）。
    pub fn finish(&self) {
        self.finish_pair(None);
    }

    pub(crate) fn finish_pair(&self, subtitles: Option<&SubtitleOutput>) {
        let mut video = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(subtitles) = subtitles {
            let mut subs = subtitles
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            video.finished = true;
            subs.finished = true;
        } else {
            video.finished = true;
        }
    }

    /// 取某 seq 的段数据。
    pub fn get_segment(&self, seq: u64) -> Option<bytes::Bytes> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| s.data.clone())
    }

    /// 当前段数（用于状态展示）。
    pub fn segment_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .segments
            .len()
    }

    pub fn total_produced(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_seq
    }

    /// 生成 media playlist，使用绝对 URL（http://host/p/<id>/s?n=0），
    /// 与参考站 https://cdn/api?h=xxx&u=xxx 格式一致。
    pub fn media_playlist_absolute(&self, prefix: &str) -> String {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 只发布稳定窗口；grace 段和 publish_delay 段都不写入 playlist。
        let (start, end, media_seq, disc_seq) = self.publish_range(&g);
        let mut s = String::new();
        s.push_str("#EXTM3U\n");
        s.push_str("#EXT-X-VERSION:3\n");
        s.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            (self.target_duration.load(Ordering::Relaxed)).max(1)
        ));
        s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_seq));
        if self.is_live {
            s.push_str(&format!("#EXT-X-DISCONTINUITY-SEQUENCE:{disc_seq}\n"));
        }
        let rolling = self.window > 0;
        // 非滚动（点播）：段只增不删。下载中标 EVENT 让播放器持续刷新拿新段，
        // 全部下完才补 ENDLIST —— 避免把"未下完"的点播误标成完整 VOD 导致播放器提前停。
        if !rolling {
            s.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        for seg in g
            .segments
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
        {
            if seg.discontinuity {
                s.push_str("#EXT-X-DISCONTINUITY\n");
            }
            s.push_str(&format!("#EXTINF:{:.3},\n", seg.duration));
            s.push_str(&format!("{prefix}/picture-{}.jpeg\n", seg.seq));
        }
        if g.finished {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    }

    /// 生成 media playlist (m3u8) 文本。
    pub fn media_playlist(&self) -> String {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 只发布稳定窗口；grace 段和 publish_delay 段都不写入 playlist。
        let (start, end, media_seq, disc_seq) = self.publish_range(&g);
        let mut s = String::new();
        s.push_str("#EXTM3U\n");
        s.push_str("#EXT-X-VERSION:3\n");
        s.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            (self.target_duration.load(Ordering::Relaxed)).max(1)
        ));
        s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_seq));
        if self.is_live {
            s.push_str(&format!("#EXT-X-DISCONTINUITY-SEQUENCE:{disc_seq}\n"));
        }
        // window>0 表示滚动窗口（实时删历史），不是可 seek 的完整 VOD
        let rolling = self.window > 0;
        if !rolling {
            s.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        for seg in g
            .segments
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
        {
            if seg.discontinuity {
                s.push_str("#EXT-X-DISCONTINUITY\n");
            }
            s.push_str(&format!("#EXTINF:{:.3},\n", seg.duration));
            // 绝对路径 + query string（同参考站 https://cdn/api?h=xxx&u=xxx 模式）
            s.push_str(&format!("{}/picture-{}.jpeg\n", self.base_url, seg.seq));
        }
        // 仅在非滚动且已全部下完时写 ENDLIST
        if g.finished {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    }

    /// 生成 master playlist，引用伪装后的 media playlist，带 CODECS 与 VIDEO-RANGE。
    pub fn master_playlist(&self, bandwidth: u64, width: u32, height: u32) -> String {
        self.master_playlist_absolute(&self.base_url, bandwidth, width, height, None)
    }

    pub fn master_playlist_absolute(
        &self,
        prefix: &str,
        bandwidth: u64,
        width: u32,
        height: u32,
        subtitles: Option<SubtitleRendition<'_>>,
    ) -> String {
        let mut out = String::new();
        out.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");
        if let Some(sub) = subtitles {
            out.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,LANGUAGE=\"{}\",URI=\"{}/xyz\"\n",
                hls_quote(sub.name),
                hls_quote(sub.lang),
                prefix
            ));
        }
        out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}"));
        {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(codecs) = &inner.codecs {
                let subs = if subtitles.is_some() { ",wvtt" } else { "" };
                out.push_str(&format!(",CODECS=\"{}{subs}\"", hls_quote(codecs)));
            }
        }
        out.push_str(&format!(
            ",RESOLUTION={width}x{height},VIDEO-RANGE={}",
            self.video_range,
        ));
        if subtitles.is_some() {
            out.push_str(",SUBTITLES=\"subs\"");
        }
        out.push('\n');
        out.push_str(&format!("{prefix}/api\n"));
        out
    }
}

#[derive(Clone, Copy)]
pub struct SubtitleRendition<'a> {
    pub name: &'a str,
    pub lang: &'a str,
}

impl SubtitleOutput {
    pub fn new(
        window: usize,
        target_duration: u64,
        is_live: bool,
        publish_delay_segments: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(SubtitleInner {
                segments: VecDeque::new(),
                next_seq: 0,
                finished: false,
                removed_discontinuities: 0,
            }),
            budget: MemoryBudget::new(
                cache_limit("DVHLS_OUTPUT_SUBTITLE_MIB", 16),
                "task subtitles",
            ),
            window,
            target_duration: AtomicU64::new(target_duration),
            publish_delay_segments: AtomicUsize::new(if is_live {
                publish_delay_segments.clamp(1, 64)
            } else {
                0
            }),
            is_live,
        }
    }

    pub fn set_publish_delay_segments(&self, delay: usize) {
        let delay = if self.is_live { delay.clamp(1, 64) } else { 0 };
        self.publish_delay_segments.store(delay, Ordering::Relaxed);
    }

    pub fn push_segment(&self, duration: f64, body: String) -> crate::Result<u64> {
        self.push_segment_inner(duration, format_webvtt_segment(&body, None), false)
    }

    pub fn push_segment_discontinuity(&self, duration: f64, body: String) -> crate::Result<u64> {
        self.push_segment_inner(duration, format_webvtt_segment(&body, None), true)
    }

    pub fn push_mapped_segment(
        &self,
        duration: f64,
        body: String,
        mpegts: u64,
        discontinuity: bool,
    ) -> crate::Result<u64> {
        self.push_segment_inner(
            duration,
            format_webvtt_segment(&body, Some(mpegts)),
            discontinuity,
        )
    }

    fn push_segment_inner(
        &self,
        duration: f64,
        body: String,
        discontinuity: bool,
    ) -> crate::Result<u64> {
        anyhow::ensure!(
            duration.is_finite() && duration > 0.0,
            "invalid output segment duration"
        );
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = g.next_seq;
        let next = seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("subtitle output sequence overflow"))?;
        let body = self.reserve_segment(&mut g, bytes::Bytes::from(body))?;
        self.commit_segment(&mut g, seq, next, duration, body, discontinuity);
        Ok(seq)
    }

    fn reserve_segment(
        &self,
        g: &mut SubtitleInner,
        body: bytes::Bytes,
    ) -> crate::Result<bytes::Bytes> {
        if self.window > 0 {
            let delay = self.publish_delay_segments.load(Ordering::Relaxed);
            let cap = self
                .window
                .saturating_add(delay)
                .saturating_add(LIVE_GRACE_SEGMENTS);
            while g.segments.len() >= cap {
                if let Some(old) = g.segments.pop_front() {
                    g.removed_discontinuities += u64::from(old.discontinuity);
                }
            }
        }
        check_segment_count(g.segments.len())?;
        budgeted(body, &self.budget)
    }

    fn commit_segment(
        &self,
        g: &mut SubtitleInner,
        seq: u64,
        next: u64,
        duration: f64,
        body: bytes::Bytes,
        discontinuity: bool,
    ) {
        g.next_seq = next;
        g.segments.push_back(SubtitleSegment {
            seq,
            duration,
            body,
            discontinuity,
        });
        self.target_duration
            .fetch_max(duration.ceil() as u64, Ordering::Relaxed);
    }

    fn publish_range(&self, g: &SubtitleInner) -> (usize, usize, u64, u64) {
        let delay = self.publish_delay_segments.load(Ordering::Relaxed);
        let (start, end) = window_range(g.segments.len(), self.window, delay, g.finished);
        let media_seq = g.segments.get(start).map(|s| s.seq).unwrap_or(g.next_seq);
        let disc_seq = g.removed_discontinuities
            + g.segments
                .iter()
                .take(start)
                .filter(|s| s.discontinuity)
                .count() as u64;
        (start, end, media_seq, disc_seq)
    }

    pub fn published_segment_count(&self) -> usize {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (start, end, _, _) = self.publish_range(&g);
        end - start
    }

    pub fn playlist_absolute(&self, prefix: &str) -> String {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (start, end, media_seq, disc_seq) = self.publish_range(&g);
        let rolling = self.window > 0;
        let mut s = String::new();
        s.push_str("#EXTM3U\n");
        s.push_str("#EXT-X-VERSION:3\n");
        s.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.target_duration.load(Ordering::Relaxed).max(1)
        ));
        s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_seq));
        if self.is_live {
            s.push_str(&format!("#EXT-X-DISCONTINUITY-SEQUENCE:{disc_seq}\n"));
        }
        if !rolling {
            s.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        for seg in g
            .segments
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
        {
            if seg.discontinuity {
                s.push_str("#EXT-X-DISCONTINUITY\n");
            }
            s.push_str(&format!("#EXTINF:{:.3},\n", seg.duration));
            s.push_str(&format!("{prefix}/xyz-{}.txt\n", seg.seq));
        }
        if g.finished {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    }

    pub fn get_segment_bytes(&self, seq: u64) -> Option<bytes::Bytes> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| s.body.clone())
    }

    pub fn get_segment(&self, seq: u64) -> Option<String> {
        self.get_segment_bytes(seq)
            .map(|body| String::from_utf8_lossy(&body).into_owned())
    }

    pub fn segment_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .segments
            .len()
    }

    pub fn clear_live_segments_keep_sequence(&self) {
        if self.is_live {
            self.clear_segments_keep_sequence();
        }
    }

    pub(crate) fn clear_segments_keep_sequence(&self) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.removed_discontinuities += g.segments.iter().filter(|s| s.discontinuity).count() as u64;
        g.segments.clear();
        g.finished = false;
    }

    pub fn finish(&self) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finished = true;
    }
}

fn format_webvtt_segment(body: &str, mpegts: Option<u64>) -> String {
    let body = body.trim().trim_start_matches('\u{feff}');
    if mpegts.is_none() && body.starts_with("WEBVTT") {
        return format!("{body}\n");
    }
    let mut result = String::from("WEBVTT\n");
    if let Some(pts) = mpegts {
        result.push_str(&format!(
            "X-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{}\n",
            crate::clock::wrap33(pts)
        ));
    }
    result.push('\n');
    // Accumulators normally pass only cues. If a full header is supplied,
    // drop it instead of keeping a conflicting source timestamp map.
    let body = if body.starts_with("WEBVTT") {
        body.split_once("\n\n").map(|(_, text)| text).unwrap_or("")
    } else {
        body
    };
    if !body.is_empty() {
        result.push_str(body);
        result.push('\n');
    }
    result
}

fn hls_quote(value: &str) -> String {
    // HLS has no portable escape syntax inside quoted attributes.
    value
        .chars()
        .map(|ch| match ch {
            '"' => '\'',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{HlsOutput, MemoryBudget, OutputLimitExceeded, SubtitleOutput, SubtitleRendition};

    fn output(window: usize, delay: usize) -> HlsOutput {
        HlsOutput::new(
            window,
            4,
            "hvc1,ec-3".to_string(),
            "PQ".to_string(),
            true,
            "/p/test".to_string(),
            delay,
        )
    }

    #[test]
    fn live_publish_delay_hides_latest_segment() {
        let hls = output(3, 1);
        for _ in 0..5 {
            hls.push_segment(4.0, bytes::Bytes::from_static(b"ts"))
                .unwrap();
        }

        let playlist = hls.media_playlist();
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:1\n"));
        assert!(playlist.contains("picture-1.jpeg"));
        assert!(playlist.contains("picture-3.jpeg"));
        assert!(!playlist.contains("picture-4.jpeg"));
        assert!(hls.get_segment(4).is_some());
    }

    #[test]
    fn live_gc_keeps_grace_and_delay_outside_playlist() {
        let hls = output(3, 1);
        for _ in 0..8 {
            hls.push_segment(4.0, bytes::Bytes::from_static(b"ts"))
                .unwrap();
        }

        let playlist = hls.media_playlist();
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:4\n"));
        assert!(!playlist.contains("picture-3.jpeg"));
        assert!(playlist.contains("picture-4.jpeg"));
        assert!(playlist.contains("picture-6.jpeg"));
        assert!(!playlist.contains("picture-7.jpeg"));
        assert!(hls.get_segment(1).is_some());
        assert!(hls.get_segment(7).is_some());
        assert!(hls.get_segment(0).is_none());
    }

    #[test]
    fn discontinuity_is_written_before_marked_segment() {
        let hls = HlsOutput::new(0, 4, "avc1".into(), "SDR".into(), false, "/p/x".into(), 0);
        hls.push_segment(4.0, bytes::Bytes::from_static(b"a"))
            .unwrap();
        hls.push_segment_discontinuity(4.0, bytes::Bytes::from_static(b"b"))
            .unwrap();

        let playlist = hls.media_playlist();
        let marker = playlist.find("#EXT-X-DISCONTINUITY").unwrap();
        let segment = playlist.find("picture-1.jpeg").unwrap();
        assert!(marker < segment);
    }

    #[test]
    fn clear_live_segments_keeps_sequence_moving_forward() {
        let hls = output(6, 1);
        hls.push_segment(4.0, bytes::Bytes::from_static(b"a"))
            .unwrap();
        hls.push_segment(4.0, bytes::Bytes::from_static(b"b"))
            .unwrap();
        hls.clear_live_segments_keep_sequence();

        let empty_playlist = hls.media_playlist();
        assert!(empty_playlist.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert!(!empty_playlist.contains("picture-0.jpeg"));

        hls.push_segment(4.0, bytes::Bytes::from_static(b"c"))
            .unwrap();
        hls.push_segment(4.0, bytes::Bytes::from_static(b"d"))
            .unwrap();
        let playlist = hls.media_playlist();
        assert!(playlist.contains("picture-2.jpeg"));
        assert!(!playlist.contains("picture-3.jpeg"));
        assert!(!playlist.contains("picture-0.jpeg"));
    }

    #[test]
    fn master_playlist_uses_obfuscated_child_paths() {
        let hls = output(6, 1);
        let playlist = hls.master_playlist_absolute(
            "http://127.0.0.1:37201/p/task",
            8_000_000,
            1920,
            1080,
            Some(SubtitleRendition {
                name: "English",
                lang: "en",
            }),
        );

        assert!(playlist.contains("URI=\"http://127.0.0.1:37201/p/task/xyz\""));
        assert!(playlist.contains("http://127.0.0.1:37201/p/task/api\n"));
        assert!(!playlist.contains("/media"));
        assert!(!playlist.contains("/subtitles"));
    }

    #[test]
    fn master_codecs_omitted_before_confirmation_and_after_epoch_change() {
        let hls = HlsOutput::new(
            0,
            4,
            String::new(),
            "SDR".into(),
            false,
            "/p/task".into(),
            0,
        );
        let sub = SubtitleRendition {
            name: "English",
            lang: "en",
        };
        let master = || hls.master_playlist_absolute("/p/task", 500_000, 160, 90, Some(sub));
        assert!(!master().contains("CODECS="));

        let ac4 = Some("avc1.42c00a,ac-4.02.01.00".into());
        hls.observe_output_codecs(ac4.clone());
        hls.push_segment(4.0, bytes::Bytes::from_static(b"a"))
            .unwrap();
        hls.observe_output_codecs(ac4.clone());
        assert!(master().contains("CODECS=\"avc1.42c00a,ac-4.02.01.00,wvtt\""));
        hls.observe_output_codecs(Some("avc1.42c00a,mp4a.40.2".into()));
        assert!(!master().contains("CODECS="));
        hls.observe_output_codecs(ac4);
        assert!(!master().contains("CODECS="));
    }

    #[test]
    fn subtitle_playlist_uses_txt_segments() {
        let subtitles = SubtitleOutput::new(6, 4, true, 1);
        subtitles
            .push_segment(4.0, "00:00:00.000 --> 00:00:01.000\nhello".into())
            .unwrap();
        subtitles
            .push_segment(4.0, "00:00:01.000 --> 00:00:02.000\nworld".into())
            .unwrap();

        let playlist = subtitles.playlist_absolute("http://127.0.0.1:37201/p/task");

        assert!(playlist.contains("http://127.0.0.1:37201/p/task/xyz-0.txt"));
        assert!(!playlist.contains("subtitle-"));
        assert!(!playlist.contains(".vtt"));
    }

    #[test]
    fn finished_window_releases_unpublished_tail_in_both_renditions() {
        let hls = output(2, 3);
        let subs = SubtitleOutput::new(2, 4, true, 3);
        for index in 0..6 {
            hls.push_segment_pair(
                4.0,
                bytes::Bytes::from_static(b"TS"),
                false,
                Some((&subs, String::new(), index * 360_000)),
            )
            .unwrap();
            assert_eq!(
                hls.published_segment_count(),
                subs.published_segment_count()
            );
            if index < 3 {
                assert_eq!(hls.published_segment_count(), 0);
            }
        }
        assert_eq!(hls.published_segment_count(), 2);
        assert!(!hls.media_playlist().contains("picture-5.jpeg"));
        hls.finish_pair(Some(&subs));
        assert_eq!(hls.published_segment_count(), 5);
        assert_eq!(subs.published_segment_count(), 5);
        assert!(hls
            .media_playlist()
            .contains("picture-5.jpeg\n#EXT-X-ENDLIST\n"));
        assert!(subs
            .playlist_absolute("/p/test")
            .contains("xyz-5.txt\n#EXT-X-ENDLIST\n"));
        assert!(subs
            .get_segment(5)
            .unwrap()
            .contains("LOCAL:00:00:00.000,MPEGTS:1800000"));
    }

    #[test]
    fn discontinuity_sequence_accounts_for_hidden_and_evicted_history() {
        let hls = output(2, 1);
        let subs = SubtitleOutput::new(2, 4, true, 1);
        for index in 0..9 {
            hls.push_segment_pair(
                4.0,
                bytes::Bytes::from_static(b"TS"),
                index == 1,
                Some((&subs, String::new(), index * 360_000)),
            )
            .unwrap();
            if [4, 8].contains(&index) {
                let media = hls.media_playlist();
                let text = subs.playlist_absolute("/p/test");
                for m3u8 in [media, text] {
                    assert!(m3u8.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
                    assert!(!m3u8.contains("#EXT-X-DISCONTINUITY\n"));
                }
            }
        }
    }

    #[test]
    fn budget_tracks_inflight_bodies_and_pair_failure_is_atomic() {
        let mut hls = output(0, 1);
        hls.budget = MemoryBudget::new(3, "test video");
        let mut subs = SubtitleOutput::new(0, 4, true, 1);
        subs.budget = MemoryBudget::new(1, "test subtitle");
        let err = hls
            .push_segment_pair(
                4.0,
                bytes::Bytes::from_static(b"abc"),
                false,
                Some((&subs, "cue".into(), 0)),
            )
            .unwrap_err();
        assert!(err.is::<OutputLimitExceeded>());
        assert_eq!(hls.segment_count(), 0);
        assert_eq!(subs.segment_count(), 0);
        assert_eq!(
            hls.push_segment(4.0, bytes::Bytes::from_static(b"abc"))
                .unwrap(),
            0
        );
        let response = hls.get_segment(0).unwrap();
        hls.clear_live_segments_keep_sequence();
        assert!(hls
            .push_segment(4.0, bytes::Bytes::from_static(b"d"))
            .unwrap_err()
            .is::<OutputLimitExceeded>());
        drop(response);
        assert_eq!(
            hls.push_segment(4.0, bytes::Bytes::from_static(b"d"))
                .unwrap(),
            1
        );
    }
}
