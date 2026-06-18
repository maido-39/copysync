//! Shared IPC vocabulary between `copysync-agent` (the headless sync daemon) and
//! the GUI client. The wire format is deliberately trivial: one JSON value per
//! line over a per-user local socket (a named pipe on Windows, an abstract-named
//! socket elsewhere). The GUI sends [`Request`] lines; the agent sends
//! [`Outbound`] lines (replies + pushed events on the same stream).

use serde::{Deserialize, Serialize};

/// Per-user socket label so multiple logged-in users don't collide. Maps to
/// `\\.\pipe\<label>` on Windows and an abstract-namespace socket on Linux.
pub fn socket_label() -> String {
    let who = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".into());
    format!("copysync-{who}.sock")
}

/// Live connection/sync state, mirroring the old Tauri `status` event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub connected: bool,
    pub reconnecting: bool,
    pub server: String,
    pub device: String,
    pub e2e: bool,
    pub pool: String,
    pub pools: Vec<String>,
}

/// One history row for the GUI's 기록 list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistRow {
    pub ts: String,
    pub kind: String,      // text | image | file
    pub direction: String, // in | out
    pub origin: String,
    pub preview: String,
    pub size: i64,
}

/// A clip event surfaced to the UI (drives toasts + the live feed). The trailing
/// fields are optional so the GUI has enough to render a rich feed entry without
/// the agent always populating them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipInfo {
    pub direction: String,
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub sensitive: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub on_demand: Option<bool>,
}

/// One roster device (mirrors the engine's `RosterDevice`) surfaced to the GUI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RosterDevice {
    pub id: String,
    pub name: String,
    pub online: bool,
}

/// A CopySync server found on the LAN via mDNS discovery.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoundServer {
    pub name: String,
    pub url: String,
}

/// GUI → agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Request {
    GetStatus,
    GetHistory { query: Option<String> },
    GetRoster,
    SendText { text: String },
    SendFile { path: String },
    SetPool { pool: String },
    SetTargets { ids: Vec<String> },
    SetPrivacyFilter { on: bool },
    SetAutoClear { secs: u64 },
    SetMarkSensitive { on: bool },
    DiscoverServers,
    Pair {
        server: String,
        otp: String,
        name: String,
        pin: String,
        e2e_pass: String,
    },
    Reconnect,
    /// Ask the agent to push [`Event`]s on this connection from now on.
    Subscribe,
}

/// agent → GUI: a direct reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Response {
    Status(Status),
    History(Vec<HistRow>),
    Roster(Vec<RosterDevice>),
    Found(Vec<FoundServer>),
    Paired(Status),
    Ok,
    Error { message: String },
}

/// agent → GUI: an unsolicited push to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Event {
    Status(Status),
    Clip(ClipInfo),
    Roster(Vec<RosterDevice>),
    Error { message: String },
    Reconnect { info: String },
    Notify { title: String, body: String },
    Cliplog { msg: String },
}

/// Everything the agent writes to a client, tagged so replies and pushed events
/// are distinguishable on a single duplex stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "msg", rename_all = "snake_case")]
pub enum Outbound {
    Reply(Response),
    Event(Event),
}

impl Outbound {
    /// Serialize as a single newline-terminated line.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| "{}".into());
        s.push('\n');
        s
    }
}
