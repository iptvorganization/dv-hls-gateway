//! 任务管理器：持有所有任务（DashMap），管理状态与取消。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::{mapref::entry::Entry, DashMap};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::fetch::{DownloadError, SharedDownloader};
use crate::segment::{HlsOutput, SubtitleOutput};

use super::fetch_tuning::AdaptiveFetchTuning;

const DEFAULT_ON_DEMAND_IDLE_TIMEOUT_SECS: u64 = 300;
const ON_DEMAND_IDLE_CHECK_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Parsing,
    Running,
    Paused,
    Stopped,
    Error,
    Finished,
}

impl TaskState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => TaskState::Parsing,
            1 => TaskState::Running,
            2 => TaskState::Stopped,
            3 => TaskState::Error,
            4 => TaskState::Finished,
            _ => TaskState::Paused,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            TaskState::Parsing => 0,
            TaskState::Running => 1,
            TaskState::Stopped => 2,
            TaskState::Error => 3,
            TaskState::Finished => 4,
            TaskState::Paused => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Mpd,
    Hls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Always,
    OnDemand,
}

impl RunMode {
    pub fn is_on_demand(self) -> bool {
        matches!(self, RunMode::OnDemand)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeyMode {
    #[default]
    Static,
    Dynamic,
    Clear,
}

impl KeyMode {
    pub fn is_dynamic(self) -> bool {
        matches!(self, KeyMode::Dynamic)
    }

    pub fn is_clear(self) -> bool {
        matches!(self, KeyMode::Clear)
    }
}

/// 用户选定的轨道。
#[derive(Debug, Clone)]
pub struct TrackSelection {
    pub mpd_url: String,
    pub source_kind: SourceKind,
    /// 多行 "KID:KEY"，按 KID 自动匹配视频/音频轨道。
    pub keys: String,
    pub key_mode: KeyMode,
    pub video_rep_id: String,
    pub audio_rep_id: Option<String>,
    pub enable_subtitles: bool,
    pub subtitle_rep_id: Option<String>,
}

/// 单个任务运行时句柄。
pub struct Task {
    pub id: String,
    pub name: String,
    pub run_mode: RunMode,
    pub mpd_url: String,
    pub video_rep_id: String,
    pub audio_rep_id: Option<String>,
    pub subtitles_enabled: bool,
    pub subtitle_rep_id: Option<String>,
    pub subtitle_lang: String,
    pub source_kind: SourceKind,
    pub codecs: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    /// 是否为动态 MPD（直播）。true 时 total_segments 会动态更新。
    pub is_dynamic: bool,
    pub state: AtomicU8,
    pub error_msg: parking_lot_lite::Mutex<String>,
    /// 当前阶段描述（如 "解析 MPD"、"匹配 KEY"、"转封装中"）。
    pub stage: parking_lot_lite::Mutex<String>,
    pub segments_done: AtomicU64,
    /// 累计已下载字节（用于速度展示）。
    pub bytes_done: AtomicU64,
    /// 任务总段数（已知时）。
    pub total_segments: AtomicU64,
    /// 下一个要处理的视频段号（停止/暂停后重启时从这里继续）。
    pub resume_number: AtomicU64,
    /// 累计的视频 DTS（源 timescale），tfdt 缺失时的回退累加器；重启时延续以保持时间戳连续。
    pub acc_v_dts: AtomicU64,
    pub acc_a_dts: AtomicU64,
    /// 时间线锚点：任务首段视频/音频的 tfdt baseMediaDecodeTime（源 timescale）。
    /// 之后每段时间戳 = (本段 tfdt - origin)，使视频/音频都锚定源绝对时基、消除累加漂移
    /// 并保留源固有 A/V offset。`u64::MAX` 表示尚未设置。持久化以保证停止/续传后锚点不变。
    pub origin_v_tfdt: AtomicU64,
    pub origin_a_tfdt: AtomicU64,
    pub hls: Arc<HlsOutput>,
    pub subtitles: Arc<SubtitleOutput>,
    /// 每任务自适应并发状态：失败/慢请求降并发，连续快请求恢复。
    pub fetch_tuning: AdaptiveFetchTuning,
    /// 当前取消令牌（停止/重启时会被替换为新的）。
    pub cancel: parking_lot_lite::Mutex<CancellationToken>,
    /// 暂停标志：流水线在每段循环检查，为 true 时挂起拉流但保留进度。
    pub paused: AtomicBool,
    /// 按需模式下最近一次播放器访问 playlist/segment 的时间；0 表示尚无播放请求。
    pub last_playback_request_secs: AtomicU64,
    /// 按需模式空闲超时时间。
    pub idle_timeout_secs: u64,
    /// true 表示任务是因为按需空闲/等待请求而暂停，可被播放请求自动唤醒。
    pub on_demand_auto_paused: AtomicBool,
    /// 保存的轨道选择与配置，用于停止后重启。
    pub sel: TrackSelection,
}

impl Task {
    pub fn state(&self) -> TaskState {
        TaskState::from_u8(self.state.load(Ordering::Relaxed))
    }
    pub fn set_state(&self, s: TaskState) {
        self.state.store(s.as_u8(), Ordering::Relaxed);
    }
    pub fn set_stage(&self, s: impl Into<String>) {
        *self.stage.lock() = s.into();
    }
    /// 取当前取消令牌的克隆（流水线用它监听取消）。
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.lock().clone()
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    pub fn set_paused(&self, v: bool) {
        self.paused.store(v, Ordering::Relaxed);
    }
    pub fn mark_playback_request(&self) {
        self.last_playback_request_secs
            .store(now_secs(), Ordering::Relaxed);
    }
    pub fn playback_idle_secs(&self) -> u64 {
        let last = self.last_playback_request_secs.load(Ordering::Relaxed);
        if last == 0 {
            0
        } else {
            now_secs().saturating_sub(last)
        }
    }
    pub fn is_on_demand_auto_paused(&self) -> bool {
        self.on_demand_auto_paused.load(Ordering::Relaxed)
    }

    fn discard_incomplete_vod(&self) {
        self.hls.clear_segments_keep_sequence();
        self.subtitles.clear_segments_keep_sequence();
        self.resume_number.store(0, Ordering::Relaxed);
        self.acc_v_dts.store(0, Ordering::Relaxed);
        self.acc_a_dts.store(0, Ordering::Relaxed);
        self.origin_v_tfdt.store(u64::MAX, Ordering::Relaxed);
        self.origin_a_tfdt.store(u64::MAX, Ordering::Relaxed);
        self.segments_done.store(0, Ordering::Relaxed);
    }
    pub fn configure_fetch_tuning(&self, short_source: bool) {
        self.fetch_tuning.configure_source(&self.id, short_source);
    }
    pub fn segment_fetch_concurrency(&self) -> usize {
        self.fetch_tuning.segment_fetch_concurrency()
    }
    pub async fn fetch_media(
        &self,
        dl: &SharedDownloader,
        url: &str,
    ) -> crate::Result<bytes::Bytes> {
        let cancel = self.cancel_token();
        let _permit = self.fetch_tuning.acquire_shared(&cancel).await?;
        let started = Instant::now();
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow::anyhow!("media download cancelled")),
            result = dl.get(url) => result,
        };
        let elapsed = started.elapsed();
        if result.is_ok() {
            self.fetch_tuning.record_success(&self.id, elapsed);
        } else {
            let download_error = result
                .as_ref()
                .err()
                .and_then(|err| err.downcast_ref::<DownloadError>());
            if download_error
                .map(|err| err.is_concurrency_pressure())
                .unwrap_or(false)
            {
                self.fetch_tuning.record_failure(&self.id, elapsed);
            } else {
                let kind = download_error
                    .map(|err| err.kind().to_string())
                    .unwrap_or_else(|| "unclassified".to_string());
                self.fetch_tuning
                    .record_ignored_failure(&self.id, elapsed, &kind);
            }
        }
        result
    }
}

