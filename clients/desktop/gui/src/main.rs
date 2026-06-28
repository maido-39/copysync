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
use egui::{Color32, LayerId, Margin, Rounding, Stroke};
use egui::epaint::Shadow;

use copysync_ipc::{
    socket_label, Event, FoundServer, HistRow, Outbound, Request, Response, RosterDevice, Status,
};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, Stream};

// ============================================================ crash/debug log
//
// The GUI compiles with `windows_subsystem = "windows"` in release, so it has NO
// console: any `eprintln!` is invisible and a panic kills `gui.exe` with no
// window and no trace. To make launch failures *analyzable* we (1) append every
// diagnostic line to a persistent file at
// `dirs::config_dir()/copysync/logs/gui.log`, and (2) install a panic hook that
// records the panic + a full backtrace there and, on Windows, pops a MessageBox
// so a user actually sees the failure. See `install_crash_handler`.

/// Resolve the GUI crash/debug log path: `dirs::config_dir()/copysync/logs/gui.log`.
/// Falls back to the system temp dir if there's no config dir, so we *always*
/// have somewhere to write.
fn gui_log_path() -> std::path::PathBuf {
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    base.join("copysync").join("logs").join("gui.log")
}

/// Append one timestamped line to `gui.log`, creating the directory if needed.
/// Best-effort and panic-free (it's called from the panic hook): every error is
/// swallowed because there is nowhere safer to report a logging failure.
fn gui_log_line(msg: &str) {
    let path = gui_log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // `chrono_lite_now()` is wall-clock HH:MM:SS only; good enough to order
        // events within a session without pulling in a date crate.
        let _ = writeln!(f, "[{}] {}", chrono_lite_now(), msg);
    }
}

/// Minimal `user32!MessageBoxW` FFI so we can surface a fatal error to the user
/// on Windows *without* adding a heavy new crate. Linked only on Windows; a
/// no-op everywhere else. `title`/`body` are shown UTF-16-encoded, NUL-terminated.
#[cfg(windows)]
fn message_box(title: &str, body: &str) {
    // MB_OK | MB_ICONERROR | MB_SETFOREGROUND so the dialog steals focus even
    // though the main window never opened.
    const MB_OK: u32 = 0x0000_0000;
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            utype: u32,
        ) -> i32;
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let body_w = wide(body);
    let title_w = wide(title);
    // SAFETY: both buffers are valid, NUL-terminated UTF-16; `hwnd` null is the
    // documented "no owner window" value.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
fn message_box(_title: &str, _body: &str) {}

/// Install a process-wide panic hook that appends the panic message, source
/// location, and a full `std::backtrace::Backtrace` to `gui.log`, then (on
/// Windows) pops a MessageBox so the user sees that the GUI died. Chains the
/// previous hook so default stderr behavior is preserved in debug builds.
///
/// Call this as the *very first* thing in `main()` so even a panic during
/// option/window construction is captured.
fn install_crash_handler() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        // `Backtrace::force_capture` ignores RUST_BACKTRACE so we always get a
        // trace in the file even when the env var is unset.
        let bt = std::backtrace::Backtrace::force_capture();
        let full = format!(
            "PANIC at {location}: {payload}\nbacktrace:\n{bt}"
        );
        gui_log_line(&full);
        message_box("CopySync GUI failed", &format!("{payload}\n\n위치: {location}\n\n자세한 내용: {}", gui_log_path().display()));
        // Preserve prior behavior (stderr in debug, etc.).
        previous(info);
    }));
}

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
    let mut cmd = std::process::Command::new(&agent);
    cmd.arg("serve");
    // Don't pop a console window for the headless agent. It's a console binary so
    // its CLI subcommands keep stdout when run from a terminal — only THIS
    // GUI-spawned daemon is created windowless (CREATE_NO_WINDOW = 0x08000000).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn()
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
        // CopySync teal ACCENT (#0E8C84 — the actions/active token, distinct from
        // the green "연결됨" success tone), fully opaque.
        rgba.extend_from_slice(&[0x0E, 0x8C, 0x84, 0xFF]);
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
        // Each iteration ends by sending a synthetic `Reconnect` heartbeat. If that
        // send fails, the App (and its `Receiver`) has been dropped — e.g. the
        // window closed, or a renderer attempt that already spawned us was torn down
        // and main() retried with another backend. A failed send is the one reliable
        // signal that this thread is now orphaned, so we `return` instead of looping
        // forever (which would keep calling `ensure_agent()` and respawn the agent).
        let info = match subscribe_stream(&tx, &ctx) {
            Ok(()) => "이벤트 연결 재시도".to_string(), // clean EOF — reconnect
            Err(e) => format!("이벤트 연결 끊김: {e}"),
        };
        if tx.send(Event::Reconnect { info }).is_err() {
            return; // UI gone — stop this (possibly orphaned) thread.
        }
        ctx.request_repaint();
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

// ============================================================ design palette
//
// "Warm Trust, Verified" design system (see clients/desktop/DESIGN.md). Every
// token is resolved to an `egui::Color32` for the *active* theme; depth is three
// flat surface tiers (`bg` < `panel` < `panel2`) fenced by 1px `line` borders,
// no gradient/shadow/blur anywhere. Colors are the only thing this struct owns —
// shapes (pills/tags/toast) read these tokens at their call sites.

/// `0xRRGGBB` → opaque `Color32`. A tiny const helper so the token table below
/// reads like the DESIGN.md hex table.
const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Inverse of [`hex`]: pack a `Color32`'s RGB into a `0xRRGGBB` `u32`, dropping
/// alpha. Used to persist the accent/fg color-picker overrides in `Prefs`.
fn rgb24(c: Color32) -> u32 {
    ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32
}

/// Tint a token toward transparency by a linear alpha fraction (0..=1). Used for
/// the spec's "@22%/@18%/@45%" fill/stroke tints — `from_rgba_unmultiplied`
/// keeps the source hue and just lowers coverage so it composites over whatever
/// tier sits behind it.
fn tint(c: Color32, alpha: f32) -> Color32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// The resolved token set for one theme (dark or light). Built once per
/// `apply_theme` and re-read every frame from a fresh `Palette::resolve(...)`.
#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    bg: Color32,
    panel: Color32,
    panel2: Color32,
    line: Color32,
    fg: Color32,
    muted: Color32,
    accent: Color32,
    success: Color32,
    danger: Color32,
    warn: Color32,
    tag_text: Color32,
    tag_image: Color32,
    tag_file: Color32,
    /// Pre-composited opaque toast surface (`bg`-side pink) + its title/body inks.
    toast_fill: Color32,
    /// Toast border stroke (opaque so it's const-constructible; dark = pink@55%
    /// pre-composited over `toast_fill`, light = solid `#E0588F`).
    toast_stroke: Color32,
    toast_title: Color32,
    toast_body: Color32,
    /// `blocked_pink` base — used only for the reason chip fill inside the toast.
    blocked_pink: Color32,
    /// On-accent ink for the one place teal carries a label (primary buttons).
    on_accent: Color32,
}

impl Palette {
    const DARK: Palette = Palette {
        dark: true,
        bg: hex(0x1A1816),
        panel: hex(0x222019),
        panel2: hex(0x2B2823),
        line: hex(0x3A362F),
        fg: hex(0xEDE8DF),
        muted: hex(0xA39C8E),
        accent: hex(0x0E8C84),
        success: hex(0x34D399),
        danger: hex(0xF87171),
        warn: hex(0xFBBF24),
        tag_text: hex(0x3FB94F),
        tag_image: hex(0xA78BFA),
        tag_file: hex(0x5B9CF6),
        // DESIGN.md §3 toast: opaque #482B31 backing (pink@20% over panel),
        // stroke blocked_pink@55%, title #FBCFE8, body = fg. The stroke is
        // pre-composited (#E0588F@55% over #482B31 = #9C4465) so it stays const.
        toast_fill: hex(0x482B31),
        toast_stroke: hex(0x9C4465),
        toast_title: hex(0xFBCFE8),
        toast_body: hex(0xEDE8DF),
        blocked_pink: hex(0xE0588F),
        on_accent: Color32::WHITE,
    };

