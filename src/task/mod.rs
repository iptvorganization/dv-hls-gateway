//! 任务管理：生命周期、状态机、每任务流水线编排。

mod audio;
pub mod fetch_tuning;
pub mod hls_pipeline;
pub mod key_resolver;
pub mod live_tuning;
pub mod manager;
mod output_codecs;
pub mod pipeline;

pub use manager::{
    KeyMode, RunMode, SourceKind, TaskManager, TaskState, TaskStatus, TrackSelection,
};
