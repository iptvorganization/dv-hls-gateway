//! dv-hls-gateway 服务入口。
//!
//! 启动纯手动推流 HTTP 服务。

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use dv_hls_gateway::config::{self, AppConfig};
use dv_hls_gateway::fetch::Downloader;
use dv_hls_gateway::server::build_router;
use dv_hls_gateway::task::TaskManager;

#[derive(Parser, Debug)]
#[command(
    name = "dv-hls-gateway",
    about = "MPD/M3U8 stream to live HLS-TS gateway"
)]
struct Args {
    /// 配置文件路径；默认读取二进制同目录的 dv-hls-gateway.json
    #[arg(long)]
    config: Option<PathBuf>,
    /// 监听地址（0.0.0.0 = 所有网卡，可被局域网/外部访问）
    #[arg(long)]
    host: Option<String>,
    /// 监听端口
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dv_hls_gateway=debug".into()),
        )
        .init();

    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(config::default_config_path);
    let mut app_config = match config::load_from_path(&config_path)? {
        Some(config) => {
            tracing::info!(path = %config_path.display(), "loaded config");
            config
        }
        None => {
            let config = AppConfig::default();
            match config::write_template(&config_path, &config) {
                Ok(()) => tracing::warn!(
                    path = %config_path.display(),
                    "config file not found; wrote default template"
                ),
                Err(e) => tracing::warn!(
                    path = %config_path.display(),
                    error = %e,
                    "config file not found and template could not be written; using built-in server defaults"
                ),
            }
            config
        }
    };
    if let Some(host) = args.host {
        app_config.server.host = host;
    }
    if let Some(port) = args.port {
        app_config.server.port = port;
    }
    config::init(app_config.clone());

    let downloader = Arc::new(Downloader::new());
    let mgr = TaskManager::new(downloader);

    let app = build_router(mgr);
    let addr = format!("{}:{}", app_config.server.host, app_config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        config = %config_path.display(),
        "dv-hls-gateway listening on http://{addr}"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
