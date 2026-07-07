//! axum HTTP 服务：前端单页、任务 API、HLS m3u8/ts 输出。

pub mod api;
pub mod hls;

use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;

use crate::config;
use crate::task::TaskManager;

#[derive(Clone)]
pub struct AppState {
    pub mgr: TaskManager,
}

/// 仅对 /api/* 鉴权；首页与 /hls/*（播放器拉流，无法带头）放行。
async fn auth_mw(req: Request, next: Next) -> Result<Response, StatusCode> {
    let auth_key = config::get().auth.effective_key();
    if auth_key.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let ok = req
        .headers()
        .get("x-auth-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == auth_key)
        .unwrap_or(false)
        // 兼容 query ?key=，便于调试
        || req
            .uri()
            .query()
            .map(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .any(|(k, v)| k == "key" && v == auth_key)
            })
            .unwrap_or(false);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub fn build_router(mgr: TaskManager) -> Router {
    let state = Arc::new(AppState { mgr });

    // 受保护的 API 路由（需鉴权）
    let api = Router::new()
        .route("/parse", post(api::parse))
        .route("/tasks", get(api::list_tasks).post(api::create_task))
        .route("/tasks/:id/stop", post(api::stop_task))
        .route("/tasks/:id/pause", post(api::pause_task))
        .route("/tasks/:id/start", post(api::start_task))
        .route("/tasks/:id", axum::routing::delete(api::delete_task))
        .layer(middleware::from_fn(auth_mw))
        .with_state(state.clone());

    // 公开路由（首页 + 伪装播放输出，不鉴权）
    // /p/<id>        — media playlist，或启用字幕时的 master playlist
    // /p/<id>/api    — 启用字幕时的 media playlist
    // /p/<id>/xyz    — 启用字幕时的 subtitle playlist
    Router::new()
        .route("/", get(api::index))
        .route("/p/:id", get(hls::master))
        .route("/p/:id/api", get(hls::media))
        .route("/p/:id/xyz", get(hls::subtitles))
        .route("/p/:id/:filename", get(hls::segment))
        .nest("/api", api)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