/// 对前端的状态视图。
#[derive(Debug, Clone, Serialize)]
pub struct TaskStatus {
    pub id: String,
    pub name: String,
    pub run_mode: RunMode,
    pub state: TaskState,
    pub stage: String,
    pub is_dynamic: bool,
    pub mpd_url: String,
    pub video_rep_id: String,
    pub audio_rep_id: Option<String>,
    pub subtitles_enabled: bool,
    pub subtitle_rep_id: Option<String>,
    pub subtitle_lang: String,
    pub source_kind: SourceKind,
    pub codecs: String,
    pub resolution: String,
    pub segments_done: u64,
    pub total_segments: u64,
    pub segments_available: usize,
    pub mb_done: f64,
    pub error: String,
    pub playlist_url: String,
    pub idle_secs: u64,
    pub idle_timeout_secs: u64,
}

#[derive(Clone)]
pub struct TaskManager {
    tasks: Arc<DashMap<String, TaskSlot>>,
    pub downloader: SharedDownloader,
}

struct TaskSlot {
    task: Arc<Task>,
    // Protected by the DashMap entry lock. Only the supervisor clears it,
    // after the worker has joined, so a stop/resume cannot overlap workers.
    running: bool,
}

impl TaskManager {
    pub fn new(downloader: SharedDownloader) -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
            downloader,
        }
    }

    pub fn add_task(&self, task: Arc<Task>) {
        let id = task.id.clone();
        let on_demand = task.run_mode.is_on_demand();
        let Entry::Vacant(entry) = self.tasks.entry(id.clone()) else {
            tracing::error!(task = %id, "duplicate task id");
            return;
        };
        let mut slot = entry.insert(TaskSlot {
            task,
            running: false,
        });
        if on_demand {
            slot.task.set_paused(true);
            slot.task
                .on_demand_auto_paused
                .store(true, Ordering::Relaxed);
            slot.task.set_stage("等待播放请求");
            slot.task.set_state(TaskState::Paused);
        } else {
            self.spawn_locked(&mut slot, "启动中");
        }
        drop(slot);
        if on_demand {
            self.spawn_on_demand_idle_watcher(id);
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.get(id).map(|slot| slot.task.clone())
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Task>> {
        let (_, slot) = self.tasks.remove(id)?;
        slot.task.cancel.lock().cancel();
        Some(slot.task)
    }

    fn spawn_locked(&self, slot: &mut TaskSlot, stage: &str) {
        debug_assert!(!slot.running);
        slot.running = true;
        let task = slot.task.clone();
        let cancel = CancellationToken::new();
        *task.cancel.lock() = cancel.clone();
        task.set_paused(false);
        task.on_demand_auto_paused.store(false, Ordering::Relaxed);
        if task.run_mode.is_on_demand() {
            task.mark_playback_request();
        }
        task.hls.mark_next_discontinuity();
        task.error_msg.lock().clear();
        task.set_stage(stage);
        task.set_state(TaskState::Running);
        let mgr = self.clone();
        let dl = self.downloader.clone();
        let id = task.id.clone();
        tokio::spawn(async move {
            let mut worker = tokio::spawn(async move {
                match task.source_kind {
                    SourceKind::Mpd => super::pipeline::run_pipeline(task, dl).await,
                    SourceKind::Hls => super::hls_pipeline::run_hls_pipeline(task, dl).await,
                }
            });
            let result = tokio::select! {
                result = &mut worker => result,
                _ = cancel.cancelled() => {
                    // Give the pipeline a chance to flush already validated output.
                    match tokio::time::timeout(Duration::from_secs(2), &mut worker).await {
                        Ok(result) => result,
                        Err(_) => {
                            worker.abort();
                            worker.await
                        }
                    }
                }
            };
            mgr.worker_finished(&id, result, cancel.is_cancelled());
        });
    }

    fn worker_finished(
        &self,
        id: &str,
        result: Result<crate::Result<()>, tokio::task::JoinError>,
        cancelled: bool,
    ) {
        let Some(mut slot) = self.tasks.get_mut(id) else {
            return;
        };
        slot.running = false;
        let task = slot.task.clone();
        // Recheck under the same lock as pause/stop so a late cancel cannot
        // be lost between JoinHandle completion and the terminal transition.
        let cancelled = cancelled || task.cancel_token().is_cancelled();
        if cancelled {
            // An aborted worker may have advanced its input cursor while an
            // aggregate was still private. Do not resume a VOD past that data.
            if !task.is_dynamic && !matches!(&result, Ok(Ok(()))) {
                task.discard_incomplete_vod();
            }
            if task.is_paused() {
                let auto = task.is_on_demand_auto_paused();
                if auto && task.is_dynamic {
                    task.hls.clear_live_segments_keep_sequence();
                    task.subtitles.clear_live_segments_keep_sequence();
                }
                task.set_stage(if auto {
                    "按需空闲暂停"
                } else {
                    "已暂停"
                });
                task.set_state(TaskState::Paused);
                if auto && task.playback_idle_secs() < task.idle_timeout_secs {
                    self.spawn_locked(&mut slot, "按需请求唤醒");
                }
            } else {
                task.set_stage("已停止");
                task.set_state(TaskState::Stopped);
            }
            return;
        }
        match result {
            Ok(Ok(())) => {
                task.hls
                    .finish_pair(task.subtitles_enabled.then_some(&task.subtitles));
                task.set_stage("已完成");
                task.set_state(TaskState::Finished);
            }
            result => {
                let error = match result {
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => format!("pipeline worker terminated: {e}"),
                    Ok(Ok(())) => unreachable!(),
                };
                tracing::error!(task = %task.id, %error, "pipeline failed");
                if !task.is_dynamic {
                    task.discard_incomplete_vod();
                }
                *task.error_msg.lock() = error;
                task.set_stage("出错");
                task.set_state(TaskState::Error);
            }
        }
    }

    pub fn mark_playback_request(&self, id: &str) -> Option<Arc<Task>> {
        let mut slot = self.tasks.get_mut(id)?;
        let task = slot.task.clone();
        task.mark_playback_request();
        if !slot.running
            && task.run_mode.is_on_demand()
            && task.is_on_demand_auto_paused()
            && matches!(task.state(), TaskState::Paused | TaskState::Stopped)
        {
            self.spawn_locked(&mut slot, "按需请求唤醒");
        }
        Some(task)
    }

    fn spawn_on_demand_idle_watcher(&self, id: String) {
        let mgr = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(ON_DEMAND_IDLE_CHECK_SECS)).await;
                let Some(slot) = mgr.tasks.get_mut(&id) else {
                    break;
                };
                let task = &slot.task;
                if !slot.running || task.cancel_token().is_cancelled() {
                    continue;
                }
                let idle_secs = task.playback_idle_secs();
                if idle_secs < task.idle_timeout_secs {
                    continue;
                }
                task.on_demand_auto_paused.store(true, Ordering::Relaxed);
                task.set_paused(true);
                task.set_stage("正在按需暂停");
                task.cancel.lock().cancel();
                tracing::info!(task = %task.id, idle_secs, "stopping idle on-demand session");
            }
        });
    }

    /// Returns None for an unknown task, or whether the worker is still exiting.
    pub fn stop(&self, id: &str) -> Option<bool> {
        let slot = self.tasks.get_mut(id)?;
        slot.task.set_paused(false);
        slot.task
            .on_demand_auto_paused
            .store(false, Ordering::Relaxed);
        if slot.running {
            slot.task.set_stage("正在停止");
            slot.task.cancel.lock().cancel();
        } else if slot.task.state() != TaskState::Finished {
            slot.task.set_stage("已停止");
            slot.task.set_state(TaskState::Stopped);
        }
        Some(slot.running)
    }

    pub fn pause(&self, id: &str) -> bool {
        let Some(slot) = self.tasks.get_mut(id) else {
            return false;
        };
        if !slot.running || slot.task.cancel_token().is_cancelled() {
            return false;
        }
        slot.task.set_paused(true);
        slot.task
            .on_demand_auto_paused
            .store(false, Ordering::Relaxed);
        slot.task.set_stage("正在暂停");
        slot.task.cancel.lock().cancel();
        true
    }

    pub fn start(&self, id: &str) -> bool {
        let Some(mut slot) = self.tasks.get_mut(id) else {
            return false;
        };
        if slot.running || slot.task.state() == TaskState::Finished {
            return false;
        }
        self.spawn_locked(&mut slot, "恢复中");
        true
    }

    pub fn list(&self) -> Vec<TaskStatus> {
        self.tasks
            .iter()
            .map(|e| {
                let t = &e.value().task;
                TaskStatus {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    run_mode: t.run_mode,
                    state: t.state(),
                    stage: t.stage.lock().clone(),
                    is_dynamic: t.is_dynamic,
                    mpd_url: t.mpd_url.clone(),
                    video_rep_id: t.video_rep_id.clone(),
                    audio_rep_id: t.audio_rep_id.clone(),
                    subtitles_enabled: t.subtitles_enabled,
                    subtitle_rep_id: t.subtitle_rep_id.clone(),
                    subtitle_lang: t.subtitle_lang.clone(),
                    source_kind: t.source_kind,
                    codecs: t.codecs.clone(),
                    resolution: format!("{}x{}", t.width, t.height),
                    segments_done: t.segments_done.load(Ordering::Relaxed),
                    total_segments: t.total_segments.load(Ordering::Relaxed),
                    segments_available: t.hls.published_segment_count(),
                    mb_done: t.bytes_done.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    error: t.error_msg.lock().clone(),
                    playlist_url: format!("/p/{}", t.id),
                    idle_secs: t.playback_idle_secs(),
                    idle_timeout_secs: t.idle_timeout_secs,
                }
            })
            .collect()
    }
}