    const LIGHT: Palette = Palette {
        dark: false,
        bg: hex(0xF6F3EC),
        panel: hex(0xFFFFFF),
        panel2: hex(0xEFEBE1),
        line: hex(0xDAD3C5),
        fg: hex(0x23201B),
        muted: hex(0x6E685C),
        accent: hex(0x0B7C72),
        success: hex(0x15803D),
        danger: hex(0xC81E1E),
        warn: hex(0x9A6406),
        tag_text: hex(0x117A33),
        tag_image: hex(0x7E22CE),
        tag_file: hex(0x1D4ED8),
        // Light toast must be near-solid (not translucent): solid #FDE7F1 fill,
        // 1px #E0588F stroke, title #9D174D, body #831843.
        toast_fill: hex(0xFDE7F1),
        toast_stroke: hex(0xE0588F),
        toast_title: hex(0x9D174D),
        toast_body: hex(0x831843),
        blocked_pink: hex(0xE0588F),
        on_accent: Color32::WHITE,
    };

    /// Resolve "system" the way the old `apply_theme` did — read the current
    /// `dark_mode` flag off the context's visuals (eframe seeds this from the OS
    /// where it can). Dark/Light pick their table directly.
    fn resolve(mode: ThemeMode, ctx: &egui::Context) -> Palette {
        let dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => ctx.style().visuals.dark_mode,
        };
        if dark {
            Palette::DARK
        } else {
            Palette::LIGHT
        }
    }

    /// Tag color for a history `kind`, in the active theme's hue set.
    fn tag_color(&self, kind: &str) -> Color32 {
        match kind {
            "image" => self.tag_image,
            "file" => self.tag_file,
            _ => self.tag_text,
        }
    }

    /// Status pill / dot color for the connection state.
    fn status_color(&self, connected: bool, reconnecting: bool) -> Color32 {
        if connected {
            self.success
        } else if reconnecting {
            self.warn
        } else {
            self.danger
        }
    }

    /// Build the global `egui::Visuals` for this palette per DESIGN.md §3.
    fn visuals(&self) -> egui::Visuals {
        let mut v = if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };

        let line_stroke = Stroke::new(1.0, self.line);
        let fg_stroke = Stroke::new(1.0, self.fg);
        let rounding = Rounding::same(8.0);

        v.dark_mode = self.dark;
        v.override_text_color = Some(self.fg);
        v.window_fill = self.bg;
        v.panel_fill = self.bg;
        v.extreme_bg_color = self.panel2; // input backgrounds
        v.faint_bg_color = self.panel2;
        v.window_stroke = line_stroke;
        v.window_rounding = rounding;
        v.window_shadow = Shadow::NONE;
        v.popup_shadow = Shadow::NONE;

        // tier-1 surfaces (cards/top bar): panel fill, 1px line, fg text
        v.widgets.noninteractive.bg_fill = self.panel;
        v.widgets.noninteractive.weak_bg_fill = self.panel;
        v.widgets.noninteractive.bg_stroke = line_stroke;
        v.widgets.noninteractive.fg_stroke = fg_stroke;
        v.widgets.noninteractive.rounding = rounding;

        // tier-2 interactive idle (inputs, default buttons): panel2 fill, 1px line
        v.widgets.inactive.bg_fill = self.panel2;
        v.widgets.inactive.weak_bg_fill = self.panel2;
        v.widgets.inactive.bg_stroke = line_stroke;
        v.widgets.inactive.fg_stroke = fg_stroke;
        v.widgets.inactive.rounding = rounding;

        // hover: border picks up the accent (teal@45%), fill steps a touch
        v.widgets.hovered.bg_fill = self.panel2;
        v.widgets.hovered.weak_bg_fill = self.panel2;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, tint(self.accent, 0.45));
        v.widgets.hovered.fg_stroke = fg_stroke;
        v.widgets.hovered.rounding = rounding;

        // active/pressed: solid accent fill + on-accent label
        v.widgets.active.bg_fill = self.accent;
        v.widgets.active.weak_bg_fill = self.accent;
        v.widgets.active.bg_stroke = Stroke::new(1.0, self.accent);
        v.widgets.active.fg_stroke = Stroke::new(1.0, self.on_accent);
        v.widgets.active.rounding = rounding;

        // open combo/menu surface: step back to panel tier
        v.widgets.open.bg_fill = self.panel;
        v.widgets.open.weak_bg_fill = self.panel;
        v.widgets.open.bg_stroke = line_stroke;
        v.widgets.open.fg_stroke = fg_stroke;
        v.widgets.open.rounding = rounding;

        // selection / text highlight: accent@16% fill, 1px accent stroke
        v.selection.bg_fill = tint(self.accent, 0.16);
        v.selection.stroke = Stroke::new(1.0, self.accent);

        v
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
    /// Absolute path to the background wallpaper image, or empty for none.
    bg_path: String,
    /// Wallpaper zoom (1.0 = cover the panel). Mirrors the web "확대" slider.
    bg_zoom: f32,
    /// Brightness multiplier applied to the wallpaper pixels (1.0 = unchanged).
    bg_brightness: f32,
    /// Gaussian blur sigma in pixels applied once at decode time (0 = none).
    bg_blur: f32,
    /// Card/panel fill opacity (1.0 = opaque). Lowered so the wallpaper shows
    /// through the cards; only meaningful when a wallpaper is set.
    card_opacity: f32,
    /// Optional accent (테마색) override as `0xRRGGBB`. `None` = use the built-in
    /// palette accent. When set it replaces `Palette::accent` and every hue
    /// derived from it (hover/selection tints) in both dark and light themes.
    accent_rgb: Option<u32>,
    /// Optional foreground (글자색) override as `0xRRGGBB`. `None` = built-in
    /// palette `fg`. When set it replaces `Palette::fg` (the primary text ink).
    fg_rgb: Option<u32>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            hotkey: DEFAULT_HOTKEY.to_string(),
            // Mirrors the web client's `themeDefaults`.
            bg_path: String::new(),
            bg_zoom: 1.0,
            bg_brightness: 1.0,
            bg_blur: 0.0,
            card_opacity: 1.0,
            accent_rgb: None,
            fg_rgb: None,
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
        // Wallpaper controls — each is optional and falls back to its default,
        // so an old theme-only/hotkey-only file still loads cleanly.
        if let Some(p) = v.get("bg_path").and_then(|p| p.as_str()) {
            prefs.bg_path = p.to_string();
        }
        if let Some(z) = v.get("bg_zoom").and_then(|z| z.as_f64()) {
            prefs.bg_zoom = z as f32;
        }
        if let Some(b) = v.get("bg_brightness").and_then(|b| b.as_f64()) {
            prefs.bg_brightness = b as f32;
        }
        if let Some(b) = v.get("bg_blur").and_then(|b| b.as_f64()) {
            prefs.bg_blur = b as f32;
        }
        if let Some(o) = v.get("card_opacity").and_then(|o| o.as_f64()) {
            prefs.card_opacity = o as f32;
        }
        // Color overrides — optional; absent in old files → None (built-in
        // palette). Stored as a plain JSON number (0xRRGGBB); we mask to 24 bits
        // so a stray alpha byte can't leak in.
        if let Some(c) = v.get("accent_rgb").and_then(|c| c.as_u64()) {
            prefs.accent_rgb = Some((c as u32) & 0x00FF_FFFF);
        }
        if let Some(c) = v.get("fg_rgb").and_then(|c| c.as_u64()) {
            prefs.fg_rgb = Some((c as u32) & 0x00FF_FFFF);
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
        "bg_path": prefs.bg_path,
        "bg_zoom": prefs.bg_zoom,
        "bg_brightness": prefs.bg_brightness,
        "bg_blur": prefs.bg_blur,
        "card_opacity": prefs.card_opacity,
        // `Option<u32>` → number or `null`; `load_prefs` reads it back via
        // `as_u64()`, so a `null` (or missing key) cleanly maps back to `None`.
        "accent_rgb": prefs.accent_rgb,
        "fg_rgb": prefs.fg_rgb,
    });
    let _ = std::fs::write(&p, serde_json::to_vec_pretty(&body).unwrap_or_default());
}

