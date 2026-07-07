//! Fixed and dynamic key loading shared by MPD and HLS pipelines.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::json;

use crate::config::{self, CONFIG_FILE_NAME};
use crate::crypto::cenc::{parse_key_hex, KeyStore};

use super::manager::KeyMode;

pub async fn resolve_key_store(
    mode: KeyMode,
    static_keys: &str,
    required_kids: impl IntoIterator<Item = impl AsRef<str>>,
) -> crate::Result<KeyStore> {
    let kids = normalize_unique_kids(required_kids);
    match mode {
        KeyMode::Static => {
            let store = KeyStore::parse(static_keys);
            if store.is_empty() && !kids.is_empty() {
                return Err(anyhow::anyhow!("no keys provided"));
            }
            Ok(store)
        }
        KeyMode::Dynamic => {
            if kids.is_empty() {
                return Ok(KeyStore::default());
            }
            fetch_key_store_for_kids(&kids).await
        }
    }
}

pub async fn fetch_missing_dynamic_keys(
    store: &mut KeyStore,
    kids: impl IntoIterator<Item = impl AsRef<str>>,
) -> crate::Result<()> {
    let missing = normalize_unique_kids(kids)
        .into_iter()
        .filter(|kid| !store.has_kid(kid) && !store.has_bare_key())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    let fetched = fetch_key_store_for_kids(&missing).await?;
    store.merge(fetched);
    Ok(())
}

