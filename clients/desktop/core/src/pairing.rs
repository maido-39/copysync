//! OTP pairing over the pinned TLS connection (mirrors `copyctl` pairing).

use serde::Deserialize;

use crate::config::Config;
use crate::pinning;

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub server_id: String,
    pub server_name: String,
    pub spki_pin: String,
    #[serde(default)]
    pub proto: i32,
}

/// Trust-on-first-use: read `/pair/serverinfo` WITHOUT pinning to discover the
/// pin. Only used at pairing time when the user did not supply a pin out-of-band.
pub async fn server_info_insecure(base: &str) -> anyhow::Result<ServerInfo> {
    let url = format!("{}/pair/serverinfo", base.trim_end_matches('/'));
    let resp = pinning::insecure_client().get(url).send().await?;
    Ok(resp.error_for_status()?.json().await?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResp {
    device_id: String,
    token: String,
    server_id: String,
    server_name: String,
}

/// Redeem an OTP over the pinned connection and return a populated [`Config`].
/// If `pin` is empty, the server's pin is discovered via TOFU first.
pub async fn claim(
    base: &str,
    pin: &str,
    otp: &str,
    device_name: &str,
    e2e_pass: &str,
) -> anyhow::Result<Config> {
    let pin = if pin.trim().is_empty() {
        server_info_insecure(base).await?.spki_pin
    } else {
        pin.trim().to_string()
    };
    let pin_bytes = pinning::decode_pin(&pin)?;

    let url = format!("{}/pair/claim", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "otp": otp,
        "deviceName": device_name,
        "platform": "linux",
    });
    let resp = pinning::http_client(pin_bytes)
        .post(url)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("pair failed (HTTP {}): {}", status.as_u16(), text.trim());
    }
    let r: ClaimResp = serde_json::from_str(&text)?;
    Ok(Config {
        server_url: base.trim_end_matches('/').to_string(),
        server_name: r.server_name,
        server_id: r.server_id,
        device_id: r.device_id,
        device_name: device_name.to_string(),
        token: r.token,
        pin,
        e2e_pass: e2e_pass.to_string(),
        exclude_sensitive: true,
        sensitive_ttl_secs: 45,
        custom_patterns: Vec::new(),
    })
}