// ============================================================ background image
//
// Mirrors the web/Android clients' 화면 wallpaper feature. The picked image is
// decoded once, brightness-scaled, gaussian-blurred, and uploaded as a single
// egui texture cached here; we only rebuild it when the path / brightness / blur
// change (NOT per-frame — those ops are far too costly for the render loop). The
// `zoom` and `card_opacity` are cheap, applied live at paint time, so they don't
// trigger a rebuild.

/// Cap the decoded wallpaper's longest edge so a huge photo doesn't blow up GPU
/// memory or the one-off blur cost (the web client caps at 1600 too).
const BG_MAX_EDGE: u32 = 1600;

/// Alpha of the readability scrim painted over the wallpaper and under all content
/// (the egui twin of the web client's `--scrim`). At ~0.55 the theme background
/// reads clearly behind translucent cards so `fg`/`muted` text stays legible over
/// a busy image even at low card opacity, while the wallpaper is still visible.
const SCRIM_ALPHA: f32 = 0.55;

/// All wallpaper state: the persisted controls plus the lazily-built texture and
/// the inputs it was built from (so we can detect when a rebuild is needed).
struct BgImage {
    path: String,
    zoom: f32,
    brightness: f32,
    blur: f32,
    card_opacity: f32,
    /// The cached processed texture (None when there's no/failed image).
    texture: Option<egui::TextureHandle>,
    /// The (path, brightness, blur) the current `texture` was built from. A change
    /// vs. the live controls means we must rebuild; zoom/opacity are excluded
    /// because they're applied at paint time, not baked into the texture.
    built_from: Option<(String, u32, u32)>,
}

impl BgImage {
    fn from_prefs(p: &Prefs) -> Self {
        Self {
            path: p.bg_path.clone(),
            zoom: p.bg_zoom,
            brightness: p.bg_brightness,
            blur: p.bg_blur,
            card_opacity: p.card_opacity,
            texture: None,
            built_from: None,
        }
    }

    /// A stable key for the current decode inputs. Floats are quantized to whole
    /// units (brightness ×100, blur ×10) so tiny slider jitter doesn't thrash the
    /// rebuild, matching how the sliders step.
    fn decode_key(&self) -> (String, u32, u32) {
        (
            self.path.clone(),
            (self.brightness.clamp(0.0, 4.0) * 100.0).round() as u32,
            (self.blur.clamp(0.0, 40.0) * 10.0).round() as u32,
        )
    }

    /// Whether the cached texture still matches the live decode inputs.
    fn needs_rebuild(&self) -> bool {
        match (&self.built_from, self.path.is_empty()) {
            // No image wanted: stale only if a texture is still cached.
            (_, true) => self.texture.is_some(),
            // Image wanted: rebuild if never built or inputs changed.
            (None, false) => true,
            (Some(key), false) => *key != self.decode_key(),
        }
    }

    /// Clear the path + cached texture ("제거").
    fn clear(&mut self) {
        self.path.clear();
        self.texture = None;
        self.built_from = None;
    }
}