fn normalize_unique_kids(kids: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    kids.into_iter()
        .filter_map(|kid| normalize_kid(kid.as_ref()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn normalize_kid(kid: &str) -> Option<String> {
    let kid = kid.trim().trim_start_matches("0x").trim_start_matches("0X");
    let kid = kid.to_ascii_lowercase().replace('-', "");
    (kid.len() == 32 && kid.chars().all(|c| c.is_ascii_hexdigit())).then_some(kid)
}

async fn fetch_key_store_for_kids(kids: &[String]) -> crate::Result<KeyStore> {
    let key_api = &config::get().key_api;
    let attempts = env_usize("DVHLS_KEY_API_ATTEMPTS")
        .unwrap_or(key_api.attempts)
        .max(1);
    let mut last_error = None;
    for attempt in 1..=attempts {
        match fetch_key_store_once(kids).await {
            Ok(store) => return Ok(store),
            Err(e) => {
                let message = format!("{e:#}");
                last_error = Some(e);
                if attempt < attempts {
                    let delay = key_api_retry_delay(attempt);
                    tracing::warn!(
                        attempt,
                        attempts,
                        delay_ms = delay.as_millis() as u64,
                        kids = %kids.join(","),
                        "dynamic key fetch failed, retrying: {message}"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("dynamic key API failed")))
}

async fn fetch_key_store_once(kids: &[String]) -> crate::Result<KeyStore> {
    let text = fetch_key_lines(kids).await?;
    let store = KeyStore::parse(&text);
    if store.is_empty() {
        return Err(anyhow::anyhow!("dynamic key API returned no usable keys"));
    }
    Ok(store)
}

fn key_api_retry_delay(attempt: usize) -> Duration {
    let key_api = &config::get().key_api;
    let base = env_u64("DVHLS_KEY_API_RETRY_BASE_MS").unwrap_or(key_api.retry_base_ms);
    let max = env_u64("DVHLS_KEY_API_RETRY_MAX_MS").unwrap_or(key_api.retry_max_ms);
    let scaled = base.saturating_mul(attempt as u64);
    Duration::from_millis(scaled.min(max.max(base)))
}

async fn fetch_key_lines(kids: &[String]) -> crate::Result<String> {
    let (url, token) = key_api_endpoint()?;
    let body = json!({ "kid": kids }).to_string();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(8))
        .build()?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("X-Token", token)
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "dynamic key API returned status {status}: {}",
            text.chars().take(240).collect::<String>()
        ));
    }
    parse_key_api_response_for_kids(&text, kids)
}

fn key_api_endpoint() -> crate::Result<(String, String)> {
    let key_api = &config::get().key_api;
    let url = std::env::var("DVHLS_KEY_API_URL").unwrap_or_else(|_| key_api.url.clone());
    let token = std::env::var("DVHLS_KEY_API_TOKEN").unwrap_or_else(|_| key_api.token.clone());
    let url = url.trim().to_string();
    let token = token.trim().to_string();
    if url.is_empty() {
        return Err(anyhow::anyhow!(
            "dynamic key API URL is not configured; set key_api.url in {CONFIG_FILE_NAME} or DVHLS_KEY_API_URL"
        ));
    }
    if token.is_empty() {
        return Err(anyhow::anyhow!(
            "dynamic key API token is not configured; set key_api.token in {CONFIG_FILE_NAME} or DVHLS_KEY_API_TOKEN"
        ));
    }
    Ok((url, token))
}

pub fn parse_key_api_response(text: &str) -> crate::Result<String> {
    parse_key_api_response_for_kids(text, &[])
}

pub fn parse_key_api_response_for_kids(
    text: &str,
    requested_kids: &[String],
) -> crate::Result<String> {
    let lines: Vec<String> = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("dynamic key API returned invalid JSON string array: {e}"))?;
    let mut normalized_lines = Vec::with_capacity(lines.len());
    let mut returned_kids = BTreeSet::new();

    for line in lines
        .into_iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
    {
        let (kid, _) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("dynamic key API returned non KID:KEY item: {line}"))?;
        let kid = normalize_kid(kid).ok_or_else(|| {
            anyhow::anyhow!("dynamic key API returned invalid KID in item: {line}")
        })?;
        if parse_key_hex(&line).is_none() {
            return Err(anyhow::anyhow!(
                "dynamic key API returned invalid KEY in item for KID {kid}"
            ));
        }
        returned_kids.insert(kid.clone());
        normalized_lines.push(line);
    }

    if normalized_lines.is_empty() {
        return Err(anyhow::anyhow!(
            "dynamic key API returned an empty key array"
        ));
    }

    let missing = requested_kids
        .iter()
        .filter_map(|kid| normalize_kid(kid))
        .filter(|kid| !returned_kids.contains(kid))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "dynamic key API did not return KID:KEY for requested KID(s): {}",
            missing.join(",")
        ));
    }

    Ok(normalized_lines.join("\n"))
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_kids() {
        assert_eq!(
            normalize_kid("0x00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            "00112233445566778899aabbccddeeff"
        );
        assert!(normalize_kid("not-a-kid").is_none());
    }

    #[test]
    fn parses_key_api_array() {
        let text = r#"[
          "00112233445566778899aabbccddeeff:8ad11538e926554d58d389209643b9e1",
          "11223344556677889900aabbccddeeff:9ad11538e926554d58d389209643b9e1"
        ]"#;
        let parsed = parse_key_api_response_for_kids(
            text,
            &["00112233445566778899aabbccddeeff".to_string()],
        )
        .unwrap();
        assert!(
            parsed.contains("00112233445566778899aabbccddeeff:8ad11538e926554d58d389209643b9e1")
        );
        assert!(parsed.contains('\n'));
    }

    #[test]
    fn key_api_array_must_contain_kid_key_items() {
        let bare = r#"["8ad11538e926554d58d389209643b9e1"]"#;
        assert!(parse_key_api_response_for_kids(
            bare,
            &["00112233445566778899aabbccddeeff".to_string()]
        )
        .is_err());

        let invalid_key = r#"["00112233445566778899aabbccddeeff:not-a-key"]"#;
        assert!(parse_key_api_response_for_kids(
            invalid_key,
            &["00112233445566778899aabbccddeeff".to_string()]
        )
        .is_err());
    }

    #[test]
    fn key_api_array_must_cover_requested_kids() {
        let text = r#"["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:8ad11538e926554d58d389209643b9e1"]"#;
        let err = parse_key_api_response_for_kids(
            text,
            &["00112233445566778899aabbccddeeff".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("00112233445566778899aabbccddeeff"));
    }

    #[test]
    fn key_api_response_must_be_json_string_array() {
        assert!(parse_key_api_response_for_kids(
            r#"{"kid":"00112233445566778899aabbccddeeff"}"#,
            &["00112233445566778899aabbccddeeff".to_string()]
        )
        .is_err());
    }
}
