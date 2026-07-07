//! Runtime configuration loaded from a JSON file next to the binary.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE_NAME: &str = "dv-hls-gateway.json";

static CONFIG: OnceLock<AppConfig> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub key_api: KeyApiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            key_api: KeyApiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 37201,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub key: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            key: generate_auth_key(),
        }
    }
}

impl AuthConfig {
    pub fn effective_key(&self) -> &str {
        self.key.trim()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyApiConfig {
    pub url: String,
    pub token: String,
    pub attempts: usize,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

impl Default for KeyApiConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            attempts: 12,
            retry_base_ms: 400,
            retry_max_ms: 8_000,
        }
    }
}

impl KeyApiConfig {
    pub fn require_endpoint(&self) -> crate::Result<(&str, &str)> {
        let url = self.url.trim();
        let token = self.token.trim();
        if url.is_empty() {
            return Err(anyhow::anyhow!(
                "dynamic key API URL is not configured; set key_api.url in {CONFIG_FILE_NAME}"
            ));
        }
        if token.is_empty() {
            return Err(anyhow::anyhow!(
                "dynamic key API token is not configured; set key_api.token in {CONFIG_FILE_NAME}"
            ));
        }
        Ok((url, token))
    }
}

pub fn init(config: AppConfig) {
    let _ = CONFIG.set(config);
}

pub fn get() -> &'static AppConfig {
    CONFIG.get_or_init(AppConfig::default)
}

pub fn default_config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(CONFIG_FILE_NAME)))
        .unwrap_or_else(|| PathBuf::from(CONFIG_FILE_NAME))
}

pub fn load_from_path(path: &Path) -> crate::Result<Option<AppConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
    let auth_missing_or_empty = value
        .get("auth")
        .and_then(|auth| auth.get("key"))
        .and_then(|key| key.as_str())
        .map(|key| key.trim().is_empty())
        .unwrap_or(true);
    let mut wrote_migration = false;
    if auth_missing_or_empty {
        let obj = value.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("parse config {}: root must be an object", path.display())
        })?;
        obj.insert(
            "auth".to_string(),
            serde_json::json!({ "key": generate_auth_key() }),
        );
        wrote_migration = true;
    }
    let config: AppConfig = serde_json::from_value(value)
        .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
    if wrote_migration {
        write_config(path, &config)?;
    }
    Ok(Some(config))
}

pub fn write_template(path: &Path) -> crate::Result<()> {
    write_config(path, &AppConfig::default())
}

fn write_config(path: &Path, config: &AppConfig) -> crate::Result<()> {
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(path, format!("{text}\n"))
        .map_err(|e| anyhow::anyhow!("write config template {}: {e}", path.display()))
}

fn generate_auth_key() -> String {
    format!("dvhls-{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_keep_server_port_but_leave_key_api_unset() {
        let config = AppConfig::default();
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 37201);
        assert!(config.auth.key.starts_with("dvhls-"));
        assert!(config.key_api.url.is_empty());
        assert!(config.key_api.require_endpoint().is_err());
    }

    #[test]
    fn config_accepts_minimal_json_with_key_api() {
        let config: AppConfig = serde_json::from_str(
            r#"{
              "server": { "port": 38080 },
              "auth": { "key": "panel-secret" },
              "key_api": { "url": "http://key-api.example.invalid/keys", "token": "secret" }
            }"#,
        )
        .unwrap();

        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 38080);
        assert_eq!(config.auth.effective_key(), "panel-secret");
        assert_eq!(
            config.key_api.require_endpoint().unwrap(),
            ("http://key-api.example.invalid/keys", "secret")
        );
        assert_eq!(config.key_api.attempts, 12);
    }
}
