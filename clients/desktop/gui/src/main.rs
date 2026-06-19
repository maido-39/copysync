//! copysync-gui — the native eframe/egui desktop client.
//!
//! This is a *pure IPC client* to `copysync-agent`: it owns no sync state itself.
//! Every action turns into a [`Request`] over the per-user local socket; live
//! updates arrive as pushed [`Event`]s on a long-lived subscribed connection.
//!
//! Wire format: newline-delimited JSON over an `interprocess` v2 SYNC local
//! socket. The GUI writes `Request` lines and reads `Outbound` lines back
//! (`Reply(Response)` for direct answers, `Event(Event)` for pushed updates).
//!
//! Three IPC paths:
//!   * [`request`]  — one short-lived `Stream` per call: write the request, read
//!     ONE reply line. Local socket, so cheap enough to call inline from UI
//!     handlers. The one exception is `DiscoverServers` (~2.5s mDNS) which we run
//!     on a worker thread and deliver via a channel.
//!   * the **event thread** — connects, `Subscribe`s, then loops reading pushed
//!     `Event` lines onto an mpsc sender, reconnecting on drop. Holds an
//!     `egui::Context` so it can `request_repaint()` after each event.
//!   * **auto-spawn** — if the first connect fails we spawn the sibling
//!     `copysync-agent` binary and retry for ~3s before giving up.
//!
//! This box is headless: the GUI compiles and links but its runtime (a real
//! window + GL context) was never exercised here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use eframe::egui;
use egui::Color32;

use copysync_ipc::{
    socket_label, Event, FoundServer, HistRow, Outbound, Request, Response, RosterDevice, Status,
};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, Stream};

// M4 desktop-shell crates. Each exposes a *global* event channel via a
// `receiver()` that we poll with `try_recv()` from `update()` — none of them
// need to be threaded into the winit loop, they just need `update()` to keep
// running while idle (see `request_repaint_after` in `update`).
use auto_launch::AutoLaunchBuilder;
use global_hotkey::{
    hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use std::str::FromStr;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

// ============================================================ IPC transport

/// Open a fresh socket connection, or `Err` if the agent isn't listening.
fn connect() -> Result<Stream> {
    let name = socket_label().to_ns_name::<GenericNamespaced>()?;
    Ok(Stream::connect(name)?)
}

/// One short-lived request/reply round-trip. Opens a connection, writes the
/// request line, reads exactly ONE reply line, and parses it as
/// `Outbound::Reply(Response)`. Pushed events on a fresh (un-subscribed)
/// connection won't appear, so the first line is always our reply.
fn request(req: &Request) -> Result<Response> {
    let conn = connect().context("connect to agent (is it running?)")?;
    let mut reader = BufReader::new(conn);
    let line = serde_json::to_string(req)? + "\n";
    reader.get_mut().write_all(line.as_bytes())?;
    reader.get_mut().flush()?;
    let mut buf = String::new();
    if reader.read_line(&mut buf)? == 0 {
        return Err(anyhow!("연결이 종료되었습니다 (응답 없음)"));
    }
    match serde_json::from_str::<Outbound>(buf.trim())? {
        Outbound::Reply(r) => Ok(r),
        // A subscribed connection could interleave events; a one-shot one
        // shouldn't, but be defensive and surface it rather than mis-parse.
        Outbound::Event(_) => Err(anyhow!("예상치 못한 이벤트 응답")),
    }
}

/// Convenience: send a request, treat `Response::Error` as a real error so the
/// caller can route it straight to a status string / debug log.
fn request_ok(req: &Request) -> Result<Response> {
    match request(req)? {
        Response::Error { message } => Err(anyhow!(message)),
        other => Ok(other),
    }
}

/// Resolve the sibling `copysync-agent` binary path next to our own executable
/// (`copysync-agent.exe` on Windows). Used both for auto-spawn and for the
/// login-autostart entry, so the daemon is what runs at boot — not the GUI.
fn agent_path() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("locate own executable")?;
    let dir = exe.parent().context("executable has no parent dir")?;
    let agent_name = if cfg!(windows) {
        "copysync-agent.exe"
    } else {
        "copysync-agent"
    };
    Ok(dir.join(agent_name))
}

/// Spawn the sibling `copysync-agent` binary next to our own executable, then
/// poll-connect for ~3s. Returns `Ok` once a connection succeeds.
fn spawn_agent_and_wait() -> Result<()> {
    let agent = agent_path()?;
    if !agent.exists() {
        return Err(anyhow!(
            "copysync-agent 실행 파일을 찾을 수 없습니다: {}",
            agent.display()
        ));
    }
    std::process::Command::new(&agent)
        .arg("serve")
        .spawn()
        .with_context(|| format!("spawn {}", agent.display()))?;

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if connect().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(anyhow!("에이전트를 시작했지만 연결되지 않았습니다"))
}

/// Ensure the agent is reachable: try connecting, and if that fails, auto-spawn
/// it and wait. Best-effort — the UI still loads (showing "연결 끊김") on failure.
fn ensure_agent() -> Result<()> {
    if connect().is_ok() {
        return Ok(());
    }
    spawn_agent_and_wait()
}

// ============================================================ login autostart

