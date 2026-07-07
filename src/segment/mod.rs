//! 分段存储 + 滚动 m3u8 生成 + GC。
//!
//! 决策：仅内存。.ts 段存内存 (HTTP 直接服务)，无磁盘 IO，性能最佳。
//! 维护 live window（playlist 只发布最近 window 段），但内存额外多留
//! LIVE_GRACE_SEGMENTS 段：某段从发布窗口滚出后仍可被 `get_segment` 命中一小段时间，
//! 兜住「远端冷 CDN 边缘回源 / 攥着旧 playlist 的切台播放器」恰好赶上该段被 GC 的
//! 404 竞态。MEDIA-SEQUENCE 由首个发布段的 seq 推导，与发布窗口严格一致。

use std::collections::VecDeque;
use std::sync::Mutex;

/// 已滚出发布窗口、但仍驻留内存以兜底在途请求的额外段数。
/// 发布窗口外再多留 grace 段：某段从 playlist 滚出后仍能被命中约 grace×段时长
/// （默认 3 段 ≈ 12–18s），消除远端冷边缘回源时段已被 GC 的 404 竞态。
const LIVE_GRACE_SEGMENTS: usize = 3;

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
    pub body: String,
    pub discontinuity: bool,
}

/// 一个任务的 HLS 输出（线程安全）。
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct HlsOutput {
    inner: Mutex<Inner>,
    /// live window 大小（保留段数）；0 表示 VOD（不删）。
    window: usize,
    /// 目标段时长（秒，向上取整到整数秒）。在 push_segment 时动态更新为实际最大段时长，
    /// 保证 HLS spec 要求 TARGETDURATION ≥ 所有 EXTINF（尤其聚合段可能超出初始设定）。
    target_duration: AtomicU64,
    /// live 输出时隐藏最新 N 个已产出段，不马上写进 playlist。
    /// 这给播放器和生产端之间留一段完整缓冲，避免一次慢下载/慢封装直接导致断粮。
    publish_delay_segments: AtomicUsize,
    codecs: String,
    video_range: String,
    is_live: bool,
    /// 段 URL 的前缀（绝对路径），用于 ExoPlayer 兼容。
    base_url: String,
}

struct Inner {
    segments: VecDeque<TsSegment>,
    next_seq: u64,
    finished: bool,
}

pub struct SubtitleOutput {
    inner: Mutex<SubtitleInner>,
    window: usize,
    target_duration: AtomicU64,
    publish_delay_segments: AtomicUsize,
    is_live: bool,
}

