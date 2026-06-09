//! Wire-protocol types. These mirror `server/internal/protocol/messages.go` and
//! `server/internal/model/types.go` field-for-field (see `docs/PROTOCOL.md`).

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

// Message type tags — the "t" field of the envelope.
pub const T_HELLO: &str = "hello";
pub const T_HELLO_OK: &str = "hello_ok";
pub const T_HELLO_ERR: &str = "hello_err";
pub const T_CLIP: &str = "clip";
pub const T_ACK: &str = "ack";
pub const T_BLOB_REQUEST: &str = "blob_request";
pub const T_PRESENCE: &str = "presence";
pub const T_ROSTER: &str = "roster";
pub const T_ERROR: &str = "error";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_seen_at: String,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct DeviceInfo {
    #[serde(flatten)]
    pub device: Device,
    #[serde(default)]
    pub online: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub device_id: String,
    pub device_name: String,
    pub token: String,
    pub platform: String,
    pub proto: i32,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct HelloOk {
    pub server_id: String,
    pub server_name: String,
    #[serde(default)]
    pub e2e: bool,
    #[serde(default)]
    pub you: Device,
    #[serde(default)]
    pub roster: Vec<DeviceInfo>,
    #[serde(default)]
    pub max_msg: i64,
    #[serde(default)]
    pub blob_cap: i64,
    #[serde(default)]
    pub on_demand_threshold: i64,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct HelloErr {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct EncMeta {
    pub alg: String,
    pub key_id: String,
    #[serde(default)]
    pub nonce: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Ack {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub queued_for: Vec<String>,
    #[serde(default)]
    pub message: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct BlobRequest {
    pub id: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Roster {
    #[serde(default)]
    pub devices: Vec<DeviceInfo>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Presence {
    #[serde(default)]
    pub device: Device,
    #[serde(default)]
    pub online: bool,
}

/// Recipient selector: the JSON string `"all"` or an array of device ids.
#[derive(Clone, Debug)]
pub enum Targets {
    All,
    Devices(Vec<String>),
}

impl Default for Targets {
    fn default() -> Self {
        Targets::All
    }
}

impl Serialize for Targets {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Targets::All => s.serialize_str("all"),
            Targets::Devices(d) => d.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Targets {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Null => Ok(Targets::Devices(vec![])),
            serde_json::Value::String(s) if s == "all" => Ok(Targets::All),
            serde_json::Value::String(s) => Err(de::Error::custom(format!("unknown targets {s}"))),
            serde_json::Value::Array(_) => {
                let ids: Vec<String> =
                    serde_json::from_value(v).map_err(de::Error::custom)?;
                Ok(Targets::Devices(ids))
            }
            _ => Err(de::Error::custom("targets must be \"all\" or an array")),
        }
    }
}

/// A clipboard item relayed through the server.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClipEvent {
    pub id: String,
    #[serde(rename = "originDeviceId")]
    pub origin_device: String,
    pub seq: u64,
    pub ts: String,
    #[serde(default)]
    pub mime: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub inline_text: String,
    /// Rich-text (text/html) variant; ciphertext (base64) when E2E, like inline_text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blob_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub on_demand: bool,
    pub size: i64,
    #[serde(default)]
    pub sha256: String,
    pub targets: Targets,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enc: Option<EncMeta>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ClipEvent {
    /// True when this event references a blob rather than carrying inline text.
    pub fn is_blob(&self) -> bool {
        !self.blob_id.is_empty()
    }

    /// A short human-readable kind for history/UI.
    pub fn kind(&self) -> &'static str {
        if self.is_blob() {
            if self.mime.first().map(|m| m.starts_with("image/")).unwrap_or(false) {
                "image"
            } else {
                "file"
            }
        } else {
            "text"
        }
    }

    /// Build a text clip, encrypting in place when an E2E key is supplied
    /// (`(key, key_id)`). Byte-compatible with `copyctl`'s `sendText`: `size` is
    /// the plaintext length; `sha256` is of the payload actually sent.
    pub fn new_text(
        seq: u64,
        text: &str,
        html: Option<&str>,
        key: Option<(&[u8], &str)>,
        targets: Targets,
    ) -> anyhow::Result<ClipEvent> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use sha2::{Digest, Sha256};
        let html = html.filter(|h| !h.is_empty());
        let mut ev = ClipEvent {
            id: new_id(),
            seq,
            ts: now_ts(),
            mime: if html.is_some() {
                vec!["text/html".into(), "text/plain".into()]
            } else {
                vec!["text/plain".into()]
            },
            size: text.len() as i64,
            targets,
            ..Default::default()
        };
        match key {
            Some((k, kid)) => {
                let raw = crate::e2e::seal(k, text.as_bytes())?;
                ev.inline_text = STANDARD.encode(&raw);
                ev.sha256 = hex::encode(Sha256::digest(&raw));
                if let Some(h) = html {
                    ev.html = STANDARD.encode(crate::e2e::seal(k, h.as_bytes())?);
                }
                ev.enc = Some(EncMeta {
                    alg: crate::e2e::ALG.into(),
                    key_id: kid.into(),
                    nonce: String::new(),
                });
            }
            None => {
                ev.inline_text = text.to_string();
                ev.sha256 = hex::encode(Sha256::digest(text.as_bytes()));
                if let Some(h) = html {
                    ev.html = h.to_string();
                }
            }
        }
        Ok(ev)
    }
}

/// 16 random bytes, hex-encoded — a clip/event id.
pub fn new_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// Current time as an RFC3339 string (the server also stamps empty ts).
pub fn now_ts() -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_roundtrip() {
        assert_eq!(serde_json::to_string(&Targets::All).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::to_string(&Targets::Devices(vec!["d1".into()])).unwrap(),
            "[\"d1\"]"
        );
        let t: Targets = serde_json::from_str("\"all\"").unwrap();
        assert!(matches!(t, Targets::All));
        let t: Targets = serde_json::from_str("[\"a\",\"b\"]").unwrap();
        assert!(matches!(t, Targets::Devices(v) if v.len() == 2));
        let t: Targets = serde_json::from_str("null").unwrap();
        assert!(matches!(t, Targets::Devices(v) if v.is_empty()));
    }

    #[test]
    fn clip_field_names_match_go() {
        let ev = ClipEvent {
            id: "x".into(),
            origin_device: "dev".into(),
            seq: 7,
            ts: "2026-01-01T00:00:00Z".into(),
            mime: vec!["text/plain".into()],
            inline_text: "hi".into(),
            size: 2,
            sha256: "ab".into(),
            targets: Targets::All,
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["originDeviceId"], "dev");
        assert_eq!(v["inlineText"], "hi");
        assert_eq!(v["targets"], "all");
        assert!(v.get("blobId").is_none(), "empty blobId must be omitted");
        assert!(v.get("enc").is_none(), "nil enc must be omitted");
    }
}