/// Decode `path`, apply `brightness` then a one-off gaussian `blur`, and return a
/// `(rgba_bytes, [w,h])` pair ready for `ColorImage::from_rgba_unmultiplied`.
/// Returns `Err` (never panics) on a missing/unreadable/undecodable file so the
/// caller can just log it and show no wallpaper. Runs entirely on `image`'s pure-
/// Rust PNG/JPEG codecs — no system libs, so it cross-compiles to windows-gnu.
fn decode_wallpaper(path: &str, brightness: f32, blur: f32) -> Result<(Vec<u8>, [usize; 2])> {
    let img = image::ImageReader::open(path)
        .with_context(|| format!("배경 이미지 열기 실패: {path}"))?
        .with_guessed_format()
        .context("배경 이미지 형식 추정 실패")?
        .decode()
        .context("배경 이미지 디코드 실패")?;

    // Downscale the longest edge to BG_MAX_EDGE to bound texture/blur cost.
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    let mut rgba = if longest > BG_MAX_EDGE {
        let scale = BG_MAX_EDGE as f32 / longest as f32;
        let nw = (w as f32 * scale).round().max(1.0) as u32;
        let nh = (h as f32 * scale).round().max(1.0) as u32;
        image::imageops::resize(&img.to_rgba8(), nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img.to_rgba8()
    };

    // Brightness: scale each RGB channel (leave alpha), saturating at 255. 1.0 is
    // a no-op so we skip the per-pixel loop in the common case.
    if (brightness - 1.0).abs() > f32::EPSILON {
        for px in rgba.pixels_mut() {
            for c in 0..3 {
                px[c] = (px[c] as f32 * brightness).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    // Gaussian blur ONCE here (not per-frame). `blur` is the sigma in px.
    if blur > 0.05 {
        rgba = image::imageops::blur(&rgba, blur);
    }

    let dims = [rgba.width() as usize, rgba.height() as usize];
    Ok((rgba.into_raw(), dims))
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
    // pairing worker channel (Pair runs off-thread like DiscoverServers, since
    // the agent's Pair handler does a real network round-trip that can hang on a
    // slow/unreachable server — running it on the UI thread freezes the GUI).
    pair_rx: Option<Receiver<Result<Status, String>>>,

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
    // Optional accent (테마색) / foreground (글자색) overrides as `0xRRGGBB`.
    // `None` = use the built-in palette color. Applied in `palette()` so every
    // card/button/input that reads `pal.accent`/`pal.fg` picks them up, in both
    // dark and light. Edited via the color pickers in 설정→화면.
    accent_override: Option<u32>,
    fg_override: Option<u32>,

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

    // ---- background wallpaper (parity with the web/Android clients) ----
    // Persisted controls + the cached, pre-processed texture. The texture is
    // (re)built ONLY when an input below changes (see `refresh_wallpaper`), never
    // per-frame — decode + brightness + gaussian blur are too costly for that.
    bg: BgImage,

    // one-time post-connect bootstrap (initial history/roster pull)
    bootstrapped: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, events_rx: Receiver<Event>) -> Self {
        // Korean (CJK) glyphs: register the embedded font before the first frame,
        // otherwise the whole UI renders as □ tofu (egui's fonts are Latin-only).
        install_fonts(&cc.egui_ctx);

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
            pair_rx: None,
            privacy_filter: true,
            mark_sensitive: false,
            auto_clear_secs: 0,
            discovered: Vec::new(),
            discover_rx: None,
            discovering: false,
            theme_mode,
            accent_override: prefs.accent_rgb,
            fg_override: prefs.fg_rgb,
            tray: None,
            hotkey_mgr,
            hotkey_current,
            hotkey_input: prefs.hotkey.clone(),
            autostart_on,
            bg: BgImage::from_prefs(&prefs),
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
            bg_path: self.bg.path.clone(),
            bg_zoom: self.bg.zoom,
            bg_brightness: self.bg.brightness,
            bg_blur: self.bg.blur,
            card_opacity: self.bg.card_opacity,
            accent_rgb: self.accent_override,
            fg_rgb: self.fg_override,
        }
    }

    /// Bring the window back to the foreground (tray "열기", left-click, or the
    /// quick-panel hotkey). Un-hides it and asks the WM for focus.
    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Full quit (tray "종료"): tell the agent to shut down (best-effort), then
    /// hard-exit this process. We use `process::exit` rather than the normal
    /// `ViewportCommand::Close` because eframe leaves daemon threads (the event
    /// thread, tray, and global-hotkey pumps) alive after `run_native` returns,
    /// which is exactly why `copysync-gui` was lingering in the background.
    /// `process::exit(0)` guarantees every thread is torn down.
    fn really_quit(&mut self) {
        self.logline(true, "종료 요청 — 에이전트 정지 후 GUI 종료");
        // Best-effort, time-bounded: ask the agent to stop on a detached thread
        // and exit after a short deadline regardless of the reply. `request()`
        // blocks on connect+read with no timeout, so a wedged/unresponsive agent
        // must never be able to stall the hard exit on the UI thread.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = request(&Request::Shutdown);
            let _ = tx.send(());
        });
        // The agent acks Ok quickly (its own exit is deferred ~150ms), so this
        // window is ample for the happy path while bounding the wedged case.
        let _ = rx.recv_timeout(Duration::from_millis(300));
        std::process::exit(0);
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
                TRAY_QUIT_ID => self.really_quit(),
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

    /// Whether a wallpaper texture is actually loaded this frame. Gates every
    /// wallpaper-only effect (translucent surfaces, the scrim) so that with no
    /// image the UI is byte-for-byte the original opaque look.
    fn has_wallpaper(&self) -> bool {
        !self.bg.path.is_empty() && self.bg.texture.is_some()
    }

    /// The resolved "Warm Trust" palette for the current theme mode, plus the two
    /// runtime customizations the user can apply in 설정→화면:
    ///
    ///   * **Color overrides** — `accent_override`/`fg_override` replace
    ///     `Palette::accent`/`fg`. Because every hover/selection tint is derived at
    ///     its call site via `tint(pal.accent, ..)`, overriding `accent` here also
    ///     recolors all of those. Works in dark *and* light (applied post-resolve).
    ///   * **Card opacity** — when a wallpaper is loaded we lower the alpha of the
    ///     surface tokens the cards/inputs/top-bar/secondary-buttons fill with
    ///     (`panel`, `panel2`) so the image shows through the cards themselves. This
    ///     is the single source of truth: `apply_theme` builds its `Visuals` from
    ///     this same (already-translucent) palette, so opacity is applied exactly
    ///     once across both the hand-painted frames and egui's widget fills.
    ///
    /// `bg` is intentionally NOT alpha-reduced — it's the opaque scrim color (see
    /// `paint_backdrop`). Cheap enough to call per-frame from every UI builder.
    fn palette(&self, ctx: &egui::Context) -> Palette {
        let mut pal = Palette::resolve(self.theme_mode, ctx);

        // Color overrides (independent of any wallpaper).
        if let Some(rgb) = self.accent_override {
            pal.accent = hex(rgb);
        }
        if let Some(rgb) = self.fg_override {
            pal.fg = hex(rgb);
        }

        // Card opacity — only when a wallpaper is up *and* the user asked for
        // less than full opacity; otherwise the surfaces stay fully opaque.
        if self.has_wallpaper() && self.bg.card_opacity < 0.999 {
            let a = (self.bg.card_opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
            let translucent = |c: Color32| {
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
            };
            pal.panel = translucent(pal.panel);
            pal.panel2 = translucent(pal.panel2);
        }
        pal
    }

    /// Install the custom `Visuals` for the active theme (DESIGN.md §3), replacing
    /// the raw `dark()/light()` defaults. Called on theme change and every frame
    /// from `update()` (the build is cheap and keeps "system" tracking the OS).
    /// The palette it builds from already carries the color overrides and (when a
    /// wallpaper is set) the `card_opacity`-reduced `panel`/`panel2` alpha, so the
    /// egui widget fills line up with the hand-painted card frames automatically —
    /// no separate translucency pass here.
    fn apply_theme(&self, ctx: &egui::Context) {
        let pal = self.palette(ctx);
        ctx.set_visuals(pal.visuals());
    }

    /// Rebuild the cached wallpaper texture if (and only if) its decode inputs
    /// changed since last time — debounced by `BgImage::needs_rebuild`, so slider
    /// drags recompute on change, never per-frame. A missing/unreadable image just
    /// logs and leaves no wallpaper; it never panics.
    fn refresh_wallpaper(&mut self, ctx: &egui::Context) {
        if !self.bg.needs_rebuild() {
            return;
        }
        if self.bg.path.is_empty() {
            self.bg.texture = None;
            self.bg.built_from = None;
            return;
        }
        let key = self.bg.decode_key();
        match decode_wallpaper(&self.bg.path, self.bg.brightness, self.bg.blur) {
            Ok((rgba, dims)) => {
                let img = egui::ColorImage::from_rgba_unmultiplied(dims, &rgba);
                let tex = ctx.load_texture("cs_wallpaper", img, egui::TextureOptions::LINEAR);
                self.bg.texture = Some(tex);
                self.bg.built_from = Some(key);
                self.logline(false, format!("배경 이미지 적용 {}×{}", dims[0], dims[1]));
            }
            Err(e) => {
                // Robust: drop the broken image, log, show no wallpaper.
                self.bg.texture = None;
                self.bg.built_from = Some(key); // don't retry until inputs change
                self.logline(true, format!("배경 이미지 실패: {e}"));
            }
        }
    }

    /// Paint the cached wallpaper across `rect`, honoring `zoom`, centered, then a
    /// semi-opaque scrim of the theme background color ON TOP of it. No-op when no
    /// texture is loaded.
    ///
    /// The scrim is the egui equivalent of the web client's `--scrim`: a solid
    /// `pal.bg` layer at [`SCRIM_ALPHA`] sitting between the wallpaper and all
    /// content. Because every translucent surface (the now-`card_opacity`-reduced
    /// cards/top-bar/inputs and the transparent central panel) composites over this
    /// scrim, `fg`/`muted` text stays readable at ANY card opacity over any image —
    /// the previously "washed-out" un-scrimmed regions are exactly what it fixes.
    ///
    /// Called against the *background* layer painter over the full screen rect so it
    /// covers the top bar, central area, and cards uniformly. Paint order is
    /// wallpaper → scrim → (egui draws panels/cards/content on top).
    fn paint_backdrop(&self, painter: &egui::Painter, rect: egui::Rect, pal: &Palette) {
        let Some(tex) = self.bg.texture.as_ref() else {
            return;
        };
        let tex_size = tex.size_vec2();
        if tex_size.x <= 0.0 || tex_size.y <= 0.0 {
            return;
        }
        // "cover" the rect: scale so the image fills it on both axes, then apply the
        // user zoom on top. UVs are derived from the centered overlap so we crop
        // (not letterbox) — matching the web client's background-size/cover.
        let base = (rect.width() / tex_size.x).max(rect.height() / tex_size.y);
        let scale = base * self.bg.zoom.max(0.05);
        let drawn = tex_size * scale; // image size in screen px at this scale
        // Fraction of the image actually visible inside `rect`, on each axis.
        let u = (rect.width() / drawn.x).min(1.0);
        let v = (rect.height() / drawn.y).min(1.0);
        // Center the crop window.
        let uv = egui::Rect::from_min_max(
            egui::pos2(0.5 - u / 2.0, 0.5 - v / 2.0),
            egui::pos2(0.5 + u / 2.0, 0.5 + v / 2.0),
        );
        painter.image(tex.id(), rect, uv, Color32::WHITE);

        // Readability scrim: solid theme background over the whole image. `pal.bg`
        // is the OPAQUE table color (never alpha-reduced by `palette()`), so we
        // tint it here to `SCRIM_ALPHA`.
        let scrim = Color32::from_rgba_unmultiplied(
            pal.bg.r(),
            pal.bg.g(),
            pal.bg.b(),
            (SCRIM_ALPHA * 255.0).round() as u8,
        );
        painter.rect_filled(rect, Rounding::ZERO, scrim);
    }

    fn logline(&mut self, force: bool, msg: impl Into<String>) {
        if !force && !self.recording {
            return;
        }
        let msg = msg.into();
        // Mirror the always-important (`force`) lines — init warnings
        // (hotkey/tray/autostart failures), errors, and lifecycle events — to the
        // persistent gui.log immediately, so they survive past the in-memory
        // Debug tab (and exist even if the Debug tab is never opened). The full
        // verbose stream (`recording`) is mirrored too so the file is a complete
        // diagnostic when the user has recording on.
        gui_log_line(&msg);
        let now = chrono_lite_now();
        self.log.push_back(format!("[{now}] {}", msg));
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
        // Rebuild the wallpaper texture first if its decode inputs changed (debounced
        // — never per-frame). Done before `apply_theme` so the card-opacity decision
        // below sees whether a wallpaper is actually loaded this frame.
        self.refresh_wallpaper(ctx);

        // Re-assert the "Warm Trust" visuals every frame. It's cheap (a const
        // palette pick) and keeps "system" mode tracking the OS dark/light flag,
        // which eframe can flip out from under us between frames. When a wallpaper
        // is set it also lowers card/panel fill alpha by `card_opacity`.
        self.apply_theme(ctx);

        // Wallpaper + scrim backdrop: painted ONCE into the background layer over
        // the whole window, beneath every panel. The top bar and central panel draw
        // into higher content layers, so their (now translucent) fills composite
        // over this — giving the cards, top bar, and central area one uniform
        // wallpaper+scrim backdrop. No-op (and zero cost) when no wallpaper is set.
        if self.has_wallpaper() {
            let pal = self.palette(ctx);
            let screen = ctx.screen_rect();
            let painter = ctx.layer_painter(LayerId::background());
            self.paint_backdrop(&painter, screen, &pal);
        }

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
        // The tray "종료" handler calls `really_quit()`, which `process::exit`s
        // directly, so there's no quit flag to check here.
        self.pump_shell_events(ctx);

        // Close-to-tray: the user clicked the window's X. Cancel the close and
        // just hide the window — sync keeps running via the agent. Reopen via the
        // tray ("열기" / left-click) or the quick-panel hotkey. A plain X must NOT
        // end the process; only the tray "종료" does.
        if ctx.input(|i| i.viewport().close_requested()) {
            // Only hide-to-tray if there's a live way back: a tray icon
            // ("열기"/left-click) OR a registered global hotkey. Otherwise the X
            // must really quit — hiding with no affordance would leave an
            // invisible process that can only be killed from a task manager.
            // Re-checked every frame (not cached) because the tray can come up
            // late on Windows.
            if self.tray.is_some() || self.hotkey_current.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.really_quit();
            }
        }

        self.pump_events();
        self.poll_discovery(ctx);
        self.poll_pair(ctx);

        // Keep `update()` ticking while idle so the tray/hotkey channels above
        // are polled even with no input events (otherwise egui sleeps).
        ctx.request_repaint_after(Duration::from_millis(200));

        // Expire toasts older than ~5s.
        self.toasts.retain(|t| t.spawned.elapsed() < Duration::from_secs(5));
        if !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.top_bar(ctx);

        // When a wallpaper is loaded, give the central panel a TRANSPARENT fill so
        // the backdrop (wallpaper + scrim, painted into the background layer above)
        // shows through behind the cards; without one we keep egui's opaque panel
        // fill (the original look).
        let central = if self.has_wallpaper() {
            egui::CentralPanel::default().frame(egui::Frame::none())
        } else {
            egui::CentralPanel::default()
        };
        central.show(ctx, |ui| {
            match self.tab {
                Tab::Connect => self.tab_connect(ui),
                Tab::History => self.tab_history(ui),
                Tab::Settings => self.tab_settings(ui, ctx),
                Tab::Debug => self.tab_debug(ui),
            }
        });

        self.blocked_toasts(ctx);
    }
}

// ============================================================ UI: components
//
// Shared "Warm Trust" component frames. Each takes the active `Palette` and
// paints the spec's exact tier/fill/stroke recipe so the call sites stay thin.

impl App {
    /// A tier-1 card: `panel` fill, 1px `line`, rounding 10, inner margin 14/12.
    /// Nested interactive surfaces inside it step up to `panel2` (the tier law).
    /// Associated (no `self`) so call-site closures can freely borrow `&mut self`.
    fn card<R>(
        ui: &mut egui::Ui,
        pal: &Palette,
        add: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        egui::Frame::none()
            .fill(pal.panel)
            .stroke(Stroke::new(1.0, pal.line))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::symmetric(14.0, 12.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui)
            })
            .inner
    }

    /// Card section title — the one strong-fg line per card.
    fn card_title(ui: &mut egui::Ui, pal: &Palette, text: &str) {
        ui.label(
            egui::RichText::new(text)
                .strong()
                .size(14.0)
                .color(pal.fg),
        );
        ui.add_space(6.0);
    }

    /// Primary action button — teal fill + on-accent (white) label, rounding 8.
    /// One per tab (텍스트 보내기 / 페어링 / 지금 재연결). This is the only place teal
    /// touches a label.
    fn primary_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> egui::Response {
        let btn = egui::Button::new(
            egui::RichText::new(label)
                .strong()
                .size(13.0)
                .color(pal.on_accent),
        )
        .fill(pal.accent)
        .stroke(Stroke::new(1.0, pal.accent))
        .rounding(Rounding::same(8.0));
        ui.add(btn)
    }

    /// Secondary button — `panel2` fill + 1px `line`, fg label. This is the egui
    /// default look (set in `visuals`), so plain `ui.button(..)` already matches;
    /// this helper just pins the radius/strong-label for the explicit cases.
    fn secondary_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> egui::Response {
        let btn = egui::Button::new(
            egui::RichText::new(label).size(13.0).color(pal.fg),
        )
        .fill(pal.panel2)
        .stroke(Stroke::new(1.0, pal.line))
        .rounding(Rounding::same(8.0));
        ui.add(btn)
    }

    /// Ghost button — transparent fill, no border, fg label. For low-stakes
    /// toolbar actions (디버깅 복사/지우기).
    fn ghost_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> egui::Response {
        let btn = egui::Button::new(
            egui::RichText::new(label).size(13.0).color(pal.fg),
        )
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::NONE)
        .rounding(Rounding::same(8.0));
        ui.add(btn)
    }

    /// One item in a segmented control (자동 지우기 / 화면 / 전송 대상). Selected =
    /// accent@18% fill + strong fg label + 1px accent@45% stroke; unselected =
    /// transparent + muted label with egui's built-in hover highlight. Returns the
    /// clickable `Response`; callers keep their own selection logic.
    fn seg_item(
        ui: &mut egui::Ui,
        pal: &Palette,
        selected: bool,
        label: &str,
    ) -> egui::Response {
        if selected {
            egui::Frame::none()
                .fill(tint(pal.accent, 0.18))
                .stroke(Stroke::new(1.0, tint(pal.accent, 0.45)))
                .rounding(Rounding::same(8.0))
                .inner_margin(Margin::symmetric(10.0, 4.0))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(label).strong().color(pal.fg),
                        )
                        .sense(egui::Sense::click()),
                    )
                })
                .inner
        } else {
            // Transparent, clickable; muted by default. `selectable_label(false,..)`
            // gives a transparent ground + egui's built-in hover highlight, and we
            // tint the text muted so it reads as the inactive segment.
            ui.selectable_label(false, egui::RichText::new(label).color(pal.muted))
        }
    }

    /// The connection status pill: dot + word. An 8px filled circle in
    /// statusColor + the label in statusColor, on a statusColor@~20% frame with a
    /// 1px statusColor@~50% stroke, rounding 8.
    fn status_pill(ui: &mut egui::Ui, pal: &Palette, label: &str, color: Color32) {
        let fill_a = if pal.dark { 0.22 } else { 0.16 };
        egui::Frame::none()
            .fill(tint(color, fill_a))
            .stroke(Stroke::new(1.0, tint(color, 0.50)))
            .rounding(Rounding::same(8.0))
            .inner_margin(Margin::symmetric(9.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    // 8px filled dot, vertically centered with the label.
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.0, color);
                    ui.label(
                        egui::RichText::new(label).size(12.5).color(color),
                    );
                });
            });
    }

    /// A kind-tag chip: tinted frame (tagColor@18% dark / @14% light), rounding
    /// 8, small inner margin, label in tagColor at ~11.5px medium.
    fn kind_chip(ui: &mut egui::Ui, pal: &Palette, kind: &str) {
        let color = pal.tag_color(kind);
        let label = kind_tag_label(kind);
        let fill_a = if pal.dark { 0.18 } else { 0.14 };
        egui::Frame::none()
            .fill(tint(color, fill_a))
            .rounding(Rounding::same(8.0))
            .inner_margin(Margin::symmetric(7.0, 2.0))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(label).size(11.5).color(color));
            });
    }
}

