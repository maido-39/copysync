//! Content-addressed blob channel (`PUT/GET /blob/{sha256:...}`), pinned + bearer.

use std::time::Duration;

use sha2::{Digest, Sha256};

/// PUT bytes; the blob id is `sha256:<hex>` of exactly those bytes.
pub async fn put_blob(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    content: Vec<u8>,
) -> anyhow::Result<String> {
    let id = blob_id(&content);
    let url = format!("{}/blob/{}", base.trim_end_matches('/'), id);
    let resp = client
        .put(url)
        .bearer_auth(token)
        .body(content)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("blob PUT {}: {}", status.as_u16(), t.trim());
    }
    Ok(id)
}

/// GET a blob. The server may long-poll (up to ~60s) while it pulls an on-demand
/// blob from the origin device, so callers should use a generous timeout client.
pub async fn get_blob(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    id: &str,
) -> anyhow::Result<Vec<u8>> {
    let url = format!("{}/blob/{}", base.trim_end_matches('/'), id);
    let resp = client.get(url).bearer_auth(token).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("blob GET {}", resp.status().as_u16());
    }
    let bytes = resp.bytes().await?.to_vec();
    // idx 4 fix: the blob id IS the content address (sha256:<hex>) of the bytes,
    // so recompute it on receipt and reject a mismatch. This restores the
    // content-addressed integrity guarantee for the non-E2E (plaintext) path —
    // where, unlike E2E, nothing else authenticates the relayed bytes — and is
    // harmless for E2E (the id covers the ciphertext, which GCM also checks).
    // Every caller fetches by ev.blob_id, which is always content-addressed, so
    // this check is always valid.
    let got = blob_id(&bytes);
    if got != id {
        anyhow::bail!("blob hash mismatch: requested {id}, got {got}");
    }
    Ok(bytes)
}

/// A reqwest client with a long timeout for on-demand pulls.
pub fn pull_client(pin: [u8; 32]) -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::pinning::build_config(pin))
        .timeout(Duration::from_secs(70))
        .build()
        .expect("reqwest pull client")
}

pub fn blob_id(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}