/// Build an `AutoLaunch` targeting the **agent** binary (so sync survives reboot
/// with no GUI). App name is "CopySync". We pass the agent `serve` arg so the
/// boot launch behaves like our own auto-spawn. The `AutoLaunchBuilder` papers
/// over the per-OS `AutoLaunch::new` signature differences for us.
fn build_autolaunch() -> Result<auto_launch::AutoLaunch> {
    let agent = agent_path()?;
    let path = agent
        .to_str()
        .context("agent path is not valid UTF-8")?
        .to_string();
    AutoLaunchBuilder::new()
        .set_app_name("CopySync")
        .set_app_path(&path)
        .set_args(&["serve"])
        .build()
        .map_err(|e| anyhow!("autostart 구성 실패: {e}"))
}

/// Whether the agent is currently registered to launch at login. Any error
/// (e.g. unreadable registry/desktop entry) is treated as "not enabled".
fn autostart_enabled() -> bool {
    build_autolaunch()
        .and_then(|al| al.is_enabled().map_err(|e| anyhow!("{e}")))
        .unwrap_or(false)
}

/// Enable or disable the login-autostart entry for the agent.
fn set_autostart(on: bool) -> Result<()> {
    let al = build_autolaunch()?;
    if on {
        al.enable().map_err(|e| anyhow!("autostart 활성화 실패: {e}"))
    } else {
        al.disable().map_err(|e| anyhow!("autostart 비활성화 실패: {e}"))
    }
}

// ============================================================ system tray

/// Stable string ids for the tray menu items. `MenuEvent.id` is a `MenuId`
/// wrapping a string, so we compare against these to dispatch.
const TRAY_OPEN_ID: &str = "cs.tray.open";
const TRAY_QUIT_ID: &str = "cs.tray.quit";

/// A 16×16 solid-teal RGBA square — a no-asset tray icon. `from_rgba` wants a
/// flat `width*height*4` byte buffer (RGBA), so we just fill one.
fn tray_icon_image() -> Result<Icon> {
    const W: u32 = 16;
    const H: u32 = 16;
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..(W * H) {
        // CopySync teal (matches the "연결됨" green-ish brand tone), fully opaque.
        rgba.extend_from_slice(&[0x14, 0xB8, 0xA6, 0xFF]);
    }
    Icon::from_rgba(rgba, W, H).map_err(|e| anyhow!("tray icon: {e}"))
}

/// Build the tray icon + its "열기 / 종료" menu. Must be called *after* the event
/// loop is live (on Windows the tray needs the message pump), so we do it lazily
/// on the first `update()`. The two `MenuItem`s are kept alive inside the
/// returned `Menu` (set on the builder), so we don't need to store them.
fn build_tray() -> Result<TrayIcon> {
    let menu = Menu::new();
    let open = MenuItem::with_id(TRAY_OPEN_ID, "열기", true, None);
    let quit = MenuItem::with_id(TRAY_QUIT_ID, "종료", true, None);
    menu.append(&open)
        .map_err(|e| anyhow!("tray menu(open): {e}"))?;
    menu.append(&quit)
        .map_err(|e| anyhow!("tray menu(quit): {e}"))?;
    TrayIconBuilder::new()
        .with_tooltip("CopySync")
        .with_icon(tray_icon_image()?)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| anyhow!("tray build: {e}"))
}

/// The background event thread. Connects, `Subscribe`s, and forwards every pushed
/// `Event` onto `tx`, requesting a UI repaint after each. On any disconnect it
/// emits a synthetic `Reconnect` note, sleeps 1s, and reconnects forever.
fn event_loop(tx: Sender<Event>, ctx: egui::Context) {
    loop {
        match subscribe_stream(&tx, &ctx) {
            Ok(()) => { /* clean EOF — fall through to reconnect */ }
            Err(e) => {
                let _ = tx.send(Event::Reconnect {
                    info: format!("이벤트 연결 끊김: {e}"),
                });
                ctx.request_repaint();
            }
        }
        std::thread::sleep(Duration::from_secs(1));
        // If the agent died, try to bring it back before reconnecting.
        let _ = ensure_agent();
    }
}

/// One subscribed session: connect, send `Subscribe`, then loop reading pushed
/// `Event` lines until EOF/error. Replies to our own `Subscribe` (an `Ok`) are
/// simply ignored — we only forward `Outbound::Event`.
fn subscribe_stream(tx: &Sender<Event>, ctx: &egui::Context) -> Result<()> {
    let conn = connect()?;
    let mut reader = BufReader::new(conn);
    let line = serde_json::to_string(&Request::Subscribe)? + "\n";
    reader.get_mut().write_all(line.as_bytes())?;
    reader.get_mut().flush()?;

    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            return Ok(()); // EOF — agent closed the stream
        }
        match serde_json::from_str::<Outbound>(buf.trim()) {
            Ok(Outbound::Event(ev)) => {
                if tx.send(ev).is_err() {
                    return Ok(()); // UI gone
                }
                ctx.request_repaint();
            }
            Ok(Outbound::Reply(_)) => { /* ack to Subscribe — ignore */ }
            Err(_) => { /* skip a malformed line, keep streaming */ }
        }
    }
}

// ============================================================ persisted prefs

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ThemeMode {
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeMode {
    fn as_str(self) -> &'static str {
        match self {
            ThemeMode::System => "system",
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "dark" => ThemeMode::Dark,
            "light" => ThemeMode::Light,
            _ => ThemeMode::System,
        }
    }
}