// ============================================================ UI: top bar

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        let pal = self.palette(ctx);
        // Top bar lives on the `panel` tier with a 1px bottom `line`.
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::none()
                    .fill(pal.panel)
                    .stroke(Stroke::new(1.0, pal.line))
                    .inner_margin(Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Wordmark (display 20).
                    ui.label(
                        egui::RichText::new("CopySync")
                            .size(20.0)
                            .strong()
                            .color(pal.fg),
                    );
                    ui.add_space(8.0);

                    // Connection status pill (dot + word) — read first.
                    let label = if self.status.connected {
                        "연결됨"
                    } else if self.status.reconnecting {
                        "재연결 중"
                    } else {
                        "연결 끊김"
                    };
                    let color =
                        pal.status_color(self.status.connected, self.status.reconnecting);
                    Self::status_pill(ui, &pal, label, color);

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            // Text-only labels: egui's bundled emoji font renders
                            // the U+FE0F variation selector (and several emoji) as a
                            // □ tofu, which showed up before "설정"/etc. Plain Korean
                            // glyphs (covered by the embedded Nanum font) never tofu.
                            self.tab_button(ui, &pal, Tab::Debug, "디버깅");
                            self.tab_button(ui, &pal, Tab::Settings, "설정");
                            self.tab_button(ui, &pal, Tab::History, "기록");
                            self.tab_button(ui, &pal, Tab::Connect, "연결");
                        },
                    );
                });
            });
    }

    fn tab_button(&mut self, ui: &mut egui::Ui, pal: &Palette, tab: Tab, label: &str) {
        let selected = self.tab == tab;
        // Selected = fg label; inactive = muted, brightening to fg on hover.
        let color = if selected { pal.fg } else { pal.muted };
        let resp = ui.selectable_label(
            selected,
            egui::RichText::new(label).size(14.0).color(color),
        );
        // 2px teal underline on the active tab — the only accent-touched nav.
        if selected {
            let r = resp.rect;
            let y = r.bottom() + 1.0;
            ui.painter().hline(
                r.left()..=r.right(),
                y,
                Stroke::new(2.0, pal.accent),
            );
        }
        if resp.clicked() {
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
        let pal = self.palette(ui.ctx());
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ---- 상태
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "상태");
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
                ui.add_space(8.0);
                if Self::primary_button(ui, &pal, "지금 재연결").clicked() {
                    match request_ok(&Request::Reconnect) {
                        Ok(_) => self.logline(true, "재연결 요청"),
                        Err(e) => self.logline(true, format!("재연결 실패: {e}")),
                    }
                }
            });

            ui.add_space(12.0);

            // ---- pool
            if !self.status.pools.is_empty() {
                Self::card(ui, &pal, |ui| {
                    Self::card_title(ui, &pal, "공유 풀");
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
                ui.add_space(12.0);
            }

            // ---- 보내기
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "보내기");
                ui.add(
                    egui::TextEdit::multiline(&mut self.send_text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("보낼 텍스트…"),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    // The one primary on this tab.
                    if Self::primary_button(ui, &pal, "텍스트 보내기").clicked()
                        && !self.send_text.trim().is_empty()
                    {
                        let text = self.send_text.clone();
                        match request_ok(&Request::SendText { text }) {
                            Ok(_) => {
                                self.logline(true, "텍스트 전송");
                                self.send_text.clear();
                            }
                            Err(e) => self.logline(true, format!("전송 실패: {e}")),
                        }
                    }
                    if Self::secondary_button(ui, &pal, "파일 보내기…").clicked() {
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

            ui.add_space(12.0);

            // ---- 전송 대상
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "전송 대상");
                let mut changed = false;
                ui.horizontal(|ui| {
                    if Self::seg_item(ui, &pal, self.target_all, "전체").clicked()
                        && !self.target_all
                    {
                        self.target_all = true;
                        changed = true;
                    }
                    if Self::seg_item(ui, &pal, !self.target_all, "선택").clicked()
                        && self.target_all
                    {
                        self.target_all = false;
                        changed = true;
                    }
                });
                if !self.target_all {
                    if self.roster.is_empty() {
                        ui.weak("연결된 기기가 없습니다.");
                    }
                    for dev in &self.roster {
                        let checked = self.selected_targets.contains(&dev.id);
                        let mut now = checked;
                        ui.horizontal(|ui| {
                            // online = success dot, offline = muted dot.
                            let dot = if dev.online { pal.success } else { pal.muted };
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
        let pal = self.palette(ui.ctx());
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("🔍 검색…")
                    .desired_width(220.0),
            );
            if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || Self::secondary_button(ui, &pal, "새로고침").clicked()
            {
                self.reload_history();
            }
        });
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.history.is_empty() {
                ui.add_space(8.0);
                ui.weak("기록이 없습니다.");
            }
            // Clone for borrow simplicity; lists are capped at 200 rows.
            let rows = self.history.clone();
            for row in &rows {
                Self::card(ui, &pal, |ui| {
                    ui.horizontal(|ui| {
                        Self::kind_chip(ui, &pal, &row.kind);
                        let body = if row.preview.is_empty() {
                            "(미리보기 없음)"
                        } else {
                            &row.preview
                        };
                        ui.label(
                            egui::RichText::new(body)
                                .size(14.0)
                                .color(pal.fg),
                        );
                    });
                    // Coded meta: 보냄 in strong-fg, 받음 in muted (NOT teal).
                    let outbound = row.direction == "out";
                    let dir = if outbound { "보냄" } else { "받음" };
                    let dir_rt = if outbound {
                        egui::RichText::new(dir).size(12.5).strong().color(pal.fg)
                    } else {
                        egui::RichText::new(dir).size(12.5).color(pal.muted)
                    };
                    let rest = format!(
                        " · {} · {} · {}",
                        value_or_dash(&row.origin),
                        human_size(row.size),
                        row.ts
                    );
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.label(dir_rt);
                        ui.label(
                            egui::RichText::new(rest).size(12.5).color(pal.muted),
                        );
                    });
                });
                ui.add_space(12.0);
            }
        });
    }
}

