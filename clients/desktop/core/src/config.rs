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

    /// Privacy filter: don't sync clips classified sensitive (passwords, cards, keys…).
    #[serde(default = "default_true")]
    pub exclude_sensitive: bool,
    /// Auto-delete sensitive clips from local history after N seconds (0 = keep).
    #[serde(default = "default_sensitive_ttl")]
    pub sensitive_ttl_secs: u64,
    /// Extra user regexes that also mark a clip sensitive.
    #[serde(default)]
    pub custom_patterns: Vec<String>,

    /// Global hotkey that toggles the Quick Panel history overlay (an accelerator
    /// string parsed by tauri-plugin-global-shortcut, e.g. "Control+Shift+KeyV").
    /// Empty disables the hotkey.
    #[serde(default = "default_quick_panel_shortcut")]
    pub quick_panel_shortcut: String,
}

/// Default Quick Panel hotkey: Cmd+Shift+V on macOS, Ctrl+Shift+V elsewhere.
pub const DEFAULT_QUICK_PANEL_SHORTCUT: &str = "CommandOrControl+Shift+KeyV";

fn default_true() -> bool {
    true
}
fn default_sensitive_ttl() -> u64 {
    45
}
fn default_quick_panel_shortcut() -> String {
    DEFAULT_QUICK_PANEL_SHORTCUT.to_string()
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

    /// Compile the user's custom sensitivity patterns (invalid ones are skipped).
    pub fn custom_regexes(&self) -> Vec<regex::Regex> {
        self.custom_patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect()
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
