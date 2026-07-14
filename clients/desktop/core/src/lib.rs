//! copysync-core — the shared client logic for the CopySync desktop app.
//!
//! It is a UI-agnostic toolkit (no Tauri, no display required) so the hard,
//! security-critical parts — the wire protocol, SPKI-pinned TLS, the WebSocket
//! control channel, the content-addressed blob channel, and the end-to-end
//! crypto — can be unit- and integration-tested headlessly against the real Go
//! server. The Tauri shell and the `interop` example are both thin layers on top.

pub mod blob;
pub mod clipboard;
pub mod config;
pub mod discovery;
pub mod e2e;
pub mod engine;
pub mod history;
pub mod pairing;
pub mod pinning;
pub mod privacy;
pub mod telemetry;
pub mod protocol;
pub mod ws;

pub use config::Config;
pub use protocol::{Ack, ClipEvent, Device, DeviceInfo, EncMeta, HelloOk, Targets};

use serde::Serialize;

/// Protocol version this client speaks (must match server `protocol.Proto`).
pub const PROTO: i32 = 1;

#[derive(Serialize)]
struct OutEnvelope<'a, T: Serialize> {
    t: &'a str,
    d: T,
}

/// Encode a control-channel frame: `{"t": <type>, "d": <payload>}`.
pub fn encode<T: Serialize>(t: &str, d: T) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&OutEnvelope { t, d })?)
}

/// Decode a frame into its type tag and the raw `d` payload for further parsing.
pub fn decode(data: &[u8]) -> anyhow::Result<(String, serde_json::Value)> {
    #[derive(serde::Deserialize)]
    struct InEnvelope {
        t: String,
        #[serde(default)]
        d: serde_json::Value,
    }
    let e: InEnvelope = serde_json::from_slice(data)?;
    Ok((e.t, e.d))
}