// ============================================================ UI: 설정

impl App {
    fn tab_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let pal = self.palette(ctx);
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ---- privacy
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "개인정보");
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
                        if Self::seg_item(ui, &pal, sel == secs, lbl).clicked() {
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

            ui.add_space(12.0);

            // ---- pairing
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "기기 페어링");
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
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let busy = self.discovering;
                    // Discovering disables the search button (desaturated look via
                    // egui's disabled rendering).
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(
                                egui::RichText::new("서버 검색").size(13.0).color(pal.fg),
                            )
                            .fill(pal.panel2)
                            .stroke(Stroke::new(1.0, pal.line))
                            .rounding(Rounding::same(8.0)),
                        )
                        .clicked()
                    {
                        self.start_discovery();
                    }
                    if Self::primary_button(ui, &pal, "페어링").clicked() {
                        self.do_pair();
                    }
                });
                for srv in &self.discovered.clone() {
                    let label = format!("{} — {}", srv.name, srv.url);
                    if Self::secondary_button(ui, &pal, &label).clicked() {
                        self.pair_server = srv.url.clone();
                    }
                }
                if !self.pair_status.is_empty() {
                    ui.weak(&self.pair_status);
                }
            });

            ui.add_space(12.0);

            // ---- theme
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "화면");
                ui.horizontal(|ui| {
                    let mut mode = self.theme_mode;
                    let mut changed = false;
                    if Self::seg_item(ui, &pal, mode == ThemeMode::Dark, "다크").clicked() {
                        mode = ThemeMode::Dark;
                        changed = true;
                    }
                    if Self::seg_item(ui, &pal, mode == ThemeMode::Light, "라이트").clicked() {
                        mode = ThemeMode::Light;
                        changed = true;
                    }
                    if Self::seg_item(ui, &pal, mode == ThemeMode::System, "시스템").clicked() {
                        mode = ThemeMode::System;
                        changed = true;
                    }
                    if changed && mode != self.theme_mode {
                        self.theme_mode = mode;
                        self.apply_theme(ctx);
                        save_prefs(&self.prefs_snapshot());
                    }
                });

                // ---- background wallpaper (parity with the web/Android clients)
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("배경 이미지").size(13.0).color(pal.fg),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if Self::secondary_button(ui, &pal, "배경 이미지 선택…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("이미지", &["png", "jpg", "jpeg"])
                            .pick_file()
                        {
                            self.bg.path = path.to_string_lossy().to_string();
                            // A new image invalidates the cache; refresh_wallpaper
                            // rebuilds on the next frame and persists via snapshot.
                            self.bg.texture = None;
                            self.bg.built_from = None;
                            save_prefs(&self.prefs_snapshot());
                            self.logline(true, "배경 이미지 선택");
                        }
                    }
                    // "제거" only matters when an image is set.
                    if !self.bg.path.is_empty()
                        && Self::secondary_button(ui, &pal, "제거").clicked()
                    {
                        self.bg.clear();
                        save_prefs(&self.prefs_snapshot());
                        self.logline(true, "배경 이미지 제거");
                    }
                });

                // Sliders only make sense once an image is chosen; mirror the web
                // ranges. Persist on release so we don't rewrite the file each frame
                // of a drag. zoom/brightness/blur change the *texture* inputs
                // (debounced rebuild); card_opacity is applied live in `apply_theme`.
                if !self.bg.path.is_empty() {
                    ui.add_space(4.0);
                    // Each row: label + slider. `changed` keeps the frame repainting
                    // (so the debounced texture rebuild / live opacity follow the
                    // drag); `drag_stopped` (release) is when we persist, avoiding a
                    // file write per drag frame.
                    let mut touched = false;
                    let mut released = false;
                    let row = |ui: &mut egui::Ui, label: &str, val: &mut f32,
                                   range: std::ops::RangeInclusive<f32>,
                                   touched: &mut bool, released: &mut bool| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(label).size(12.5).color(pal.muted));
                            let r = ui.add(egui::Slider::new(val, range).show_value(true));
                            if r.changed() {
                                *touched = true;
                            }
                            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                *released = true;
                            }
                        });
                    };
                    row(ui, "확대", &mut self.bg.zoom, 1.0..=3.0, &mut touched, &mut released);
                    row(ui, "밝기", &mut self.bg.brightness, 0.3..=2.0, &mut touched, &mut released);
                    row(ui, "흐림", &mut self.bg.blur, 0.0..=20.0, &mut touched, &mut released);
                    // Min 0.05 (not ~0.2): the readability scrim keeps text legible
                    // even at near-transparent cards, so allow the full range.
                    row(ui, "박스 투명도", &mut self.bg.card_opacity, 0.05..=1.0, &mut touched, &mut released);
                    // Persist when a drag finishes (or on a discrete keyboard step).
                    if released {
                        save_prefs(&self.prefs_snapshot());
                    }
                    // Keep repainting while the user is actively dragging a slider so
                    // the debounced texture rebuild / live opacity track the value.
                    if touched {
                        ctx.request_repaint();
                    }
                }

                // ---- color overrides (테마색 / 글자색)
                // Two egui color pickers that override the palette `accent` and `fg`
                // in BOTH themes. The swatch seeds from the current effective color
                // (override if set, else the built-in for this theme). Picking a
                // color stores the override, re-applies the theme, and persists; the
                // 기본값 button clears both back to the built-in palette.
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("색상").size(13.0).color(pal.fg),
                );
                ui.add_space(4.0);
                // Built-in colors for THIS theme (no overrides) — the fall-back the
                // swatches show when the user hasn't picked a custom color yet.
                let base = Palette::resolve(self.theme_mode, ctx);
                let mut accent_col = match self.accent_override {
                    Some(rgb) => hex(rgb),
                    None => base.accent,
                };
                let mut fg_col = match self.fg_override {
                    Some(rgb) => hex(rgb),
                    None => base.fg,
                };
                // `color_changed` drives the live theme preview every frame the
                // value moves; `color_committed` gates the disk write so we persist
                // only when the edit is released/clicked — not once per drag frame.
                // (Mirrors the wallpaper-slider pattern above, which persists on
                // `drag_stopped` rather than thrashing gui.json each frame.)
                let mut color_changed = false;
                let mut color_committed = false;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("테마색").size(12.5).color(pal.muted));
                    let r = ui.color_edit_button_srgba(&mut accent_col);
                    if r.changed() {
                        // Drop any alpha — the palette accent is always opaque.
                        self.accent_override = Some(rgb24(accent_col));
                        color_changed = true;
                    }
                    // The popup's hue/value pads are click-and-drag sliders, so
                    // `dragged()` is the "still held" signal; `changed() && !dragged()`
                    // is the release frame (and discrete clicks / keyboard steps).
                    if r.changed() && !r.dragged() {
                        color_committed = true;
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("글자색").size(12.5).color(pal.muted));
                    let r = ui.color_edit_button_srgba(&mut fg_col);
                    if r.changed() {
                        self.fg_override = Some(rgb24(fg_col));
                        color_changed = true;
                    }
                    if r.changed() && !r.dragged() {
                        color_committed = true;
                    }
                    ui.add_space(8.0);
                    if (self.accent_override.is_some() || self.fg_override.is_some())
                        && Self::secondary_button(ui, &pal, "기본값").clicked()
                    {
                        self.accent_override = None;
                        self.fg_override = None;
                        color_changed = true;
                        // A discrete click — commit immediately.
                        color_committed = true;
                    }
                });
                if color_changed {
                    // Re-assert visuals so egui's widget fills pick up the new
                    // accent/fg immediately (the hand-painted frames already read the
                    // live palette next frame). Keep this live every frame for preview.
                    self.apply_theme(ctx);
                }
                if color_committed {
                    // Persist only when the edit is committed/released, not per frame.
                    save_prefs(&self.prefs_snapshot());
                }
            });

            ui.add_space(12.0);

            // ---- desktop shell (autostart + quick-panel hotkey)
            Self::card(ui, &pal, |ui| {
                Self::card_title(ui, &pal, "시스템");

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
                        || Self::secondary_button(ui, &pal, "적용").clicked();
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

    /// Kick off pairing on a worker thread (like `start_discovery`). The agent's
    /// Pair handler does a real network round-trip that can hang for many seconds
    /// on a slow/unreachable server, so it MUST NOT run on the UI thread. The
    /// result is delivered over `pair_rx`, drained in `poll_pair` from `update()`.
    fn do_pair(&mut self) {
        if self.pair_rx.is_some() {
            return; // a pairing attempt is already in flight
        }
        let req = Request::Pair {
            server: self.pair_server.trim().to_string(),
            otp: self.pair_otp.trim().to_string(),
            name: self.pair_name.trim().to_string(),
            pin: self.pair_pin.trim().to_string(),
            e2e_pass: self.pair_e2e.clone(),
        };
        self.pair_status = "페어링 중…".into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pair_rx = Some(rx);
        std::thread::spawn(move || {
            let result = match request(&req) {
                Ok(Response::Paired(s)) => Ok(s),
                Ok(Response::Error { message }) => Err(message),
                Ok(_) => Err("예상치 못한 응답".to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        });
    }

    /// Drain the pairing worker result (mirrors `poll_discovery`).
    fn poll_pair(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.pair_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(s)) => {
                self.pair_rx = None;
                self.pair_status = "페어링 성공".into();
                self.logline(true, "페어링 성공");
                self.adopt_status(s);
                self.reload_roster();
                ctx.request_repaint();
            }
            Ok(Err(e)) => {
                self.pair_rx = None;
                self.pair_status = format!("페어링 실패: {e}");
                self.logline(true, format!("페어링 실패: {e}"));
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pair_rx = None;
            }
        }
    }
}

// ============================================================ UI: 디버깅

impl App {
    fn tab_debug(&mut self, ui: &mut egui::Ui) {
        let pal = self.palette(ui.ctx());
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.recording, "이벤트 기록");
            // Ghost buttons: transparent fill, fg text (no filled chrome).
            if Self::ghost_button(ui, &pal, "복사").clicked() {
                let joined = self.log.iter().cloned().collect::<Vec<_>>().join("\n");
                ui.output_mut(|o| o.copied_text = joined);
            }
            if Self::ghost_button(ui, &pal, "지우기").clicked() {
                self.log.clear();
            }
        });
        ui.separator();
        // The debug log sits on the `panel2` tier (step up from the bg canvas).
        egui::Frame::none()
            .fill(pal.panel2)
            .stroke(Stroke::new(1.0, pal.line))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .size(12.5)
                                    .color(pal.fg),
                            );
                        }
                    });
            });
    }
}

