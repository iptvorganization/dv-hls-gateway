//! 并发下载分段（reqwest HTTP/2），含限流、超时和短重试。

use std::{
    collections::HashMap,
    env, fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use reqwest::{Client, StatusCode};
use tokio::sync::Semaphore;

/// 共享 HTTP 客户端。
#[derive(Clone)]
pub struct Downloader {
    client: Client,
    shared_downloads: Arc<Semaphore>,
    host_downloads: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

const SHARED_DOWNLOAD_CONCURRENCY: usize = 10;
const PER_HOST_DOWNLOAD_CONCURRENCY: usize = 8;
const MEDIA_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(12);
const TEXT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ATTEMPTS: usize = 3;
const MAX_403_RETRIES: usize = 3;

impl Downloader {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (dv-hls-gateway)")
            .connect_timeout(Duration::from_secs(8))
            .build()
            .expect("build reqwest client");
        Self {
            client,
            shared_downloads: Arc::new(Semaphore::new(shared_download_concurrency())),
            host_downloads: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 下载一个 URL 的全部字节。
    pub async fn get(&self, url: &str) -> crate::Result<bytes::Bytes> {
        let _shared = self
            .shared_downloads
            .clone()
            .acquire_owned()
            .await
            .context("shared download semaphore closed")?;
        let _host = self
            .host_semaphore(url)
            .acquire_owned()
            .await
            .context("per-host download semaphore closed")?;
        self.request_bytes_with_retry(url).await
    }

    /// 下载 MPD 文本。
    pub async fn get_text(&self, url: &str) -> crate::Result<String> {
        self.request_text_with_retry(url).await
    }

    fn host_semaphore(&self, url: &str) -> Arc<Semaphore> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| "<unknown>".to_string());
        let mut hosts = self
            .host_downloads
            .lock()
            .expect("host limiter mutex poisoned");
        hosts
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(per_host_download_concurrency())))
            .clone()
    }

    async fn request_bytes_with_retry(&self, url: &str) -> crate::Result<bytes::Bytes> {
        let mut last_failure: Option<RequestFailure> = None;
        let mut forbidden_retries = 0usize;

        for attempt in 1..=MAX_ATTEMPTS + MAX_403_RETRIES {
            let result =
                tokio::time::timeout(MEDIA_ATTEMPT_TIMEOUT, self.request_bytes_once(url)).await;
            match result {
                Ok(Ok(bytes)) => return Ok(bytes),
                Ok(Err(err)) => {
                    let retry = should_retry(err.status, attempt, forbidden_retries);
                    if err.status == Some(StatusCode::FORBIDDEN) && retry {
                        forbidden_retries += 1;
                    }
                    last_failure = Some(err);
                    if !retry {
                        break;
                    }
                }
                Err(_) => {
                    last_failure = Some(RequestFailure::timeout(MEDIA_ATTEMPT_TIMEOUT));
                    if attempt >= MAX_ATTEMPTS {
                        break;
                    }
                }
            }
            tokio::time::sleep(retry_delay(attempt, forbidden_retries)).await;
        }

        let failure = last_failure.unwrap_or_else(|| RequestFailure::other("unknown error"));
        Err(DownloadError::new(url, failure.kind, failure.message).into())
    }

    async fn request_text_with_retry(&self, url: &str) -> crate::Result<String> {
        let mut last_failure: Option<RequestFailure> = None;
        let mut forbidden_retries = 0usize;

        for attempt in 1..=MAX_ATTEMPTS + MAX_403_RETRIES {
            let result =
                tokio::time::timeout(TEXT_ATTEMPT_TIMEOUT, self.request_text_once(url)).await;
            match result {
                Ok(Ok(text)) => return Ok(text),
                Ok(Err(err)) => {
                    let retry = should_retry(err.status, attempt, forbidden_retries);
                    if err.status == Some(StatusCode::FORBIDDEN) && retry {
                        forbidden_retries += 1;
                    }
                    last_failure = Some(err);
                    if !retry {
                        break;
                    }
                }
                Err(_) => {
                    last_failure = Some(RequestFailure::timeout(TEXT_ATTEMPT_TIMEOUT));
                    if attempt >= MAX_ATTEMPTS {
                        break;
                    }
                }
            }
            tokio::time::sleep(retry_delay(attempt, forbidden_retries)).await;
        }

        let failure = last_failure.unwrap_or_else(|| RequestFailure::other("unknown error"));
        Err(DownloadError::new(url, failure.kind, failure.message).into())
    }

    async fn request_bytes_once(&self, url: &str) -> Result<bytes::Bytes, RequestFailure> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(RequestFailure::from_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RequestFailure::from_status(status, body));
        }
        resp.bytes().await.map_err(RequestFailure::from_reqwest)
    }

    async fn request_text_once(&self, url: &str) -> Result<String, RequestFailure> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(RequestFailure::from_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RequestFailure::from_status(status, body));
        }
        resp.text().await.map_err(RequestFailure::from_reqwest)
    }
}

struct RequestFailure {
    status: Option<StatusCode>,
    kind: DownloadFailureKind,
    message: String,
}

impl RequestFailure {
    fn from_reqwest(err: reqwest::Error) -> Self {
        let message = err.to_string();
        Self {
            status: err.status(),
            kind: DownloadFailureKind::from_reqwest(&err, &message),
            message,
        }
    }

