//! Persisted client identity (written after pairing). Mirrors `copyctl`'s Config.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub server_url: String,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub server_id: String,
    pub device_id: String,
    pub device_name: String,
    pub token: String,
    /// base64 SPKI pin.
    pub pin: String,
    /// Optional E2E passphrase. Empty = E2E off.
    #[serde(default)]
    pub e2e_pass: String,
}

impl Config {
    pub fn pin_bytes(&self) -> anyhow::Result<[u8; 32]> {
        crate::pinning::decode_pin(&self.pin)
    }

    /// Derived (key, key_id) when an E2E passphrase + server id are set.
    pub fn e2e_key(&self) -> Option<(Vec<u8>, String)> {
        if self.e2e_pass.is_empty() || self.server_id.is_empty() {
            return None;
        }
        let k = crate::e2e::derive_key(&self.e2e_pass, &self.server_id);
        let id = crate::e2e::key_id(&k);
        Some((k.to_vec(), id))
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Config> {
        let data = std::fs::read(path)?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        if let Some(dir) = path.as_ref().parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}