// ============================================================ UI: pink toast

impl App {
    fn blocked_toasts(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let pal = self.palette(ctx);
        let screen = ctx.screen_rect();
        // Render the newest toast bottom-centered; stack older ones above it.
        for (i, toast) in self.toasts.iter().rev().enumerate() {
            let y = screen.bottom() - 70.0 - (i as f32) * 86.0;
            let pos = egui::pos2(screen.center().x, y);
            egui::Area::new(egui::Id::new(("blocked_toast", i)))
                .fixed_pos(egui::pos2(pos.x - 180.0, pos.y))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    // Critical: `Area` paints no backdrop, so we paint an OPAQUE
                    // backing surface (dark #482B31 pre-composited / light near-
                    // solid #FDE7F1) before any pink tint — see DESIGN.md §3.
                    egui::Frame::none()
                        .fill(pal.toast_fill)
                        .stroke(Stroke::new(1.0, pal.toast_stroke))
                        .rounding(Rounding::same(12.0))
                        .inner_margin(Margin::symmetric(14.0, 11.0))
                        .show(ui, |ui| {
                            ui.set_width(332.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("🔒").size(18.0));
                                ui.label(
                                    egui::RichText::new("동기화 차단됨")
                                        .strong()
                                        .size(14.0)
                                        .color(pal.toast_title),
                                );
                                // Reason chip: pink@40% fill, rounding 10, micro.
                                egui::Frame::none()
                                    .fill(tint(pal.blocked_pink, 0.40))
                                    .rounding(Rounding::same(10.0))
                                    .inner_margin(Margin::symmetric(6.0, 1.0))
                                    .show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(&toast.reason)
                                                .size(11.5)
                                                .color(pal.toast_title),
                                        );
                                    });
                            });
                            if !toast.preview.is_empty() {
                                let preview = truncate(&toast.preview, 80);
                                ui.label(
                                    egui::RichText::new(preview)
                                        .size(12.5)
                                        .color(pal.toast_body),
                                );
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

/// The kind-tag label for a history `kind` (the chip's color comes from the
/// active `Palette` via `Palette::tag_color`).
fn kind_tag_label(kind: &str) -> &'static str {
    match kind {
        "image" => "🖼 이미지",
        "file" => "📎 파일",
        _ => "📝 텍스트",
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

/// Register an embedded Korean font (Nanum Gothic, OFL) as a fallback in both
/// families so CJK text renders instead of □ tofu — egui's bundled fonts are
/// Latin-only. Appended last so Latin keeps egui's default look.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "korean".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/korean.ttf")),
    );
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("korean".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// The CopySync character icon for the title bar / taskbar (embedded PNG).
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap_or_default()
}

fn main() -> eframe::Result<()> {
    // FIRST thing: install the panic hook so even a crash during option/window
    // construction is written to gui.log (+ a MessageBox on Windows). Under
    // `windows_subsystem = "windows"` there is no console, so this file is the
    // only way a silent launch failure becomes visible/analyzable.
    install_crash_handler();
    gui_log_line("GUI 프로세스 시작");

    // If a bundled Mesa software-GL (opengl32.dll + libgallium_wgl.dll) sits next
    // to the exe — the "softgl" build for GPU-less machines (RDP / no driver) —
    // force the pure-CPU llvmpipe driver so glow gets an OpenGL 3.3 context with
    // NO GPU/DX adapter at all. Harmless when no bundled Mesa is present (the
    // system opengl32 ignores GALLIUM_DRIVER). Set before any GL/thread init.
    #[cfg(windows)]
    std::env::set_var("GALLIUM_DRIVER", "llvmpipe");

    // Best-effort: make sure the agent is up before the window opens. The UI
    // still launches (showing "연결 끊김") if this fails. Route the error to
    // gui.log — `eprintln!` is invisible under the windows subsystem.
    if let Err(e) = ensure_agent() {
        gui_log_line(&format!("에이전트에 연결할 수 없음: {e}"));
    }

    // Build + run with a chosen renderer. Each call makes its own event channel.
    // `creator_ran` records whether eframe got far enough to invoke our creator
    // closure (which spawns the long-lived event thread). The common glow failure
    // (no GL 2.0+ context) errors BEFORE the creator runs, leaving no thread/App
    // behind — only then is a wgpu retry safe. If glow created the window+context,
    // ran the creator (spawning event thread #1) and then failed afterward, a retry
    // would orphan that thread; so the caller skips the retry in that case.
    let build_and_run = |renderer: eframe::Renderer,
                         creator_ran: std::sync::Arc<std::sync::atomic::AtomicBool>|
     -> eframe::Result<()> {
        let (events_tx, events_rx) = std::sync::mpsc::channel::<Event>();
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([520.0, 680.0])
                .with_min_inner_size([420.0, 480.0])
                .with_title("CopySync")
                .with_icon(app_icon()),
            // Prefer (not Require) HW accel so a weak driver degrades instead of failing.
            hardware_acceleration: eframe::HardwareAcceleration::Preferred,
            renderer,
            ..Default::default()
        };
        eframe::run_native(
            "CopySync",
            options,
            Box::new(move |cc| {
                // The creator ran: a window + context exist and the event thread is
                // about to be spawned. Record this so a post-creator failure does not
                // trigger a renderer retry that would orphan this thread.
                creator_ran.store(true, std::sync::atomic::Ordering::SeqCst);
                // Start the event thread now that we hold an egui Context to repaint.
                let ctx = cc.egui_ctx.clone();
                std::thread::spawn(move || event_loop(events_tx, ctx));
                Ok(Box::new(App::new(cc, events_rx)))
            }),
        )
    };

    // Try glow (OpenGL) first — light, best when a real GL driver exists. Fall back
    // to wgpu (DX12/Vulkan, + WARP software on Windows) when glow can't get an
    // OpenGL 2.0+ context — the exact failure seen on RDP / driverless VMs.
    let glow_creator_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut result = build_and_run(eframe::Renderer::Glow, glow_creator_ran.clone());
    if let Err(e) = &result {
        // Only fall back when glow failed at init (before its creator spawned the
        // event thread). A post-creator failure already left a live event thread; a
        // wgpu retry would spawn a second one, so don't retry in that case.
        if glow_creator_ran.load(std::sync::atomic::Ordering::SeqCst) {
            gui_log_line(&format!(
                "glow 렌더러가 초기화 후 실패 ({e}); 이벤트 스레드 중복 생성을 피하기 위해 wgpu 폴백 생략"
            ));
        } else {
            gui_log_line(&format!("glow 렌더러 실패 ({e}); wgpu 백엔드로 폴백 재시도"));
            let wgpu_creator_ran =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            result = build_and_run(eframe::Renderer::Wgpu, wgpu_creator_ran);
            if result.is_ok() {
                gui_log_line("wgpu 백엔드로 GUI 시작 성공");
            }
        }
    }

    // Still failed (or any other init error): make it visible (log + MessageBox).
    if let Err(e) = &result {
        let msg = format!("eframe::run_native 실패: {e}");
        gui_log_line(&msg);
        message_box(
            "CopySync GUI failed",
            &format!("{msg}\n\n자세한 내용: {}", gui_log_path().display()),
        );
    }
    result
}
