//! 任务管理器：持有所有任务（DashMap），管理状态与取消。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
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
}

impl KeyMode {
    pub fn is_dynamic(self) -> bool {
        matches!(self, KeyMode::Dynamic)
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
        let result = dl.get(url).await;
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
    tasks: Arc<DashMap<String, Arc<Task>>>,
    pub downloader: SharedDownloader,
}

impl TaskManager {
    pub fn new(downloader: SharedDownloader) -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
            downloader,
        }
    }

    pub fn insert(&self, task: Arc<Task>) {
        self.tasks.insert(task.id.clone(), task);
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.get(id).map(|t| t.clone())
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.remove(id).map(|(_, t)| t)
    }

    pub fn spawn_task_pipeline(&self, task: Arc<Task>, stage: &str) {
        *task.cancel.lock() = CancellationToken::new();
        task.set_paused(false);
        task.on_demand_auto_paused.store(false, Ordering::Relaxed);
        if task.run_mode.is_on_demand() {
            task.mark_playback_request();
        }
        task.set_state(TaskState::Running);
        task.set_stage(stage);
        let dl = self.downloader.clone();
        match task.source_kind {
            SourceKind::Mpd => {
                tokio::spawn(crate::task::pipeline::run_pipeline(task, dl));
            }
            SourceKind::Hls => {
                tokio::spawn(crate::task::hls_pipeline::run_hls_pipeline(task, dl));
            }
        }
    }

    pub fn arm_on_demand_task(&self, id: &str) {
        if let Some(t) = self.get(id) {
            t.set_paused(true);
            t.on_demand_auto_paused.store(true, Ordering::Relaxed);
            t.set_state(TaskState::Paused);
            t.set_stage("等待播放请求");
        }
        self.spawn_on_demand_idle_watcher(id.to_string());
    }

    pub fn mark_playback_request(&self, id: &str) -> Option<Arc<Task>> {
        let task = self.get(id)?;
        task.mark_playback_request();
        self.start_on_demand_if_needed(task.clone());
        Some(task)
    }

    fn start_on_demand_if_needed(&self, task: Arc<Task>) {
        if !task.run_mode.is_on_demand() {
            return;
        }
        if !matches!(task.state(), TaskState::Paused | TaskState::Stopped) {
            return;
        }
        if task
            .on_demand_auto_paused
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        tracing::info!(task = %task.id, "on-demand playback request woke task");
        self.spawn_task_pipeline(task, "按需请求唤醒");
    }

    fn spawn_on_demand_idle_watcher(&self, id: String) {
        let mgr = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(ON_DEMAND_IDLE_CHECK_SECS)).await;
                let Some(task) = mgr.get(&id) else {
                    break;
                };
                if !task.run_mode.is_on_demand() {
                    break;
                }
                if matches!(task.state(), TaskState::Finished | TaskState::Error) {
                    break;
                }
                if !matches!(task.state(), TaskState::Running | TaskState::Parsing) {
                    continue;
                }
                let idle_secs = task.playback_idle_secs();
                if idle_secs < task.idle_timeout_secs {
                    continue;
                }
                task.on_demand_auto_paused.store(true, Ordering::Relaxed);
                task.set_paused(true);
                task.set_stage("按需空闲暂停");
                task.cancel.lock().cancel();
                if task.is_dynamic {
                    task.hls.clear_live_segments_keep_sequence();
                    task.subtitles.clear_live_segments_keep_sequence();
                }
                tracing::info!(
                    task = %task.id,
                    idle_secs,
                    idle_timeout_secs = task.idle_timeout_secs,
                    "auto-stopping idle on-demand session"
                );
            }
        });
    }

    /// 停止任务：取消流水线，保留已产段（可重新启动）。
    pub fn stop(&self, id: &str) -> bool {
        if let Some(t) = self.get(id) {
            t.set_paused(false);
            t.on_demand_auto_paused.store(false, Ordering::Relaxed);
            t.cancel.lock().cancel();
            true
        } else {
            false
        }
    }

    /// 暂停任务：取消流水线但标记 paused，保留进度（可恢复）。
    pub fn pause(&self, id: &str) -> bool {
        if let Some(t) = self.get(id) {
            if matches!(t.state(), TaskState::Running | TaskState::Parsing) {
                t.set_paused(true);
                t.on_demand_auto_paused.store(false, Ordering::Relaxed);
                t.cancel.lock().cancel();
                return true;
            }
        }
        false
    }

    /// 启动/恢复任务：换新取消令牌并重新 spawn 流水线，从 resume_number 继续。
    pub fn start(&self, id: &str) -> bool {
        if let Some(t) = self.get(id) {
            // 仅当未在运行时才启动
            if matches!(t.state(), TaskState::Running | TaskState::Parsing) {
                return false;
            }
            self.spawn_task_pipeline(t, "恢复中");
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<TaskStatus> {
        self.tasks
            .iter()
            .map(|e| {
                let t = e.value();
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
                    segments_available: t.hls.segment_count(),
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