/// The quick-panel global hotkey we register by default. Parsed via
/// `HotKey::from_str`, which accepts the same "Ctrl+Shift+V" spelling.
const DEFAULT_HOTKEY: &str = "Ctrl+Shift+V";

/// The handful of GUI-local prefs we persist (everything else lives in the
/// agent's config). Stored as a tiny JSON blob at
/// `dirs::config_dir()/copysync/gui.json`.
#[derive(Clone)]
struct Prefs {
    theme: ThemeMode,
    /// Accelerator string for the quick-panel hotkey, e.g. "Ctrl+Shift+V".
    hotkey: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            hotkey: DEFAULT_HOTKEY.to_string(),
        }
    }
}

fn prefs_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("copysync").join("gui.json"))
}

/// Load both prefs (theme + hotkey) from disk, falling back to defaults for any
/// missing/garbled field. Backwards-compatible with the old theme-only file.
fn load_prefs() -> Prefs {
    let mut prefs = Prefs::default();
    let Some(p) = prefs_path() else { return prefs };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return prefs;
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(t) = v.get("theme").and_then(|t| t.as_str()) {
            prefs.theme = ThemeMode::from_str(t);
        }
        if let Some(h) = v.get("hotkey").and_then(|h| h.as_str()) {
            if !h.trim().is_empty() {
                prefs.hotkey = h.to_string();
            }
        }
    }
    prefs
}

/// Persist the full prefs blob (theme + hotkey). Called on any settings change.
fn save_prefs(prefs: &Prefs) {
    let Some(p) = prefs_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({
        "theme": prefs.theme.as_str(),
        "hotkey": prefs.hotkey,
    });
    let _ = std::fs::write(&p, serde_json::to_vec_pretty(&body).unwrap_or_default());
}

// ============================================================ app state

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connect,
    History,
    Settings,
    Debug,
}

/// A blocked-clip toast: the mapped Korean reason chip + the preview, with a
/// spawn time so we can auto-dismiss after ~5s.
struct Toast {
    reason: String,
    preview: String,
    spawned: Instant,
}

/// Map the engine's raw sensitivity label to its Korean chip text. The raw
/// strings come straight from `copysync_core::privacy::Sensitivity::label()`.
fn reason_ko(label: &str) -> &str {
    match label {
        "password-like" => "비밀번호로 추정",
        "payment card" => "카드번호로 추정",
        "private key" => "개인 키",
        "OTP secret" => "OTP/2FA 비밀",
        "custom pattern" => "사용자 패턴",
        other => other,
    }
}

struct App {
    // live state mirrored from the agent
    status: Status,
    history: Vec<HistRow>,
    roster: Vec<RosterDevice>,

    // event plumbing
    events_rx: Receiver<Event>,

    // debug log
    log: VecDeque<String>,
    recording: bool,

    // transient toasts
    toasts: Vec<Toast>,

    // ui inputs
    tab: Tab,
    send_text: String,
    search_query: String,
    target_all: bool,
    selected_targets: Vec<String>,

    // pairing inputs
    pair_server: String,
    pair_otp: String,
    pair_name: String,
    pair_pin: String,
    pair_e2e: String,
    pair_status: String,

    // settings mirror (seeded from status / toggled locally)
    privacy_filter: bool,
    mark_sensitive: bool,
    auto_clear_secs: u64,

    // discovery worker channel (DiscoverServers runs off-thread, ~2.5s)
    discovered: Vec<FoundServer>,
    discover_rx: Option<Receiver<Result<Vec<FoundServer>, String>>>,
    discovering: bool,

    // theme
    theme_mode: ThemeMode,

    // ---- M4 desktop shell ----
    // The tray is created lazily on the first `update()` (Windows needs the
    // message loop running first); `None` until then. Owning it here keeps it
    // alive for the app's lifetime — dropping a `TrayIcon` removes the icon.
    tray: Option<TrayIcon>,
    // Global-hotkey manager + the currently-registered `HotKey` (so we can
    // unregister it before swapping to a new one). Dropping the manager
    // unregisters everything, so it must live in `App`.
    hotkey_mgr: Option<GlobalHotKeyManager>,
    hotkey_current: Option<HotKey>,
    // The accelerator string shown/edited in 설정, e.g. "Ctrl+Shift+V".
    hotkey_input: String,
    // Mirror of the OS login-autostart state for the agent (checkbox value).
    autostart_on: bool,
    // Set when the user picks the real-exit path (tray 종료); on the next frame
    // `update()` issues `ViewportCommand::Close` so the window can actually quit.
    really_quit: bool,

