//! GitHub App manifest conversion helpers.
//!
//! HTTP access to GitHub lives in `djinn-provider` so the web server does not
//! construct raw HTTP clients outside the provider capability boundary.

use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::Deserialize;

/// Manifest conversion response from `POST /app-manifests/{code}/conversions`.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestConversion {
    pub id: u64,
    pub slug: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub webhook_secret: Option<String>,
    pub pem: String,
}

/// Exchange a GitHub App manifest `code` for the generated App credentials.
pub async fn exchange_manifest_code(code: &str) -> Result<ManifestConversion> {
    let url = format!("https://api.github.com/app-manifests/{code}/conversions");
    let resp = Client::new()
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "djinn-server")
        .send()
        .await
        .map_err(|e| anyhow!("manifest conversion request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("manifest conversion HTTP {status}: {body}"));
    }

    resp.json::<ManifestConversion>()
        .await
        .map_err(|e| anyhow!("manifest conversion decode failed: {e}"))
}
