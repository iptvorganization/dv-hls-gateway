//! 伪装播放输出：playlist 无后缀 + TS 分片伪装 .jpeg。
//! 启用字幕时，master 只暴露 /api 和 /xyz，字幕分片伪装为 xyz-*.txt。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::segment::SubtitleRendition;
use crate::task::TaskState;

use super::AppState;

const M3U8_CT: &str = "application/vnd.apple.mpegurl";
const JPEG_CT: &str = "image/jpeg";
const TXT_CT: &str = "text/plain; charset=utf-8";
const ON_DEMAND_INITIAL_WAIT_MS: u64 = 4_000;
const ON_DEMAND_INITIAL_WAIT_STEP_MS: u64 = 200;

fn host_prefix(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

#[derive(Deserialize)]
pub struct PlaylistQuery {
    /// CDN/反代场景：?url=https://cdn.example.com/p/<id> 覆盖段 URL 的 scheme+host+路径前缀
    url: Option<String>,
}

pub async fn master(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PlaylistQuery>,
) -> impl IntoResponse {
    match st.mgr.mark_playback_request(&id) {
        Some(t) => {
            wait_for_on_demand_initial_output(&t).await;
            let prefix = match &q.url {
                // 从 ?url=https://cdn.example.com/p/<id> 提取前缀（去掉尾部 playlist id 本身，保留 /p/<id> 路径）
                Some(u) => {
                    let u = u.trim_end_matches('/');
                    u.to_string()
                }
                None => format!("{}/p/{id}", host_prefix(&headers)),
            };
            let body = if t.subtitles_enabled {
                t.hls.master_playlist_absolute(
                    &prefix,
                    t.bandwidth,
                    t.width,
                    t.height,
                    Some(SubtitleRendition {
                        name: if t.subtitle_lang.is_empty() {
                            "Subtitles"
                        } else {
                            &t.subtitle_lang
                        },
                        lang: if t.subtitle_lang.is_empty() {
                            "und"
                        } else {
                            &t.subtitle_lang
                        },
                    }),
                )
            } else {
                t.hls.media_playlist_absolute(&prefix)
            };
            (
                [
                    (header::CONTENT_TYPE, M3U8_CT),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "no task").into_response(),
    }
}

pub async fn media(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PlaylistQuery>,
) -> impl IntoResponse {
    match st.mgr.mark_playback_request(&id) {
        Some(t) => {
            wait_for_on_demand_initial_output(&t).await;
            let prefix = match &q.url {
                Some(u) => u.trim_end_matches('/').to_string(),
                None => format!("{}/p/{id}", host_prefix(&headers)),
            };
            let body = t.hls.media_playlist_absolute(&prefix);
            (
                [
                    (header::CONTENT_TYPE, M3U8_CT),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "no task").into_response(),
    }
}

pub async fn subtitles(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PlaylistQuery>,
) -> impl IntoResponse {
    match st.mgr.mark_playback_request(&id) {
        Some(t) if t.subtitles_enabled => {
            wait_for_on_demand_initial_output(&t).await;
            let prefix = match &q.url {
                Some(u) => u.trim_end_matches('/').to_string(),
                None => format!("{}/p/{id}", host_prefix(&headers)),
            };
            let body = t.subtitles.playlist_absolute(&prefix);
            (
                [
                    (header::CONTENT_TYPE, M3U8_CT),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                body,
            )
                .into_response()
        }
        Some(_) => (StatusCode::NOT_FOUND, "subtitles disabled").into_response(),
        None => (StatusCode::NOT_FOUND, "no task").into_response(),
    }
}

async fn wait_for_on_demand_initial_output(t: &crate::task::manager::Task) {
    if !t.run_mode.is_on_demand() || t.hls.segment_count() > 0 {
        return;
    }
    if !matches!(t.state(), TaskState::Running | TaskState::Parsing) {
        return;
    }
    let mut waited = 0;
    while waited < ON_DEMAND_INITIAL_WAIT_MS {
        tokio::time::sleep(std::time::Duration::from_millis(
            ON_DEMAND_INITIAL_WAIT_STEP_MS,
        ))
        .await;
        waited += ON_DEMAND_INITIAL_WAIT_STEP_MS;
        if t.hls.segment_count() > 0
            || !matches!(t.state(), TaskState::Running | TaskState::Parsing)
        {
            break;
        }
    }
}

#[derive(Deserialize)]
pub struct SegPath {
    id: String,
    filename: String,
}

pub async fn segment(State(st): State<Arc<AppState>>, Path(p): Path<SegPath>) -> impl IntoResponse {
    // URL 形如 /p/<id>/picture-<seq>.jpeg 或 /p/<id>/xyz-<seq>.txt。
    let seq = p
        .filename
        .strip_prefix("picture-")
        .and_then(|s| s.strip_suffix(".jpeg"))
        .and_then(|n| n.parse::<u64>().ok());
    let subtitle_seq = p
        .filename
        .strip_prefix("xyz-")
        .and_then(|s| s.strip_suffix(".txt"))
        .and_then(|n| n.parse::<u64>().ok());
    match (st.mgr.mark_playback_request(&p.id), seq, subtitle_seq) {
        (Some(t), Some(seq), _) => match t.hls.get_segment(seq) {
            Some(data) => ([(header::CONTENT_TYPE, JPEG_CT)], data).into_response(),
            None => (StatusCode::NOT_FOUND, "gone").into_response(),
        },
        (Some(t), _, Some(seq)) if t.subtitles_enabled => match t.subtitles.get_segment(seq) {
            Some(data) => ([(header::CONTENT_TYPE, TXT_CT)], data).into_response(),
            None => (StatusCode::NOT_FOUND, "gone").into_response(),
        },
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