    // one-time post-connect bootstrap (initial history/roster pull)
    bootstrapped: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, events_rx: Receiver<Event>) -> Self {
        let prefs = load_prefs();
        let theme_mode = prefs.theme;

        // Spin up the global-hotkey manager and register the saved (or default)
        // accelerator. All of this is best-effort: a failure just leaves the
        // quick-panel hotkey inactive and is surfaced in the debug log below.
        let mut init_log: Vec<String> = Vec::new();
        let (hotkey_mgr, hotkey_current) = match GlobalHotKeyManager::new() {
            Ok(mgr) => match HotKey::from_str(&prefs.hotkey) {
                Ok(hk) => match mgr.register(hk) {
                    Ok(()) => (Some(mgr), Some(hk)),
                    Err(e) => {
                        init_log.push(format!("단축키 등록 실패: {e}"));
                        (Some(mgr), None)
                    }
                },
                Err(e) => {
                    init_log.push(format!("단축키 파싱 실패 ({}): {e}", prefs.hotkey));
                    (Some(mgr), None)
                }
            },
            Err(e) => {
                init_log.push(format!("단축키 매니저 생성 실패: {e}"));
                (None, None)
            }
        };

        let autostart_on = autostart_enabled();

        let mut app = Self {
            status: Status::default(),
            history: Vec::new(),
            roster: Vec::new(),
            events_rx,
            log: VecDeque::with_capacity(800),
            recording: false,
            toasts: Vec::new(),
            tab: Tab::Connect,
            send_text: String::new(),
            search_query: String::new(),
            target_all: true,
            selected_targets: Vec::new(),
            pair_server: String::new(),
            pair_otp: String::new(),
            pair_name: "desktop".into(),
            pair_pin: String::new(),
            pair_e2e: String::new(),
            pair_status: String::new(),
            privacy_filter: true,
            mark_sensitive: false,
            auto_clear_secs: 0,
            discovered: Vec::new(),
            discover_rx: None,
            discovering: false,
            theme_mode,
            tray: None,
            hotkey_mgr,
            hotkey_current,
            hotkey_input: prefs.hotkey,
            autostart_on,
            really_quit: false,
            bootstrapped: false,
        };
        // Stash any init warnings so they show up once recording is on / forced.
        for line in init_log {
            app.logline(true, line);
        }
        app.apply_theme(&cc.egui_ctx);
        app
    }

    /// Re-derive a `Prefs` snapshot from the current UI state for persistence.
    fn prefs_snapshot(&self) -> Prefs {
        Prefs {
            theme: self.theme_mode,
            hotkey: self.hotkey_input.clone(),
        }
    }

    /// Bring the window back to the foreground (tray "열기", left-click, or the
    /// quick-panel hotkey). Un-hides it and asks the WM for focus.
    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Apply a new accelerator string: parse it, unregister the old hotkey, and
    /// register the new one. On success updates `hotkey_current`/`hotkey_input`
    /// and persists prefs. On failure the previous binding stays active.
    fn apply_hotkey(&mut self, spec: &str) {
        let Some(mgr) = self.hotkey_mgr.as_ref() else {
            self.logline(true, "단축키 매니저가 없어 변경할 수 없습니다");
            return;
        };
        let hk = match HotKey::from_str(spec.trim()) {
            Ok(hk) => hk,
            Err(e) => {
                self.logline(true, format!("단축키 파싱 실패 ({spec}): {e}"));
                return;
            }
        };
        // Unregister the old one first; ignore errors (it may not be live).
        if let Some(old) = self.hotkey_current.take() {
            let _ = mgr.unregister(old);
        }
        match mgr.register(hk) {
            Ok(()) => {
                self.hotkey_current = Some(hk);
                self.hotkey_input = spec.trim().to_string();
                save_prefs(&self.prefs_snapshot());
                self.logline(true, format!("단축키 변경: {}", self.hotkey_input));
            }
            Err(e) => {
                self.logline(true, format!("단축키 등록 실패: {e}"));
            }
        }
    }

    /// Drain the three global desktop-shell channels (tray menu, tray icon,
    /// global hotkey) and act on them. Polled every `update()`.
    fn pump_shell_events(&mut self, ctx: &egui::Context) {
        // Tray context-menu clicks (열기 / 종료).
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.as_ref() {
                TRAY_OPEN_ID => self.show_window(ctx),
                TRAY_QUIT_ID => {
                    // Real exit: flag it so `update()` issues Close next frame.
                    self.really_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.request_repaint();
                }
                _ => {}
            }
        }
        // Tray icon clicks — a left-button release brings the window up.
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                self.show_window(ctx);
            }
        }
        // Global quick-panel hotkey: only act on key-down (Pressed) so we don't
        // fire twice per press. Land on the 기록 tab and surface the window.
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state == HotKeyState::Pressed {
                self.tab = Tab::History;
                self.reload_history();
                self.show_window(ctx);
            }
        }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        match self.theme_mode {
            ThemeMode::Dark => ctx.set_visuals(egui::Visuals::dark()),
            ThemeMode::Light => ctx.set_visuals(egui::Visuals::light()),
            ThemeMode::System => {
                // Follow the OS where eframe can tell us; default dark otherwise.
                let dark = ctx.style().visuals.dark_mode;
                ctx.set_visuals(if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                });
            }
        }
    }

    fn logline(&mut self, force: bool, msg: impl Into<String>) {
        if !force && !self.recording {
            return;
        }
        let now = chrono_lite_now();
        self.log.push_back(format!("[{now}] {}", msg.into()));
        while self.log.len() > 800 {
            self.log.pop_front();
        }
    }

    /// Pull the latest history list from the agent into `self.history`.
    fn reload_history(&mut self) {
        let q = if self.search_query.trim().is_empty() {
            None
        } else {
            Some(self.search_query.trim().to_string())
        };
        match request_ok(&Request::GetHistory { query: q }) {
            Ok(Response::History(rows)) => self.history = rows,
            Ok(_) => {}
            Err(e) => self.logline(true, format!("기록 조회 실패: {e}")),
        }
    }

    fn reload_status(&mut self) {
        if let Ok(Response::Status(s)) = request(&Request::GetStatus) {
            self.adopt_status(s);
        }
    }

    fn reload_roster(&mut self) {
        if let Ok(Response::Roster(r)) = request(&Request::GetRoster) {
            self.roster = r;
        }
    }

    /// Take a fresh `Status` and seed the local settings mirror + pool combo.
    fn adopt_status(&mut self, s: Status) {
        self.status = s;
    }

    /// Drain pushed events from the agent and fold them into local state.
    fn pump_events(&mut self) {
        let mut events = Vec::new();
        while let Ok(ev) = self.events_rx.try_recv() {
            events.push(ev);
        }
        for ev in events {
            match ev {
                Event::Status(s) => {
                    self.logline(false, "상태 갱신");
                    self.adopt_status(s);
                }
                Event::Roster(r) => {
                    self.logline(false, format!("로스터 갱신 ({}개)", r.len()));
                    self.roster = r;
                }
                Event::Clip(info) => {
                    if let Some(reason) = info.sensitive.clone() {
                        // Blocked outbound clip → pink toast + always-log.
                        let preview = info
                            .text
                            .clone()
                            .or_else(|| info.name.clone())
                            .unwrap_or_default();
                        self.logline(true, format!("차단된 클립: {reason}"));
                        self.toasts.push(Toast {
                            reason: reason_ko(&reason).to_string(),
                            preview,
                            spawned: Instant::now(),
                        });
                    } else {
                        self.logline(false, format!("클립 {} {}", info.direction, info.kind));
                        // A new clip likely changed history → refresh on the
                        // 기록 tab; cheap local-socket call.
                        if self.tab == Tab::History {
                            self.reload_history();
                        }
                    }
                }
                Event::Reconnect { info } => {
                    self.logline(true, format!("재연결: {info}"));
                }
                Event::Error { message } => {
                    self.logline(true, format!("오류: {message}"));
                }
                Event::Notify { title, body } => {
                    self.logline(false, format!("알림: {title} — {body}"));
                }
                Event::Cliplog { msg } => {
                    self.logline(false, msg);
                }
            }
        }
    }

    /// Kick off mDNS discovery on a worker thread; the result lands on a channel.
    fn start_discovery(&mut self) {
        if self.discovering {
            return;
        }
        self.discovering = true;
        self.pair_status = "서버 검색 중…".into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.discover_rx = Some(rx);
        std::thread::spawn(move || {
            let result = match request(&Request::DiscoverServers) {
                Ok(Response::Found(list)) => Ok(list),
                Ok(Response::Error { message }) => Err(message),
                Ok(_) => Err("예상치 못한 응답".to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        });
    }

    fn poll_discovery(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.discover_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(list)) => {
                let n = list.len();
                self.discovered = list;
                self.discovering = false;
                self.discover_rx = None;
                self.pair_status = format!("{n}개 서버 발견");
                ctx.request_repaint();
            }
            Ok(Err(e)) => {
                self.discovering = false;
                self.discover_rx = None;
                self.pair_status = format!("검색 실패: {e}");
                self.logline(true, format!("서버 검색 실패: {e}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.discovering = false;
                self.discover_rx = None;
            }
        }
    }
}

/// A tiny wall-clock "HH:MM:SS" stamp without pulling in the `chrono` crate.
fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

// ============================================================ eframe::App

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Create the tray lazily on the first frame: on Windows the tray needs a
        // running message loop, which only exists once eframe's event loop is up
        // (i.e. inside `update`), not at construction time. Best-effort.
        if self.tray.is_none() {
            match build_tray() {
                Ok(tray) => self.tray = Some(tray),
                Err(e) => self.logline(true, format!("트레이 생성 실패: {e}")),
            }
        }

        // One-time bootstrap: pull initial status/history/roster after the first
        // frame (the event thread will keep them fresh from here on).
        if !self.bootstrapped {
            self.bootstrapped = true;
            self.reload_status();
            self.reload_roster();
            self.reload_history();
            self.privacy_filter = true;
            self.logline(true, "GUI 시작");
        }

        // Poll the three desktop-shell channels (tray menu/icon + global hotkey).
        self.pump_shell_events(ctx);

        // Real-exit path: the tray "종료" set `really_quit`, so close the window
        // for good. The agent keeps running — only the GUI process exits.
        if self.really_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if ctx.input(|i| i.viewport().close_requested()) {
            // Close-to-tray: the user clicked the window's X. Cancel the close
            // and just hide the window; reopen via tray "열기" or the hotkey.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        self.pump_events();
        self.poll_discovery(ctx);

        // Keep `update()` ticking while idle so the tray/hotkey channels above
        // are polled even with no input events (otherwise egui sleeps).
        ctx.request_repaint_after(Duration::from_millis(200));

        // Expire toasts older than ~5s.
        self.toasts.retain(|t| t.spawned.elapsed() < Duration::from_secs(5));
        if !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.top_bar(ctx);

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Connect => self.tab_connect(ui),
            Tab::History => self.tab_history(ui),
            Tab::Settings => self.tab_settings(ui, ctx),
            Tab::Debug => self.tab_debug(ui),
        });

        self.blocked_toasts(ctx);
    }
}