struct SubtitleInner {
    segments: VecDeque<SubtitleSegment>,
    next_seq: u64,
    finished: bool,
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
                next_seq: 0,
                finished: false,
            }),
            window,
            target_duration: AtomicU64::new(target_duration),
            publish_delay_segments: AtomicUsize::new(if is_live {
                publish_delay_segments.max(1)
            } else {
                0
            }),
            codecs,
            video_range,
            is_live,
            base_url,
        }
    }

    pub fn set_publish_delay_segments(&self, delay: usize) {
        let delay = if self.is_live { delay.max(1) } else { 0 };
        self.publish_delay_segments.store(delay, Ordering::Relaxed);
    }

    /// 追加一个新段，返回其 seq。触发 GC。
    pub fn push_segment(&self, duration: f64, data: bytes::Bytes) -> u64 {
        self.push_segment_inner(duration, data, false)
    }

    /// 追加一个新段，并在 playlist 里把它标记为一个新解码 epoch 的起点。
    pub fn push_segment_discontinuity(&self, duration: f64, data: bytes::Bytes) -> u64 {
        self.push_segment_inner(duration, data, true)
    }

    fn push_segment_inner(&self, duration: f64, data: bytes::Bytes, discontinuity: bool) -> u64 {
        let mut g = self.inner.lock().unwrap();
        let seq = g.next_seq;
        g.next_seq += 1;
        g.segments.push_back(TsSegment {
            seq,
            duration,
            data,
            discontinuity,
        });
        self.target_duration
            .fetch_max(duration.ceil() as u64, Ordering::Relaxed);
        // GC：window>0 时内存保留最近 window + publish_delay + grace 段。
        // publish_delay 是已生产但暂不发布的新段；grace 是已滚出发布窗口但仍兜底在途请求的旧段。
        // MEDIA-SEQUENCE 不在此累加，改由首个发布段的 seq 推导，恒与发布窗口一致。
        if self.window > 0 {
            let delay = self.publish_delay_segments.load(Ordering::Relaxed);
            let cap = self.window + delay + LIVE_GRACE_SEGMENTS;
            while g.segments.len() > cap {
                g.segments.pop_front();
            }
        }
        seq
    }

    /// 据发布窗口算出 playlist 的 [start, end) 与 MEDIA-SEQUENCE（首个发布段的 seq）。
    /// 内存里下标 < start 的是 grace 段；下标 >= end 的是 publish_delay 段。
    /// window==0（点播）时发布全部段，起始下标为 0。
    fn publish_range(&self, g: &Inner) -> (usize, usize, u64) {
        let total = g.segments.len();
        let end = if self.window > 0 {
            let delay = self.publish_delay_segments.load(Ordering::Relaxed);
            total.saturating_sub(delay.min(total))
        } else {
            total
        };
        let start = if self.window > 0 && end > self.window {
            end - self.window
        } else {
            0
        };
        let media_seq = g.segments.get(start).map(|s| s.seq).unwrap_or(g.next_seq);
        (start, end, media_seq)
    }

    /// 按需直播空闲停转时清掉旧窗口，避免下一次唤醒先暴露过期 live 段。
    /// next_seq 保持递增，所以对外 URL 不回退，播放器也不会看到旧 seq 复用。
    pub fn clear_live_segments_keep_sequence(&self) {
        if !self.is_live {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        g.segments.clear();
        g.finished = false;
    }

    /// 标记直播结束（写 ENDLIST）。
    pub fn finish(&self) {
        self.inner.lock().unwrap().finished = true;
    }

    /// 取某 seq 的段数据。
    pub fn get_segment(&self, seq: u64) -> Option<bytes::Bytes> {
        let g = self.inner.lock().unwrap();
        g.segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| s.data.clone())
    }

    /// 当前段数（用于状态展示）。
    pub fn segment_count(&self) -> usize {
        self.inner.lock().unwrap().segments.len()
    }

    pub fn total_produced(&self) -> u64 {
        self.inner.lock().unwrap().next_seq
    }

    /// 生成 media playlist，使用绝对 URL（http://host/p/<id>/s?n=0），
    /// 与参考站 https://cdn/api?h=xxx&u=xxx 格式一致。
    pub fn media_playlist_absolute(&self, prefix: &str) -> String {
        let g = self.inner.lock().unwrap();
        // 只发布稳定窗口；grace 段和 publish_delay 段都不写入 playlist。
        let (start, end, media_seq) = self.publish_range(&g);
        let mut s = String::new();
        s.push_str("#EXTM3U\n");
        s.push_str("#EXT-X-VERSION:3\n");
        s.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            (self.target_duration.load(Ordering::Relaxed)).max(1)
        ));
        s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_seq));
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
        if g.finished && !rolling {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    }

    /// 生成 media playlist (m3u8) 文本。
    pub fn media_playlist(&self) -> String {
        let g = self.inner.lock().unwrap();
        // 只发布稳定窗口；grace 段和 publish_delay 段都不写入 playlist。
        let (start, end, media_seq) = self.publish_range(&g);
        let mut s = String::new();
        s.push_str("#EXTM3U\n");
        s.push_str("#EXT-X-VERSION:3\n");
        s.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            (self.target_duration.load(Ordering::Relaxed)).max(1)
        ));
        s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_seq));
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
        if g.finished && !rolling {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    }

    /// 生成 master playlist，引用伪装后的 media playlist，带 CODECS 与 VIDEO-RANGE。
    pub fn master_playlist(&self, bandwidth: u64, width: u32, height: u32) -> String {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}\",RESOLUTION={}x{},VIDEO-RANGE={}\n{}/api\n",
            bandwidth, self.codecs, width, height, self.video_range, self.base_url
        )
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
        out.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}\",RESOLUTION={}x{},VIDEO-RANGE={}",
            bandwidth, self.codecs, width, height, self.video_range
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
            }),
            window,
            target_duration: AtomicU64::new(target_duration),
            publish_delay_segments: AtomicUsize::new(if is_live {
                publish_delay_segments.max(1)
            } else {
                0
            }),
            is_live,
        }
    }

    pub fn set_publish_delay_segments(&self, delay: usize) {
        let delay = if self.is_live { delay.max(1) } else { 0 };
        self.publish_delay_segments.store(delay, Ordering::Relaxed);
    }

    pub fn push_segment(&self, duration: f64, body: String) -> u64 {
        self.push_segment_inner(duration, body, false)
    }

    pub fn push_segment_discontinuity(&self, duration: f64, body: String) -> u64 {
        self.push_segment_inner(duration, body, true)
    }

    fn push_segment_inner(&self, duration: f64, body: String, discontinuity: bool) -> u64 {
        let mut g = self.inner.lock().unwrap();
        let seq = g.next_seq;
        g.next_seq += 1;
        g.segments.push_back(SubtitleSegment {
            seq,
            duration,
            body,
            discontinuity,
        });
        self.target_duration
            .fetch_max(duration.ceil() as u64, Ordering::Relaxed);
        if self.window > 0 {
            let delay = self.publish_delay_segments.load(Ordering::Relaxed);
            let cap = self.window + delay + LIVE_GRACE_SEGMENTS;
            while g.segments.len() > cap {
                g.segments.pop_front();
            }
        }
        seq
    }

    fn publish_range(&self, g: &SubtitleInner) -> (usize, usize, u64) {
        let total = g.segments.len();
        let end = if self.window > 0 {
            let delay = self.publish_delay_segments.load(Ordering::Relaxed);
            total.saturating_sub(delay.min(total))
        } else {
            total
        };
        let start = if self.window > 0 && end > self.window {
            end - self.window
        } else {
            0
        };
        let media_seq = g.segments.get(start).map(|s| s.seq).unwrap_or(g.next_seq);
        (start, end, media_seq)
    }

    pub fn playlist_absolute(&self, prefix: &str) -> String {
        let g = self.inner.lock().unwrap();
        let (start, end, media_seq) = self.publish_range(&g);
        let rolling = self.window > 0;
        let mut s = String::new();
        s.push_str("#EXTM3U\n");
        s.push_str("#EXT-X-VERSION:3\n");
        s.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.target_duration.load(Ordering::Relaxed).max(1)
        ));
        s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_seq));
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
        if g.finished && !rolling {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    }

    pub fn get_segment(&self, seq: u64) -> Option<String> {
        let g = self.inner.lock().unwrap();
        g.segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| format_webvtt_segment(&s.body))
    }

    pub fn segment_count(&self) -> usize {
        self.inner.lock().unwrap().segments.len()
    }

    pub fn clear_live_segments_keep_sequence(&self) {
        if !self.is_live {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        g.segments.clear();
        g.finished = false;
    }

    pub fn finish(&self) {
        self.inner.lock().unwrap().finished = true;
    }
}