pub fn on_demand_idle_timeout_secs() -> u64 {
    env_u64("MPD_HLS_ON_DEMAND_IDLE_TIMEOUT_SECS")
        .unwrap_or(DEFAULT_ON_DEMAND_IDLE_TIMEOUT_SECS)
        .max(30)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// 极简 Mutex（避免引入 parking_lot 依赖；用 std）。
pub mod parking_lot_lite {
    use std::sync::Mutex as StdMutex;
    pub struct Mutex<T>(StdMutex<T>);
    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self {
            Self(StdMutex::new(v))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str, url: String) -> Arc<Task> {
        let sel = TrackSelection {
            mpd_url: url.clone(),
            source_kind: SourceKind::Hls,
            keys: String::new(),
            key_mode: KeyMode::Clear,
            video_rep_id: "hls-0".into(),
            audio_rep_id: None,
            enable_subtitles: false,
            subtitle_rep_id: None,
        };
        Arc::new(Task {
            id: id.into(),
            name: id.into(),
            run_mode: RunMode::OnDemand,
            mpd_url: url,
            video_rep_id: sel.video_rep_id.clone(),
            audio_rep_id: None,
            subtitles_enabled: false,
            subtitle_rep_id: None,
            subtitle_lang: String::new(),
            source_kind: SourceKind::Hls,
            codecs: "avc1".into(),
            width: 128,
            height: 72,
            bandwidth: 1000,
            is_dynamic: true,
            state: AtomicU8::new(0),
            error_msg: parking_lot_lite::Mutex::new(String::new()),
            stage: parking_lot_lite::Mutex::new(String::new()),
            segments_done: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            total_segments: AtomicU64::new(0),
            resume_number: AtomicU64::new(0),
            acc_v_dts: AtomicU64::new(0),
            acc_a_dts: AtomicU64::new(0),
            origin_v_tfdt: AtomicU64::new(u64::MAX),
            origin_a_tfdt: AtomicU64::new(u64::MAX),
            hls: Arc::new(HlsOutput::new(
                6,
                4,
                "avc1".into(),
                "SDR".into(),
                true,
                "/p/test".into(),
                1,
            )),
            subtitles: Arc::new(SubtitleOutput::new(6, 4, true, 1)),
            fetch_tuning: AdaptiveFetchTuning::new(),
            cancel: parking_lot_lite::Mutex::new(CancellationToken::new()),
            paused: AtomicBool::new(false),
            last_playback_request_secs: AtomicU64::new(0),
            idle_timeout_secs: 300,
            on_demand_auto_paused: AtomicBool::new(false),
            sel,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_start_and_play_requests_do_not_replace_worker() {
        // A local listener that doesn't serve the manifest holds the first
        // worker during the race. No external endpoint or account is needed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/live.m3u8", listener.local_addr().unwrap());
        let mgr = TaskManager::new(Arc::new(crate::fetch::Downloader::new()));
        let task = fixture("race", url);
        mgr.add_task(task.clone());
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let mut calls = Vec::new();
        for _ in 0..16 {
            let mgr = mgr.clone();
            let barrier = barrier.clone();
            calls.push(tokio::spawn(async move {
                barrier.wait().await;
                mgr.start("race")
            }));
        }
        barrier.wait().await;
        let mut accepted = 0;
        for call in calls {
            accepted += usize::from(call.await.unwrap());
        }
        assert_eq!(accepted, 1);
        let token = task.cancel_token();
        for _ in 0..8 {
            assert!(mgr.mark_playback_request("race").is_some());
            assert!(!mgr.start("race"));
        }
        assert!(mgr.pause("race"));
        assert!(token.is_cancelled());
        // No replacement until the active worker has fully exited.
        assert!(!mgr.start("race"));
        tokio::time::timeout(Duration::from_secs(4), async {
            while task.state() != TaskState::Paused {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(mgr.start("race"));
        assert!(!task.cancel_token().is_cancelled());
        mgr.remove("race");
    }

    #[tokio::test]
    async fn worker_panic_is_reflected_in_task_state() {
        let mgr = TaskManager::new(Arc::new(crate::fetch::Downloader::new()));
        let task = fixture("panic", "http://127.0.0.1:1/local".into());
        mgr.add_task(task.clone());
        {
            let mut slot = mgr.tasks.get_mut("panic").unwrap();
            slot.running = true;
            task.set_state(TaskState::Running);
        }
        let failure = tokio::spawn(async { panic!("synthetic worker test") }).await;
        mgr.worker_finished("panic", failure, false);
        assert_eq!(task.state(), TaskState::Error);
        assert!(task.error_msg.lock().contains("pipeline worker terminated"));
        assert!(!mgr.tasks.get("panic").unwrap().running);
    }
}