// ============================================================ UI: top bar

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("CopySync");
                ui.add_space(8.0);

                // Connection pill.
                let (label, color) = if self.status.connected {
                    ("연결됨", Color32::from_rgb(34, 197, 94))
                } else if self.status.reconnecting {
                    ("재연결 중", Color32::from_rgb(234, 179, 8))
                } else {
                    ("연결 끊김", Color32::from_rgb(239, 68, 68))
                };
                egui::Frame::none()
                    .fill(color.gamma_multiply(0.25))
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(8.0, 2.0))
                    .show(ui, |ui| {
                        ui.colored_label(color, label);
                    });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.tab_button(ui, Tab::Debug, "🐞 디버깅");
                    self.tab_button(ui, Tab::Settings, "⚙️ 설정");
                    self.tab_button(ui, Tab::History, "📋 기록");
                    self.tab_button(ui, Tab::Connect, "🔗 연결");
                });
            });
        });
    }

    fn tab_button(&mut self, ui: &mut egui::Ui, tab: Tab, label: &str) {
        if ui.selectable_label(self.tab == tab, label).clicked() {
            self.tab = tab;
            if tab == Tab::History {
                self.reload_history();
            }
        }
    }
}

// ============================================================ UI: 연결

impl App {
    fn tab_connect(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ---- 상태
            ui.group(|ui| {
                ui.label(egui::RichText::new("상태").strong());
                egui::Grid::new("status_grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                    ui.label("서버");
                    ui.label(value_or_dash(&self.status.server));
                    ui.end_row();
                    ui.label("기기");
                    ui.label(value_or_dash(&self.status.device));
                    ui.end_row();
                    ui.label("연결");
                    ui.label(if self.status.connected { "연결됨" } else { "끊김" });
                    ui.end_row();
                    ui.label("E2E");
                    ui.label(if self.status.e2e { "켜짐" } else { "꺼짐" });
                    ui.end_row();
                });
                if ui.button("지금 재연결").clicked() {
                    match request_ok(&Request::Reconnect) {
                        Ok(_) => self.logline(true, "재연결 요청"),
                        Err(e) => self.logline(true, format!("재연결 실패: {e}")),
                    }
                }
            });