fn format_webvtt_segment(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        "WEBVTT\n\n".to_string()
    } else if body.trim_start_matches('\u{feff}').starts_with("WEBVTT") {
        format!("{body}\n")
    } else {
        format!("WEBVTT\n\n{body}\n")
    }
}

fn hls_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::{HlsOutput, SubtitleOutput, SubtitleRendition};

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
            hls.push_segment(4.0, bytes::Bytes::from_static(b"ts"));
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
            hls.push_segment(4.0, bytes::Bytes::from_static(b"ts"));
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
        hls.push_segment(4.0, bytes::Bytes::from_static(b"a"));
        hls.push_segment_discontinuity(4.0, bytes::Bytes::from_static(b"b"));

        let playlist = hls.media_playlist();
        let marker = playlist.find("#EXT-X-DISCONTINUITY").unwrap();
        let segment = playlist.find("picture-1.jpeg").unwrap();
        assert!(marker < segment);
    }

    #[test]
    fn clear_live_segments_keeps_sequence_moving_forward() {
        let hls = output(6, 1);
        hls.push_segment(4.0, bytes::Bytes::from_static(b"a"));
        hls.push_segment(4.0, bytes::Bytes::from_static(b"b"));
        hls.clear_live_segments_keep_sequence();

        let empty_playlist = hls.media_playlist();
        assert!(empty_playlist.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert!(!empty_playlist.contains("picture-0.jpeg"));

        hls.push_segment(4.0, bytes::Bytes::from_static(b"c"));
        hls.push_segment(4.0, bytes::Bytes::from_static(b"d"));
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
    fn subtitle_playlist_uses_txt_segments() {
        let subtitles = SubtitleOutput::new(6, 4, true, 1);
        subtitles.push_segment(4.0, "00:00:00.000 --> 00:00:01.000\nhello".into());
        subtitles.push_segment(4.0, "00:00:01.000 --> 00:00:02.000\nworld".into());

        let playlist = subtitles.playlist_absolute("http://127.0.0.1:37201/p/task");

        assert!(playlist.contains("http://127.0.0.1:37201/p/task/xyz-0.txt"));
        assert!(!playlist.contains("subtitle-"));
        assert!(!playlist.contains(".vtt"));
    }
}