    fn from_status(status: StatusCode, body: String) -> Self {
        let preview: String = body.chars().take(240).collect();
        let message = if preview.is_empty() {
            format!("upstream returned status {status}")
        } else {
            format!("upstream returned status {status}: {preview}")
        };
        Self {
            status: Some(status),
            kind: DownloadFailureKind::HttpStatus(status),
            message,
        }
    }

    fn timeout(timeout: Duration) -> Self {
        Self {
            status: None,
            kind: DownloadFailureKind::Timeout,
            message: format!("timed out after {}s", timeout.as_secs()),
        }
    }

    fn other(message: impl Into<String>) -> Self {
        Self {
            status: None,
            kind: DownloadFailureKind::Other,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadFailureKind {
    HttpStatus(StatusCode),
    Timeout,
    ConnectionReset,
    Network,
    Other,
}

impl DownloadFailureKind {
    fn from_reqwest(err: &reqwest::Error, message: &str) -> Self {
        if err.is_timeout() {
            return Self::Timeout;
        }
        if looks_like_connection_reset(message) {
            return Self::ConnectionReset;
        }
        if err.is_connect() {
            return Self::Network;
        }
        if let Some(status) = err.status() {
            return Self::HttpStatus(status);
        }
        Self::Other
    }

    pub fn is_concurrency_pressure(self) -> bool {
        match self {
            Self::Timeout | Self::ConnectionReset => true,
            Self::HttpStatus(status) => matches!(
                status,
                StatusCode::REQUEST_TIMEOUT
                    | StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ),
            Self::Network | Self::Other => false,
        }
    }
}

impl fmt::Display for DownloadFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(status) => write!(f, "http_status_{status}"),
            Self::Timeout => f.write_str("timeout"),
            Self::ConnectionReset => f.write_str("connection_reset"),
            Self::Network => f.write_str("network"),
            Self::Other => f.write_str("other"),
        }
    }
}

#[derive(Debug)]
pub struct DownloadError {
    url: String,
    kind: DownloadFailureKind,
    message: String,
}

impl DownloadError {
    fn new(url: &str, kind: DownloadFailureKind, message: String) -> Self {
        Self {
            url: url.to_string(),
            kind,
            message,
        }
    }

    pub fn kind(&self) -> DownloadFailureKind {
        self.kind
    }

    pub fn is_concurrency_pressure(&self) -> bool {
        self.kind.is_concurrency_pressure()
    }
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to download {} ({}): {}",
            self.url, self.kind, self.message
        )
    }
}

impl std::error::Error for DownloadError {}

fn should_retry(status: Option<StatusCode>, attempt: usize, forbidden_retries: usize) -> bool {
    match status {
        Some(StatusCode::FORBIDDEN) => forbidden_retries < MAX_403_RETRIES,
        Some(StatusCode::REQUEST_TIMEOUT) | Some(StatusCode::TOO_MANY_REQUESTS) => {
            attempt < MAX_ATTEMPTS
        }
        Some(s) if s.is_server_error() => attempt < MAX_ATTEMPTS,
        Some(_) => false,
        None => attempt < MAX_ATTEMPTS,
    }
}

fn retry_delay(attempt: usize, forbidden_retries: usize) -> Duration {
    let base_ms = if forbidden_retries > 0 {
        200 * forbidden_retries as u64
    } else {
        250 * attempt as u64
    };
    Duration::from_millis(base_ms.min(1_500))
}

fn shared_download_concurrency() -> usize {
    env_usize("MPD_HLS_SHARED_DOWNLOAD_CONCURRENCY")
        .unwrap_or(SHARED_DOWNLOAD_CONCURRENCY)
        .max(1)
}

fn per_host_download_concurrency() -> usize {
    env_usize("MPD_HLS_PER_HOST_CONCURRENCY")
        .unwrap_or(PER_HOST_DOWNLOAD_CONCURRENCY)
        .max(1)
}

fn env_usize(name: &str) -> Option<usize> {
    env::var(name).ok()?.trim().parse().ok()
}

fn looks_like_connection_reset(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("stream error")
        || lower.contains("http2 error")
        || lower.contains("operation was aborted")
}

impl Default for Downloader {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedDownloader = Arc<Downloader>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_statuses_are_limited_to_retry_pressure_errors() {
        assert!(DownloadFailureKind::Timeout.is_concurrency_pressure());
        assert!(DownloadFailureKind::ConnectionReset.is_concurrency_pressure());
        assert!(
            DownloadFailureKind::HttpStatus(StatusCode::TOO_MANY_REQUESTS)
                .is_concurrency_pressure()
        );
        assert!(
            DownloadFailureKind::HttpStatus(StatusCode::SERVICE_UNAVAILABLE)
                .is_concurrency_pressure()
        );
        assert!(!DownloadFailureKind::HttpStatus(StatusCode::FORBIDDEN).is_concurrency_pressure());
        assert!(!DownloadFailureKind::HttpStatus(StatusCode::NOT_FOUND).is_concurrency_pressure());
        assert!(
            !DownloadFailureKind::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR)
                .is_concurrency_pressure()
        );
    }
}