            ui.add_space(8.0);

            // ---- pool
            if !self.status.pools.is_empty() {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("공유 풀").strong());
                    let mut selected = self.status.pool.clone();
                    egui::ComboBox::from_id_salt("pool_combo")
                        .selected_text(value_or_dash(&selected))
                        .show_ui(ui, |ui| {
                            for p in &self.status.pools {
                                ui.selectable_value(&mut selected, p.clone(), p);
                            }
                        });
                    if selected != self.status.pool {
                        match request_ok(&Request::SetPool { pool: selected.clone() }) {
                            Ok(_) => {
                                self.logline(true, format!("풀 변경: {selected}"));
                                self.status.pool = selected;
                            }
                            Err(e) => self.logline(true, format!("풀 변경 실패: {e}")),
                        }
                    }
                });
                ui.add_space(8.0);
            }

            // ---- 보내기
            ui.group(|ui| {
                ui.label(egui::RichText::new("보내기").strong());
                ui.add(
                    egui::TextEdit::multiline(&mut self.send_text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("보낼 텍스트…"),
                );
                ui.horizontal(|ui| {
                    if ui.button("텍스트 보내기").clicked() && !self.send_text.trim().is_empty() {
                        let text = self.send_text.clone();
                        match request_ok(&Request::SendText { text }) {
                            Ok(_) => {
                                self.logline(true, "텍스트 전송");
                                self.send_text.clear();
                            }
                            Err(e) => self.logline(true, format!("전송 실패: {e}")),
                        }
                    }
                    if ui.button("파일 보내기…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            let p = path.to_string_lossy().to_string();
                            match request_ok(&Request::SendFile { path: p.clone() }) {
                                Ok(_) => self.logline(true, format!("파일 전송: {p}")),
                                Err(e) => self.logline(true, format!("파일 전송 실패: {e}")),
                            }
                        }
                    }
                });
            });

            ui.add_space(8.0);

            // ---- 전송 대상
            ui.group(|ui| {
                ui.label(egui::RichText::new("전송 대상").strong());
                let mut changed = false;
                ui.horizontal(|ui| {
                    changed |= ui.radio_value(&mut self.target_all, true, "전체").changed();
                    changed |= ui.radio_value(&mut self.target_all, false, "선택").changed();
                });
                if !self.target_all {
                    if self.roster.is_empty() {
                        ui.weak("연결된 기기가 없습니다.");
                    }
                    for dev in &self.roster {
                        let checked = self.selected_targets.contains(&dev.id);
                        let mut now = checked;
                        ui.horizontal(|ui| {
                            let dot = if dev.online {
                                Color32::from_rgb(34, 197, 94)
                            } else {
                                Color32::from_gray(120)
                            };
                            ui.colored_label(dot, "●");
                            if ui.checkbox(&mut now, &dev.name).changed() {
                                changed = true;
                            }
                        });
                        if now && !checked {
                            self.selected_targets.push(dev.id.clone());
                        } else if !now && checked {
                            self.selected_targets.retain(|id| id != &dev.id);
                        }
                    }
                }
                if changed {
                    let ids = if self.target_all {
                        Vec::new()
                    } else {
                        self.selected_targets.clone()
                    };
                    match request_ok(&Request::SetTargets { ids }) {
                        Ok(_) => self.logline(false, "전송 대상 변경"),
                        Err(e) => self.logline(true, format!("대상 변경 실패: {e}")),
                    }
                }
            });
        });
    }
}

