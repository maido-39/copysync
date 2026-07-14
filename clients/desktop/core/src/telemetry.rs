//! Client → server telemetry upload.
//!
//! Ships this device's system-operation logs (engine start/stop, reconnects,
//! errors, pairing) to the paired server so an operator can diagnose a client
//! WITHOUT shell access to it — `agent.log`/`engine.log` otherwise never leave
//! the machine. Transport is the same pinned-TLS + device-bearer-token channel
//! the blob uploads use.
//!
//! Privacy: only operational lines are sent — never clipboard content. The
//! caller is responsible for keeping clip text out of the messages it enqueues.

use serde::Serialize;

/// One operational log line to upload.
#[derive(Serialize, Clone)]
pub struct Line {
    /// "info" | "warn" | "error".
    pub level: String,
    /// Client-side timestamp (free-form; the server also stamps receive time).
    pub ts: String,
    /// The message. Must not contain clipboard content.
    pub msg: String,
}

#[derive(Serialize)]
struct Body<'a> {
    client: &'a str,
    lines: &'a [Line],
}

/// POST a batch of operational log lines to `{server_url}/telemetry`, pinned +
/// bearer-authenticated. `client` labels the sender ("agent"/"gui"/…). Returns
/// `Ok(())` only on a 2xx response so the caller can retry (re-buffer) on failure.
pub async fn upload(
    server_url: &str,
    token: &str,
    pin: [u8; 32],
    client: &str,
    lines: &[Line],
) -> anyhow::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let http = crate::pinning::http_client(pin);
    let url = format!("{}/telemetry", server_url.trim_end_matches('/'));
    let resp = http
        .post(url)
        .bearer_auth(token)
        .json(&Body { client, lines })
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("telemetry upload: HTTP {}", resp.status().as_u16());
    }
    Ok(())
}