// ============================================================ UI: 기록

impl App {
    fn tab_history(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("검색…")
                    .desired_width(220.0),
            );
            if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui.button("새로고침").clicked()
            {
                self.reload_history();
            }
        });
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.history.is_empty() {
                ui.weak("기록이 없습니다.");
            }
            // Clone for borrow simplicity; lists are capped at 200 rows.
            let rows = self.history.clone();
            for row in &rows {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let (tag, color) = kind_tag(&row.kind);
                        ui.colored_label(color, tag);
                        let body = if row.preview.is_empty() { "(미리보기 없음)" } else { &row.preview };
                        ui.label(body);
                    });
                    let dir = if row.direction == "out" { "보냄" } else { "받음" };
                    let meta = format!(
                        "{dir} · {} · {} · {}",
                        value_or_dash(&row.origin),
                        human_size(row.size),
                        row.ts
                    );
                    ui.weak(meta);
                });
            }
        });
    }
}

// ============================================================ UI: 설정

impl App {
    fn tab_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ---- privacy
            ui.group(|ui| {
                ui.label(egui::RichText::new("개인정보").strong());
                if ui.checkbox(&mut self.privacy_filter, "민감 클립 동기화 제외").changed() {
                    let on = self.privacy_filter;
                    let _ = request_ok(&Request::SetPrivacyFilter { on });
                    self.logline(true, format!("민감 제외: {on}"));
                }
                if ui.checkbox(&mut self.mark_sensitive, "받은 항목 민감 표시").changed() {
                    let on = self.mark_sensitive;
                    let _ = request_ok(&Request::SetMarkSensitive { on });
                    self.logline(true, format!("민감 표시: {on}"));
                }
                ui.add_space(4.0);
                ui.label("자동 지우기");
                ui.horizontal(|ui| {
                    let mut sel = self.auto_clear_secs;
                    let opts = [(0u64, "끔"), (30, "30초"), (60, "1분"), (300, "5분")];
                    for (secs, lbl) in opts {
                        if ui.selectable_label(sel == secs, lbl).clicked() {
                            sel = secs;
                        }
                    }
                    if sel != self.auto_clear_secs {
                        self.auto_clear_secs = sel;
                        let _ = request_ok(&Request::SetAutoClear { secs: sel });
                        self.logline(true, format!("자동 지우기: {sel}초"));
                    }
                });
            });

            ui.add_space(8.0);

            // ---- pairing
            ui.group(|ui| {
                ui.label(egui::RichText::new("기기 페어링").strong());
                egui::Grid::new("pair_grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    ui.label("서버");
                    ui.add(egui::TextEdit::singleline(&mut self.pair_server).hint_text("https://…").desired_width(260.0));
                    ui.end_row();
                    ui.label("OTP");
                    ui.add(egui::TextEdit::singleline(&mut self.pair_otp).desired_width(260.0));
                    ui.end_row();
                    ui.label("기기 이름");
                    ui.add(egui::TextEdit::singleline(&mut self.pair_name).desired_width(260.0));
                    ui.end_row();
                    ui.label("PIN");
                    ui.add(egui::TextEdit::singleline(&mut self.pair_pin).desired_width(260.0));
                    ui.end_row();
                    ui.label("E2E 암호");
                    ui.add(egui::TextEdit::singleline(&mut self.pair_e2e).password(true).desired_width(260.0));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    let busy = self.discovering;
                    if ui.add_enabled(!busy, egui::Button::new("서버 검색")).clicked() {
                        self.start_discovery();
                    }
                    if ui.button("페어링").clicked() {
                        self.do_pair();
                    }
                });
                for srv in &self.discovered.clone() {
                    let label = format!("{} — {}", srv.name, srv.url);
                    if ui.button(label).clicked() {
                        self.pair_server = srv.url.clone();
                    }
                }
                if !self.pair_status.is_empty() {
                    ui.weak(&self.pair_status);
                }
            });

            ui.add_space(8.0);

            // ---- theme
            ui.group(|ui| {
                ui.label(egui::RichText::new("화면").strong());
                ui.horizontal(|ui| {
                    let mut mode = self.theme_mode;
                    let changed = ui.selectable_value(&mut mode, ThemeMode::Dark, "다크").clicked()
                        | ui.selectable_value(&mut mode, ThemeMode::Light, "라이트").clicked()
                        | ui.selectable_value(&mut mode, ThemeMode::System, "시스템").clicked();
                    if changed && mode != self.theme_mode {
                        self.theme_mode = mode;
                        self.apply_theme(ctx);
                        save_prefs(&self.prefs_snapshot());
                    }
                });
            });

            ui.add_space(8.0);

            // ---- desktop shell (autostart + quick-panel hotkey)
            ui.group(|ui| {
                ui.label(egui::RichText::new("시스템").strong());

                // Login autostart — targets the *agent*, so sync runs at boot
                // with no GUI. Reflects the OS state; errors go to the log.
                let mut on = self.autostart_on;
                if ui.checkbox(&mut on, "부팅 시 자동 시작").changed() {
                    match set_autostart(on) {
                        Ok(()) => {
                            self.autostart_on = on;
                            self.logline(true, format!("자동 시작: {on}"));
                        }
                        Err(e) => {
                            // Leave the checkbox reflecting the real state.
                            self.autostart_on = autostart_enabled();
                            self.logline(true, format!("자동 시작 변경 실패: {e}"));
                        }
                    }
                }

                ui.add_space(4.0);
                ui.label("빠른 패널 단축키");
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.hotkey_input)
                            .hint_text("예: Ctrl+Shift+V")
                            .desired_width(180.0),
                    );
                    let commit = (r.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || ui.button("적용").clicked();
                    if commit {
                        let spec = self.hotkey_input.clone();
                        self.apply_hotkey(&spec);
                    }
                });
                let active = match &self.hotkey_current {
                    Some(_) => format!("활성: {}", self.hotkey_input),
                    None => "단축키 비활성 (등록 안 됨)".to_string(),
                };
                ui.weak(active);
            });
        });
    }

    fn do_pair(&mut self) {
        let req = Request::Pair {
            server: self.pair_server.trim().to_string(),
            otp: self.pair_otp.trim().to_string(),
            name: self.pair_name.trim().to_string(),
            pin: self.pair_pin.trim().to_string(),
            e2e_pass: self.pair_e2e.clone(),
        };
        match request(&req) {
            Ok(Response::Paired(s)) => {
                self.pair_status = "페어링 성공".into();
                self.logline(true, "페어링 성공");
                self.adopt_status(s);
                self.reload_roster();
            }
            Ok(Response::Error { message }) => {
                self.pair_status = format!("페어링 실패: {message}");
                self.logline(true, format!("페어링 실패: {message}"));
            }
            Ok(_) => self.pair_status = "예상치 못한 응답".into(),
            Err(e) => {
                self.pair_status = format!("페어링 실패: {e}");
                self.logline(true, format!("페어링 실패: {e}"));
            }
        }
    }
}

// ============================================================ UI: 디버깅

impl App {
    fn tab_debug(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.recording, "이벤트 기록");
            if ui.button("복사").clicked() {
                let joined = self.log.iter().cloned().collect::<Vec<_>>().join("\n");
                ui.output_mut(|o| o.copied_text = joined);
            }
            if ui.button("지우기").clicked() {
                self.log.clear();
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.log {
                    ui.monospace(line);
                }
            });
    }
}

// ============================================================ UI: pink toast

impl App {
    fn blocked_toasts(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let screen = ctx.screen_rect();
        // Render the newest toast bottom-centered; stack older ones above it.
        for (i, toast) in self.toasts.iter().rev().enumerate() {
            let y = screen.bottom() - 70.0 - (i as f32) * 86.0;
            let pos = egui::pos2(screen.center().x, y);
            egui::Area::new(egui::Id::new(("blocked_toast", i)))
                .fixed_pos(egui::pos2(pos.x - 180.0, pos.y))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::none()
                        .fill(Color32::from_rgba_unmultiplied(236, 72, 153, 55))
                        .rounding(10.0)
                        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                        .show(ui, |ui| {
                            ui.set_width(332.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("🔒").size(18.0));
                                ui.label(egui::RichText::new("동기화 차단됨").strong());
                                egui::Frame::none()
                                    .fill(Color32::from_rgba_unmultiplied(236, 72, 153, 110))
                                    .rounding(6.0)
                                    .inner_margin(egui::Margin::symmetric(6.0, 1.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(&toast.reason).small());
                                    });
                            });
                            if !toast.preview.is_empty() {
                                let preview = truncate(&toast.preview, 80);
                                ui.weak(preview);
                            }
                        });
                });
        }
    }
}

// ============================================================ small helpers

fn value_or_dash(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

fn kind_tag(kind: &str) -> (&'static str, Color32) {
    match kind {
        "image" => ("🖼 이미지", Color32::from_rgb(168, 85, 247)),
        "file" => ("📎 파일", Color32::from_rgb(59, 130, 246)),
        _ => ("📝 텍스트", Color32::from_rgb(34, 197, 94)),
    }
}

fn human_size(bytes: i64) -> String {
    if bytes <= 0 {
        return "0 B".into();
    }
    let b = bytes as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

// ============================================================ main

fn main() -> eframe::Result<()> {
    // Best-effort: make sure the agent is up before the window opens. The UI
    // still launches (showing "연결 끊김") if this fails.
    if let Err(e) = ensure_agent() {
        eprintln!("copysync-gui: agent not reachable: {e}");
    }

    let (events_tx, events_rx) = std::sync::mpsc::channel::<Event>();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([420.0, 480.0])
            .with_title("CopySync"),
        ..Default::default()
    };

    eframe::run_native(
        "CopySync",
        options,
        Box::new(move |cc| {
            // Start the event thread now that we hold an egui Context to repaint.
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || event_loop(events_tx, ctx));
            Ok(Box::new(App::new(cc, events_rx)))
        }),
    )
}
