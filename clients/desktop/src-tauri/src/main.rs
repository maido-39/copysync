// CopySync desktop client (Tauri + webkit/WebView2). The UI is a thin
// no-framework SPA; all protocol/crypto/networking lives in copysync-core, the
// same crate that is headlessly interop-tested against the Go server and copyctl.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use copysync_core::clipboard::{self, Image};
use copysync_core::history::{Entry, History};
use copysync_core::protocol::{
    self, BlobRequest, ClipEvent, DeviceInfo, EncMeta, Presence, Roster, Targets,
};
use copysync_core::{blob, e2e, pairing, pinning, privacy, ws, Config};

/// Commands flowing into the sync actor.
enum Cmd {
    LocalText { text: String, html: Option<String> }, // OS-clipboard text/rich-text change
    LocalImage(Image),   // OS-clipboard image change (echo-guarded)
    SendText(String),    // explicit text send from the UI
    SendFile(String),    // explicit file send from the UI (path)
    LocalFiles(Vec<String>), // OS-clipboard file copy (CF_HDROP); echo-guarded
    SetPool(String),     // switch this device's share pool
}

/// A blob held for on-demand upload when the server asks (`blob_request`).
enum Hold {
    Sealed(Vec<u8>), // E2E: exact ciphertext (sha must match what was advertised)
    Plain(PathBuf),  // plaintext: re-read on demand
}

#[derive(Clone, Serialize, Default)]
struct Status {
    paired: bool,
    connected: bool,
    server_name: String,
    device_name: String,
    server_id: String,
    e2e: bool,
    pool: String,
    pools: Vec<String>,
    /// True while disconnected-but-paired and actively retrying.
    reconnecting: bool,
}

#[derive(Clone, Serialize, Default)]
struct RosterDevice {
    id: String,
    name: String,
    online: bool,
}

/// 32-byte key for at-rest history encryption. Uses the native OS keyring on
/// macOS/Windows; elsewhere (e.g. Linux without a Secret Service) a 0600 key file
/// in the app data dir. Generated once, then reused.
fn history_key(data_dir: &std::path::Path) -> [u8; 32] {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        use base64::{engine::general_purpose::STANDARD, Engine};
        if let Ok(entry) = keyring::Entry::new("copysync", "history-key") {
            if let Ok(b64) = entry.get_password() {
                if let Ok(raw) = STANDARD.decode(&b64) {
                    if let Ok(k) = <[u8; 32]>::try_from(raw.as_slice()) {
                        return k;
                    }
                }
            }
            let k = copysync_core::e2e::random_key();
            let _ = entry.set_password(&STANDARD.encode(k));
            return k;
        }
    }
    let kf = data_dir.join("history.key");
    if let Ok(b) = std::fs::read(&kf) {
        if let Ok(k) = <[u8; 32]>::try_from(b.as_slice()) {
            return k;
        }
    }
    let k = copysync_core::e2e::random_key();
    let _ = std::fs::write(&kf, k);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&kf, std::fs::Permissions::from_mode(0o600));
    }
    k
}

/// Quick Panel: copy a history item's text back to the OS clipboard, then hide
/// the panel. The clipboard watcher re-syncs it like any normal copy.
#[tauri::command]
fn quickpanel_copy(app: AppHandle, text: String) -> Result<(), String> {
    clipboard::set_text(&text).map_err(|e| e.to_string())?;
    if let Some(win) = app.get_webview_window("quickpanel") {
        let _ = win.hide();
    }
    Ok(())
}

/// Show/hide the Quick Panel overlay (the body of the global hotkey handler).
fn toggle_quick_panel(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("quickpanel") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
            let _ = win.emit("quickpanel-show", ());
        }
    }
}

/// Register `accel` as the Quick Panel toggle hotkey. An empty/blank string means
/// "no hotkey" (a no-op). Returns Err if the accelerator is invalid or the OS
/// refuses the combo (e.g. another app already owns it).
fn register_quick_panel(app: &AppHandle, accel: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    if accel.trim().is_empty() {
        return Ok(());
    }
    app.global_shortcut()
        .on_shortcut(accel, |app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                toggle_quick_panel(app);
            }
        })
        .map_err(|e| e.to_string())
}

/// The currently-registered Quick Panel hotkey (accelerator string; empty = none).
#[tauri::command]
fn get_shortcut(state: State<'_, AppState>) -> String {
    state.quick_panel_shortcut.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Change the Quick Panel hotkey live: validate, swap the OS registration, and
/// persist to config (best-effort, like the other settings). An empty `accel`
/// disables the hotkey. On failure the previous hotkey is restored.
#[tauri::command]
fn set_shortcut(app: AppHandle, state: State<'_, AppState>, accel: String) -> Result<(), String> {
    use std::str::FromStr;
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

    let accel = accel.trim().to_string();
    let old = state.quick_panel_shortcut.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if accel == old {
        return Ok(());
    }
    // Validate first so a bad value never costs us the working hotkey.
    if !accel.is_empty() {
        Shortcut::from_str(&accel).map_err(|e| format!("잘못된 단축키 '{accel}': {e}"))?;
    }
    let gs = app.global_shortcut();
    if !old.is_empty() {
        let _ = gs.unregister(old.as_str());
    }
    if let Err(e) = register_quick_panel(&app, &accel) {
        // Roll back to the previous hotkey (e.g. the new combo is taken by another app).
        let _ = register_quick_panel(&app, &old);
        return Err(e);
    }
    *state.quick_panel_shortcut.lock().unwrap_or_else(|e| e.into_inner()) = accel.clone();
    if let Ok(mut cfg) = Config::load(&state.cfg_path) {
        cfg.quick_panel_shortcut = accel;
        let _ = cfg.save(&state.cfg_path);
    }
    Ok(())
}

struct AppState {
    tx: Mutex<Option<UnboundedSender<Cmd>>>,
    hist: Arc<Mutex<History>>,
    status: Arc<Mutex<Status>>,
    roster: Arc<Mutex<Vec<RosterDevice>>>,
    targets: Arc<Mutex<Targets>>,
    tray: Mutex<Option<tauri::tray::TrayIcon>>,
    cfg_path: PathBuf,
    downloads: PathBuf,
    exclude_sensitive: Arc<AtomicBool>,
    /// The Quick Panel global hotkey currently registered (accelerator string).
    quick_panel_shortcut: Arc<Mutex<String>>,
    /// Wipe the clipboard this many seconds after a received clip (0 = never).
    auto_clear_secs: Arc<AtomicU64>,
    /// Mark received clips so the OS clipboard history / cloud sync skips them.
    mark_sensitive: Arc<AtomicBool>,
    /// Notified to force an immediate reconnect (manual "재연결" + wakes backoff).
    reconnect: Arc<Notify>,
}

fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn remember(q: &mut VecDeque<String>, sha: String) {
    if q.iter().any(|x| x == &sha) {
        return;
    }
    if q.len() >= 64 {
        q.pop_front();
    }
    q.push_back(sha);
}

fn seen(q: &VecDeque<String>, sha: &str) -> bool {
    q.iter().any(|x| x == sha)
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

fn preview(s: &str) -> String {
    let t: String = s.chars().take(80).collect();
    if s.chars().count() > 80 {
        format!("{t}…")
    } else {
        t
    }
}

/// A reliable incoming-clip popup: a small always-on-top window shown bottom-right.
/// Used instead of OS toasts, which don't appear for portable (uninstalled) Windows
/// builds. Resolves the sender's friendly name from the roster.
fn show_toast(app: &AppHandle, origin: &str, body: &str, image: Option<String>) {
    let name = {
        let st = app.state::<AppState>();
        let r = st.roster.lock().unwrap_or_else(|e| e.into_inner());
        r.iter()
            .find(|d| d.id == origin)
            .map(|d| d.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "다른 기기".to_string())
    };
    if let Some(win) = app.get_webview_window("toast") {
        if let (Ok(Some(mon)), Ok(ws)) = (win.primary_monitor(), win.outer_size()) {
            let sz = mon.size();
            let m = 16i32;
            let x = sz.width as i32 - ws.width as i32 - m;
            let y = sz.height as i32 - ws.height as i32 - 56 - m;
            let _ = win.set_position(tauri::PhysicalPosition::new(x.max(0), y.max(0)));
        }
        let _ = win.emit("toast-show", serde_json::json!({ "title": name, "body": body, "image": image }));
        let _ = win.show();
    }
}

/// A small base64 PNG data-URI thumbnail from raw image bytes (for the toast).
fn thumb_data_uri(data: &[u8]) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let img = image::load_from_memory(data).ok()?;
    let thumb = img.thumbnail(128, 128);
    let mut buf = std::io::Cursor::new(Vec::new());
    thumb.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(format!("data:image/png;base64,{}", STANDARD.encode(buf.into_inner())))
}

#[tauri::command]
fn hide_toast(app: AppHandle) {
    if let Some(win) = app.get_webview_window("toast") {
        let _ = win.hide();
    }
}

/// Delete a (sensitive) history row after a TTL so secrets don't linger.
fn schedule_purge(hist: Arc<Mutex<History>>, id: i64, ttl_secs: u64) {
    if id < 0 || ttl_secs == 0 {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(ttl_secs)).await;
        if let Ok(h) = hist.lock() {
            let _ = h.delete(id);
        }
    });
}

/// Wipe the clipboard `secs` after a received clip — but only if it still holds
/// that exact text, so a newer copy the user made is never clobbered.
fn schedule_clear_text(expected: String, secs: u64) {
    if secs == 0 {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        if clipboard::get_text().map(|t| t == expected).unwrap_or(false) {
            let _ = clipboard::clear();
        }
    });
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string()
}

fn mime_of(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ----------------------------------------------------------------- commands

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> Status {
    state.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn build_tray_menu(app: &AppHandle, current: &str, pools: &[String]) -> Option<Menu<tauri::Wry>> {
    let menu = Menu::new(app).ok()?;
    menu.append(&MenuItem::with_id(app, "show", "CopySync 열기", true, None::<&str>).ok()?).ok()?;
    if !pools.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app).ok()?).ok()?;
        menu.append(&MenuItem::with_id(app, "pool_hdr", "풀 전환", false, None::<&str>).ok()?).ok()?;
        for p in pools {
            menu.append(
                &CheckMenuItem::with_id(app, format!("pool:{p}"), p.as_str(), true, p.as_str() == current, None::<&str>).ok()?,
            )
            .ok()?;
        }
    }
    menu.append(&PredefinedMenuItem::separator(app).ok()?).ok()?;
    menu.append(&MenuItem::with_id(app, "quit", "종료", true, None::<&str>).ok()?).ok()?;
    Some(menu)
}

/// Rebuild the tray menu so the pool submenu reflects current pools + selection.
fn rebuild_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let (pool, pools) = {
        let s = state.status.lock().unwrap_or_else(|e| e.into_inner());
        (s.pool.clone(), s.pools.clone())
    };
    if let Some(menu) = build_tray_menu(app, &pool, &pools) {
        if let Some(tray) = state.tray.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let m = app.autolaunch();
    (if enabled { m.enable() } else { m.disable() }).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_privacy_filter(state: State<'_, AppState>) -> bool {
    state.exclude_sensitive.load(Ordering::Relaxed)
}

#[tauri::command]
fn set_privacy_filter(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state.exclude_sensitive.store(enabled, Ordering::Relaxed);
    if let Ok(mut cfg) = Config::load(&state.cfg_path) {
        cfg.exclude_sensitive = enabled;
        let _ = cfg.save(&state.cfg_path);
    }
    Ok(())
}

/// Seconds to wait before auto-clearing a received clip from the OS clipboard.
#[tauri::command]
fn get_auto_clear(state: State<'_, AppState>) -> u64 {
    state.auto_clear_secs.load(Ordering::Relaxed)
}

#[tauri::command]
fn set_auto_clear(state: State<'_, AppState>, secs: u64) -> Result<(), String> {
    state.auto_clear_secs.store(secs, Ordering::Relaxed);
    if let Ok(mut cfg) = Config::load(&state.cfg_path) {
        cfg.auto_clear_secs = secs;
        let _ = cfg.save(&state.cfg_path);
    }
    Ok(())
}

/// Whether received clips are marked sensitive (excluded from OS clipboard history).
#[tauri::command]
fn get_mark_sensitive(state: State<'_, AppState>) -> bool {
    state.mark_sensitive.load(Ordering::Relaxed)
}

#[tauri::command]
fn set_mark_sensitive(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state.mark_sensitive.store(enabled, Ordering::Relaxed);
    if let Ok(mut cfg) = Config::load(&state.cfg_path) {
        cfg.mark_received_sensitive = enabled;
        let _ = cfg.save(&state.cfg_path);
    }
    Ok(())
}

/// Force an immediate reconnect (manual "재연결"). Wakes the sync actor's backoff
/// sleep or breaks an idle connection so it re-dials right away.
#[tauri::command]
fn reconnect(state: State<'_, AppState>) {
    state.reconnect.notify_waiters();
}

/// Browse the LAN for CopySync servers over mDNS. Returns `[{name, url}]`.
#[tauri::command]
async fn discover_servers() -> Result<Vec<copysync_core::discovery::Found>, String> {
    tauri::async_runtime::spawn_blocking(|| copysync_core::discovery::discover(2500))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_roster(state: State<'_, AppState>) -> Vec<RosterDevice> {
    state.roster.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tauri::command]
fn set_targets(state: State<'_, AppState>, ids: Vec<String>) {
    let t = if ids.is_empty() {
        Targets::All
    } else {
        Targets::Devices(ids)
    };
    *state.targets.lock().unwrap_or_else(|e| e.into_inner()) = t;
}

#[tauri::command]
fn get_history(state: State<'_, AppState>, query: Option<String>) -> Result<Vec<Entry>, String> {
    let h = state.hist.lock().unwrap_or_else(|e| e.into_inner());
    match query {
        Some(q) if !q.trim().is_empty() => h.search(q.trim(), 200),
        _ => h.recent(200),
    }
    .map_err(|e| e.to_string())
}

/// A small PNG data-URI thumbnail of an image file, for history previews.
#[tauri::command]
fn thumbnail(path: String) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let data = std::fs::read(&path).ok()?;
    if data.is_empty() || data.len() > 60_000_000 {
        return None;
    }
    let img = image::load_from_memory(&data).ok()?;
    let thumb = img.thumbnail(320, 320); // keeps aspect ratio, max 320px
    let mut buf = std::io::Cursor::new(Vec::new());
    thumb.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(format!("data:image/png;base64,{}", STANDARD.encode(buf.into_inner())))
}

/// First ~600 chars of a text file, for history previews.
#[tauri::command]
fn text_preview(path: String) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(&path).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).chars().take(600).collect())
}

#[tauri::command]
fn send_text(state: State<'_, AppState>, text: String) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    enqueue(&state, Cmd::SendText(text))
}

#[tauri::command]
fn send_file(state: State<'_, AppState>, path: String) -> Result<(), String> {
    enqueue(&state, Cmd::SendFile(path))
}

#[tauri::command]
fn set_pool(state: State<'_, AppState>, pool: String) -> Result<(), String> {
    enqueue(&state, Cmd::SetPool(pool))
}

fn enqueue(state: &State<'_, AppState>, cmd: Cmd) -> Result<(), String> {
    match state.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(tx) => tx
            .send(cmd)
            .map_err(|_| "동기화가 중단됨 — 설정 → 기기 페어링에서 다시 연결하세요".into()),
        None => Err("페어링이 필요합니다 — 설정 → 기기 페어링".into()),
    }
}

#[tauri::command]
async fn pair(
    app: AppHandle,
    server: String,
    otp: String,
    name: String,
    pin: String,
    e2e_pass: String,
) -> Result<Status, String> {
    let cfg = pairing::claim(&server, &pin, &otp, &name, &e2e_pass)
        .await
        .map_err(|e| e.to_string())?;
    let path = app.state::<AppState>().cfg_path.clone();
    cfg.save(&path).map_err(|e| e.to_string())?;
    start_sync(&app, cfg);
    Ok(app.state::<AppState>().status.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

// ----------------------------------------------------------------- sync wiring

fn start_sync(app: &AppHandle, cfg: Config) {
    let state = app.state::<AppState>();
    state.exclude_sensitive.store(cfg.exclude_sensitive, Ordering::Relaxed);
    state.auto_clear_secs.store(cfg.auto_clear_secs, Ordering::Relaxed);
    state.mark_sensitive.store(cfg.mark_received_sensitive, Ordering::Relaxed);
    {
        let mut s = state.status.lock().unwrap_or_else(|e| e.into_inner());
        s.paired = true;
        s.server_name = cfg.server_name.clone();
        s.device_name = cfg.device_name.clone();
        s.server_id = cfg.server_id.clone();
        s.e2e = !cfg.e2e_pass.is_empty();
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    *state.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());

    let app_cb = app.clone();
    std::thread::spawn(move || clipboard_loop(tx, app_cb));

    let app2 = app.clone();
    let hist = state.hist.clone();
    let status = state.status.clone();
    let roster = state.roster.clone();
    let targets = state.targets.clone();
    let downloads = state.downloads.clone();
    let cfg_path = state.cfg_path.clone();
    let app_sup = app.clone();
    tauri::async_runtime::spawn(async move {
        sync_actor(app2, cfg, cfg_path, rx, hist, status, roster, targets, downloads).await;
        // The sync engine stopped (bad config / fatal error). Surface it instead of
        // letting later actions fail silently with "sync not running".
        let _ = app_sup.emit("error", "동기화가 멈췄습니다 — 설정 → 기기 페어링에서 다시 연결하거나 앱을 재시작하세요.");
    });
}

fn clipboard_loop(tx: UnboundedSender<Cmd>, app: AppHandle) {
    let mut last_seq: Option<u32> = None;
    let mut last_text = String::new();
    let mut last_img = String::new();
    let mut last_files = String::new();
    loop {
        // On Windows, only touch the clipboard when its sequence number changes —
        // re-opening it every tick contends with RDP's redirector and drops copies.
        // Elsewhere seq_num() is None, so we keep polling content every tick.
        let seq = clipboard::seq_num();
        let changed = seq.map_or(true, |s| Some(s) != last_seq);
        if changed {
            if let Some(s) = seq {
                last_seq = Some(s);
            }
            let text = clipboard::get_text();
            let mut handled = false;
            if let Ok(t) = &text {
                if !t.is_empty() {
                    if *t != last_text {
                        last_text = t.clone();
                        last_img.clear();
                        last_files.clear();
                        let html = clipboard::get_html().ok().filter(|h| !h.is_empty());
                        let _ = tx.send(Cmd::LocalText { text: t.clone(), html });
                    }
                    handled = true;
                }
            }
            if !handled {
                if let Ok(img) = clipboard::get_image() {
                    let h = sha_hex(&img.rgba);
                    if h != last_img {
                        last_img = h;
                        last_text.clear();
                        last_files.clear();
                        let _ = tx.send(Cmd::LocalImage(img));
                    }
                    handled = true;
                } else if let Some(files) = clipboard::get_files() {
                    // Windows Explorer file copy (CF_HDROP).
                    let key = files.join("\u{1}");
                    if key != last_files {
                        last_files = key;
                        last_text.clear();
                        last_img.clear();
                        let _ = tx.send(Cmd::LocalFiles(files));
                    }
                    handled = true;
                }
            }
            // A clipboard change we couldn't turn into a normal text/image/file
            // event — surface what was actually there (this is where RDP file
            // copies and odd formats land). Shows up in the 디버깅 event log.
            if !handled && seq.is_some() {
                let formats = clipboard::list_formats();
                if let Some(names) = clipboard::get_virtual_file_names() {
                    let _ = app.emit(
                        "cliplog",
                        format!(
                            "RDP/가상 파일 복사 감지: {} — CF_HDROP가 아닌 스트리밍 파일(FileContents)이라 아직 동기화하지 못합니다. 포맷=[{}]",
                            names.join(", "),
                            formats.join(", ")
                        ),
                    );
                } else if text.is_err() {
                    let _ = app.emit(
                        "cliplog",
                        format!("클립보드 읽기 실패 (RDP가 점유 중일 수 있음) · 포맷=[{}]", formats.join(", ")),
                    );
                } else if !formats.is_empty() {
                    let _ = app.emit(
                        "cliplog",
                        format!("처리하지 못한 클립보드 변경 · 포맷=[{}]", formats.join(", ")),
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_millis(800));
        if tx.is_closed() {
            return;
        }
    }
}

fn current_targets(t: &Arc<Mutex<Targets>>) -> Targets {
    t.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[allow(clippy::too_many_arguments)]
async fn sync_actor(
    app: AppHandle,
    mut cfg: Config,
    cfg_path: PathBuf,
    mut rx: UnboundedReceiver<Cmd>,
    hist: Arc<Mutex<History>>,
    status: Arc<Mutex<Status>>,
    roster: Arc<Mutex<Vec<RosterDevice>>>,
    targets: Arc<Mutex<Targets>>,
    downloads: PathBuf,
) {
    // Liveness: ping this often; reconnect if no frame arrives within the watchdog
    // (the server pings every 30s, so 75s = ~2 missed pings = a dead link).
    const WS_PING_EVERY: Duration = Duration::from_secs(30);
    const WS_WATCHDOG: Duration = Duration::from_secs(75);

    let pin = match cfg.pin_bytes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bad pin: {e}");
            set_connected(&app, &status, false);
            let _ = app.emit("error", "잘못된 SPKI 핀입니다 — 설정 → 기기 페어링에서 다시 연결하세요.");
            return;
        }
    };
    let key = cfg.e2e_key();
    let custom_res = cfg.custom_regexes();
    let exclude_flag = app.state::<AppState>().exclude_sensitive.clone();
    let http = pinning::http_client(pin);
    let pull = blob::pull_client(pin);
    let mut seq: u64 = 0;
    let mut recent_text: VecDeque<String> = VecDeque::new();
    let mut recent_img: VecDeque<String> = VecDeque::new();
    let mut recent_files: VecDeque<String> = VecDeque::new();
    let mut on_demand: HashMap<String, Hold> = HashMap::new();
    let reconnect = app.state::<AppState>().reconnect.clone();
    let mut attempt: u32 = 0;

    loop {
        match ws::connect(&cfg.server_url, pin, &cfg.device_id, &cfg.device_name, &cfg.token).await {
            Ok((mut sock, hello)) => {
                attempt = 0;
                let threshold = hello.on_demand_threshold;
                set_roster(&app, &roster, hello.roster.clone());
                {
                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                    s.pool = hello.pool.clone();
                    s.pools = hello.pools.clone();
                    s.reconnecting = false;
                }
                set_connected(&app, &status, true);
                rebuild_tray(&app);
                // Liveness watchdog: ping every WS_PING_EVERY and reconnect if no
                // frame arrives within WS_WATCHDOG (the server pings every 30s).
                // Catches a silently dead TCP link — RDP/network drop, suspend.
                let mut last_recv = Instant::now();
                let mut ping_at = tokio::time::interval(WS_PING_EVERY);
                ping_at.tick().await;
                let mut watchdog = tokio::time::interval(Duration::from_secs(15));
                watchdog.tick().await;
                loop {
                    tokio::select! {
                        _ = ping_at.tick() => {
                            if ws::ping(&mut sock).await.is_err() { break; }
                        }
                        _ = watchdog.tick() => {
                            if last_recv.elapsed() > WS_WATCHDOG { break; }
                        }
                        _ = reconnect.notified() => break,
                        cmd = rx.recv() => match cmd {
                            None => return,
                            Some(Cmd::LocalText { text, html }) => {
                                let sha = sha_hex(text.as_bytes());
                                if seen(&recent_text, &sha) { continue; }
                                remember(&mut recent_text, sha);
                                if exclude_flag.load(Ordering::Relaxed) {
                                    if let Some(reason) = privacy::classify(&text, &custom_res) {
                                        // Privacy filter: never sync; record locally + auto-purge.
                                        let id = hist.lock().unwrap_or_else(|e| e.into_inner())
                                            .add(&protocol::now_ts(), "text", "me", "out", &text, "text/plain", text.len() as i64, "", "")
                                            .unwrap_or(-1);
                                        schedule_purge(hist.clone(), id, cfg.sensitive_ttl_secs);
                                        let _ = app.emit("clip", serde_json::json!({"direction":"out","kind":"text","text":text,"sensitive":reason.label()}));
                                        continue;
                                    }
                                }
                                if !send_text_clip(&mut sock, &mut seq, &text, html.as_deref(), &key, current_targets(&targets), &app, &hist).await { break; }
                            }
                            Some(Cmd::SendText(t)) => {
                                remember(&mut recent_text, sha_hex(t.as_bytes()));
                                if !send_text_clip(&mut sock, &mut seq, &t, None, &key, current_targets(&targets), &app, &hist).await { break; }
                            }
                            Some(Cmd::LocalImage(img)) => {
                                let sha = sha_hex(&img.rgba);
                                if seen(&recent_img, &sha) { continue; }
                                remember(&mut recent_img, sha);
                                if !send_image_clip(&mut sock, &mut seq, &img, &key, current_targets(&targets), &app, &hist, &http, &cfg).await { break; }
                            }
                            Some(Cmd::SendFile(p)) => {
                                if !send_file_clip(&mut sock, &mut seq, &p, &key, current_targets(&targets), threshold, &mut on_demand, &app, &hist, &http, &cfg).await { break; }
                            }
                            Some(Cmd::LocalFiles(files)) => {
                                for p in files {
                                    // Skip a file we just placed on the clipboard from an inbound clip (echo).
                                    if seen(&recent_files, &p) { continue; }
                                    if !send_file_clip(&mut sock, &mut seq, &p, &key, current_targets(&targets), threshold, &mut on_demand, &app, &hist, &http, &cfg).await { break; }
                                }
                            }
                            Some(Cmd::SetPool(name)) => {
                                if ws::send(&mut sock, protocol::T_SET_POOL, &protocol::SetPool { pool: name.clone() }).await.is_err() { break; }
                                let snap = { let mut s = status.lock().unwrap_or_else(|e| e.into_inner()); s.pool = name; s.clone() };
                                let _ = app.emit("status", snap);
                                rebuild_tray(&app);
                            }
                        },
                        frame = ws::recv(&mut sock) => match frame {
                            Ok(Some((t, d))) => {
                                last_recv = Instant::now();
                                if t == ws::KEEPALIVE {
                                    // ping/pong — liveness only.
                                } else if t == protocol::T_TOKEN_ROTATE {
                                    // Stage-3: persist the re-issued bearer token; the next
                                    // reconnect uses it and the server retires the old one.
                                    if let Ok(tr) = serde_json::from_value::<protocol::TokenRotate>(d) {
                                        if !tr.token.is_empty() {
                                            cfg.token = tr.token;
                                            let _ = cfg.save(&cfg_path);
                                        }
                                    }
                                } else {
                                    handle_frame(t, d, &key, &pull, &http, &cfg, &app, &hist, &downloads, &roster, &mut recent_text, &mut recent_img, &mut recent_files, &on_demand).await;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }
                set_connected(&app, &status, false);
            }
            Err(e) => {
                eprintln!("connect failed: {e}");
                let _ = app.emit("error", format!("연결 실패 — 재시도 중: {e}"));
            }
        }
        // Exponential backoff with jitter (1→2→4…→30s), surfaced to the UI; a
        // manual "재연결" (reconnect.notified) skips the wait.
        attempt = attempt.saturating_add(1);
        let delay = backoff_delay(attempt);
        let snap = {
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            s.connected = false;
            s.reconnecting = true;
            s.clone()
        };
        let _ = app.emit("status", snap);
        let _ = app.emit("reconnect", format!("{attempt}회 · {}초 후 재시도", delay.as_secs().max(1)));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = reconnect.notified() => {}
        }
    }
}

/// Reconnect backoff: 1, 2, 4, 8, 16, 30, 30… seconds, plus a little jitter so
/// many clients don't retry in lockstep.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = (1u64 << attempt.min(5)).min(30);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.subsec_millis() as u64) % 700)
        .unwrap_or(0);
    Duration::from_millis(secs * 1000 + jitter)
}

fn set_connected(app: &AppHandle, status: &Arc<Mutex<Status>>, on: bool) {
    let snapshot = {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.connected = on;
        s.clone()
    };
    let _ = app.emit("status", snapshot);
}

fn set_roster(app: &AppHandle, roster: &Arc<Mutex<Vec<RosterDevice>>>, devices: Vec<DeviceInfo>) {
    let list: Vec<RosterDevice> = devices
        .iter()
        .map(|d| RosterDevice {
            id: d.device.id.clone(),
            name: d.device.name.clone(),
            online: d.online,
        })
        .collect();
    *roster.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
    let _ = app.emit("roster", list);
}

fn apply_presence(app: &AppHandle, roster: &Arc<Mutex<Vec<RosterDevice>>>, p: Presence) {
    let snapshot = {
        let mut r = roster.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = r.iter_mut().find(|x| x.id == p.device.id) {
            d.online = p.online;
            d.name = p.device.name.clone();
        } else {
            r.push(RosterDevice {
                id: p.device.id.clone(),
                name: p.device.name.clone(),
                online: p.online,
            });
        }
        r.clone()
    };
    let _ = app.emit("roster", snapshot);
}

// ----------------------------------------------------------------- outbound

async fn send_text_clip(
    sock: &mut ws::Ws,
    seq: &mut u64,
    text: &str,
    html: Option<&str>,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    app: &AppHandle,
    hist: &Arc<Mutex<History>>,
) -> bool {
    *seq += 1;
    let ev = match ClipEvent::new_text(
        *seq,
        text,
        html,
        key.as_ref().map(|(k, i)| (k.as_slice(), i.as_str())),
        targets,
    ) {
        Ok(e) => e,
        Err(_) => return true,
    };
    if ws::send(sock, protocol::T_CLIP, &ev).await.is_err() {
        return false;
    }
    add_history(hist, &ev.ts, "text", "me", "out", text, "text/plain", text.len() as i64, "", "");
    let _ = app.emit("clip", serde_json::json!({"direction":"out","kind":"text","text":text}));
    true
}

/// Cache an outbound clipboard image locally so the history can render its
/// thumbnail (inbound images already persist a file; outbound only kept a label).
fn cache_outbound_image(app: &AppHandle, png: &[u8]) -> Option<String> {
    let dir = app.path().app_data_dir().ok()?.join("clip-out");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.png", sha_hex(png)));
    if !path.exists() {
        std::fs::write(&path, png).ok()?;
    }
    Some(path.to_string_lossy().into_owned())
}

#[allow(clippy::too_many_arguments)]
async fn send_image_clip(
    sock: &mut ws::Ws,
    seq: &mut u64,
    img: &Image,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    app: &AppHandle,
    hist: &Arc<Mutex<History>>,
    http: &reqwest::Client,
    cfg: &Config,
) -> bool {
    let png = match clipboard::encode_png(img) {
        Ok(p) => p,
        Err(_) => return true,
    };
    let payload = match key {
        Some((k, _)) => match e2e::seal(k, &png) {
            Ok(c) => c,
            Err(_) => return true,
        },
        None => png.clone(),
    };
    let bid = match blob::put_blob(http, &cfg.server_url, &cfg.token, payload.clone()).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("image blob upload failed: {e}");
            return true; // blob channel issue, not the control channel
        }
    };
    *seq += 1;
    let ev = ClipEvent {
        id: protocol::new_id(),
        seq: *seq,
        ts: protocol::now_ts(),
        mime: vec!["image/png".into()],
        name: "clipboard.png".into(),
        blob_id: bid,
        size: png.len() as i64,
        sha256: sha_hex(&payload),
        targets,
        enc: key.as_ref().map(|(_, kid)| EncMeta {
            alg: e2e::ALG.into(),
            key_id: kid.clone(),
            nonce: String::new(),
        }),
        ..Default::default()
    };
    if ws::send(sock, protocol::T_CLIP, &ev).await.is_err() {
        return false;
    }
    let prev = cache_outbound_image(app, &png).unwrap_or_else(|| "(클립보드 이미지)".into());
    add_history(hist, &ev.ts, "image", "me", "out", &prev, "image/png", png.len() as i64, &ev.blob_id, "clipboard.png");
    let _ = app.emit("clip", serde_json::json!({"direction":"out","kind":"image","name":"clipboard.png","size":png.len()}));
    true
}

#[allow(clippy::too_many_arguments)]
async fn send_file_clip(
    sock: &mut ws::Ws,
    seq: &mut u64,
    path: &str,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    threshold: i64,
    on_demand: &mut HashMap<String, Hold>,
    app: &AppHandle,
    hist: &Arc<Mutex<History>>,
    http: &reqwest::Client,
    cfg: &Config,
) -> bool {
    let p = Path::new(path);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) => {
            notify(app, "CopySync", &format!("파일을 열 수 없습니다: {e}"));
            return true;
        }
    };
    let size = meta.len() as i64;
    let name = file_name(path);
    let mime = mime_of(path);
    let kind = if mime.starts_with("image/") { "image" } else { "file" };

    *seq += 1;
    let ev = if threshold > 0 && size > threshold {
        // On-demand: advertise now, upload when the server asks.
        let (bid, sha) = match key {
            Some((k, _)) => {
                let data = match std::fs::read(p) {
                    Ok(d) => d,
                    Err(e) => { notify(app, "CopySync", &format!("읽기 실패: {e}")); return true; }
                };
                let ct = match e2e::seal(k, &data) { Ok(c) => c, Err(_) => return true };
                let sha = sha_hex(&ct);
                let bid = format!("sha256:{sha}");
                on_demand.insert(bid.clone(), Hold::Sealed(ct));
                (bid, sha)
            }
            None => {
                let sha = match file_sha_hex(p) { Ok(s) => s, Err(e) => { notify(app,"CopySync",&format!("읽기 실패: {e}")); return true; } };
                let bid = format!("sha256:{sha}");
                on_demand.insert(bid.clone(), Hold::Plain(p.to_path_buf()));
                (bid, sha)
            }
        };
        ClipEvent {
            id: protocol::new_id(), seq: *seq, ts: protocol::now_ts(),
            mime: vec![mime.clone()], name: name.clone(), blob_id: bid, size,
            sha256: sha, on_demand: true, targets,
            enc: key.as_ref().map(|(_, kid)| EncMeta { alg: e2e::ALG.into(), key_id: kid.clone(), nonce: String::new() }),
            ..Default::default()
        }
    } else {
        // Eager: upload the (encrypted) bytes immediately.
        let data = match std::fs::read(p) {
            Ok(d) => d,
            Err(e) => { notify(app, "CopySync", &format!("읽기 실패: {e}")); return true; }
        };
        let payload = match key {
            Some((k, _)) => match e2e::seal(k, &data) { Ok(c) => c, Err(_) => return true },
            None => data,
        };
        let bid = match blob::put_blob(http, &cfg.server_url, &cfg.token, payload.clone()).await {
            Ok(b) => b,
            Err(e) => { notify(app, "CopySync", &format!("업로드 실패: {e}")); return true; }
        };
        ClipEvent {
            id: protocol::new_id(), seq: *seq, ts: protocol::now_ts(),
            mime: vec![mime.clone()], name: name.clone(), blob_id: bid, size,
            sha256: sha_hex(&payload), targets,
            enc: key.as_ref().map(|(_, kid)| EncMeta { alg: e2e::ALG.into(), key_id: kid.clone(), nonce: String::new() }),
            ..Default::default()
        }
    };
    if ws::send(sock, protocol::T_CLIP, &ev).await.is_err() {
        return false;
    }
    add_history(hist, &ev.ts, kind, "me", "out", &name, &mime, size, &ev.blob_id, &name);
    let _ = app.emit("clip", serde_json::json!({"direction":"out","kind":kind,"name":name,"size":size,"onDemand":ev.on_demand}));
    true
}

fn file_sha_hex(p: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

// ----------------------------------------------------------------- inbound

#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    t: String,
    d: serde_json::Value,
    key: &Option<(Vec<u8>, String)>,
    pull: &reqwest::Client,
    http: &reqwest::Client,
    cfg: &Config,
    app: &AppHandle,
    hist: &Arc<Mutex<History>>,
    downloads: &Path,
    roster: &Arc<Mutex<Vec<RosterDevice>>>,
    recent_text: &mut VecDeque<String>,
    recent_img: &mut VecDeque<String>,
    recent_files: &mut VecDeque<String>,
    on_demand: &HashMap<String, Hold>,
) {
    match t.as_str() {
        protocol::T_CLIP => {
            if let Ok(ev) = serde_json::from_value::<ClipEvent>(d) {
                handle_incoming(ev, key, pull, cfg, app, hist, downloads, recent_text, recent_img, recent_files).await;
            }
        }
        protocol::T_BLOB_REQUEST => {
            if let Ok(br) = serde_json::from_value::<BlobRequest>(d) {
                if let Some(hold) = on_demand.get(&br.id) {
                    let bytes = match hold {
                        Hold::Sealed(b) => b.clone(),
                        Hold::Plain(p) => match std::fs::read(p) {
                            Ok(b) => b,
                            Err(_) => return,
                        },
                    };
                    let _ = blob::put_blob(http, &cfg.server_url, &cfg.token, bytes).await;
                }
            }
        }
        protocol::T_ROSTER => {
            if let Ok(r) = serde_json::from_value::<Roster>(d) {
                set_roster(app, roster, r.devices);
            }
        }
        protocol::T_PRESENCE => {
            if let Ok(p) = serde_json::from_value::<Presence>(d) {
                apply_presence(app, roster, p);
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    ev: ClipEvent,
    key: &Option<(Vec<u8>, String)>,
    pull: &reqwest::Client,
    cfg: &Config,
    app: &AppHandle,
    hist: &Arc<Mutex<History>>,
    downloads: &Path,
    recent_text: &mut VecDeque<String>,
    recent_img: &mut VecDeque<String>,
    recent_files: &mut VecDeque<String>,
) {
    if ev.is_blob() {
        let data = match blob::get_blob(pull, &cfg.server_url, &cfg.token, &ev.blob_id).await {
            Ok(d) => d,
            Err(e) => {
                notify(app, "CopySync", &format!("파일 받기 실패: {e}"));
                return;
            }
        };
        let plain = match (&ev.enc, key) {
            (Some(_), Some((k, _))) => match e2e::open(k, &data) {
                Ok(p) => p,
                Err(_) => {
                    notify(app, "CopySync", "받은 파일을 복호화할 수 없습니다 (암호문?)");
                    return;
                }
            },
            (Some(_), None) => {
                notify(app, "CopySync", "암호화된 파일을 받았지만 암호문이 설정되지 않았습니다");
                return;
            }
            (None, _) => data,
        };
        let _ = std::fs::create_dir_all(downloads);
        let name = if ev.name.is_empty() {
            ev.blob_id.trim_start_matches("sha256:").to_string()
        } else {
            ev.name.clone()
        };
        let path = downloads.join(&name);
        let _ = std::fs::write(&path, &plain);
        let is_image = ev.mime.first().map(|m| m.starts_with("image/")).unwrap_or(false);
        if is_image {
            if let Ok(img) = clipboard::decode_image(&plain) {
                remember(recent_img, sha_hex(&img.rgba));
                let _ = clipboard::set_image(&img);
            }
            show_toast(app, &ev.origin_device, &format!("🖼 {name} · {}", human(plain.len())), thumb_data_uri(&plain));
        } else {
            // Eager (≤ threshold) file: put it on the clipboard so it's pasteable.
            if !ev.on_demand {
                let p = path.to_string_lossy().to_string();
                remember(recent_files, p.clone());
                let _ = clipboard::set_files(&[p]);
            }
            show_toast(app, &ev.origin_device, &format!("📎 {name} · {}", human(plain.len())), None);
        }
        add_history(hist, &ev.ts, ev.kind(), &ev.origin_device, "in", &path.to_string_lossy(),
            ev.mime.first().map(|s| s.as_str()).unwrap_or(""), plain.len() as i64, &ev.blob_id, &name);
        let _ = app.emit("clip", serde_json::json!({"direction":"in","kind":ev.kind(),"name":name,"size":plain.len(),"path":path.to_string_lossy()}));
        return;
    }

    // Text.
    let text = match (&ev.enc, key) {
        (Some(_), Some((k, _))) => {
            use base64::{engine::general_purpose::STANDARD, Engine};
            let raw = match STANDARD.decode(&ev.inline_text) {
                Ok(r) => r,
                Err(_) => return,
            };
            match e2e::open(k, &raw) {
                Ok(p) => String::from_utf8_lossy(&p).to_string(),
                Err(_) => {
                    notify(app, "CopySync", "받은 텍스트를 복호화할 수 없습니다 (암호문?)");
                    return;
                }
            }
        }
        (Some(_), None) => {
            notify(app, "CopySync", "암호화된 텍스트를 받았지만 암호문이 설정되지 않았습니다");
            return;
        }
        (None, _) => ev.inline_text.clone(),
    };
    // Optional rich-text (HTML) variant.
    let html: Option<String> = if ev.html.is_empty() {
        None
    } else {
        match (&ev.enc, key) {
            (Some(_), Some((k, _))) => {
                use base64::{engine::general_purpose::STANDARD, Engine};
                STANDARD
                    .decode(&ev.html)
                    .ok()
                    .and_then(|raw| e2e::open(k, &raw).ok())
                    .map(|p| String::from_utf8_lossy(&p).to_string())
            }
            (Some(_), None) => None,
            (None, _) => Some(ev.html.clone()),
        }
    };
    remember(recent_text, sha_hex(text.as_bytes()));
    let st = app.state::<AppState>();
    let mark = st.mark_sensitive.load(Ordering::Relaxed);
    let auto_clear = st.auto_clear_secs.load(Ordering::Relaxed);
    match &html {
        Some(h) => { let _ = clipboard::set_html(h, &text); }
        None if mark => { let _ = clipboard::set_text_sensitive(&text); }
        None => { let _ = clipboard::set_text(&text); }
    }
    if auto_clear > 0 {
        schedule_clear_text(text.clone(), auto_clear);
    }
    let row = add_history(hist, &ev.ts, "text", &ev.origin_device, "in", &text, "text/plain", text.len() as i64, "", "");
    if privacy::classify(&text, &[]).is_some() {
        // Received password-like clip: purge from local history after the TTL.
        schedule_purge(hist.clone(), row, cfg.sensitive_ttl_secs);
    }
    show_toast(app, &ev.origin_device, &preview(&text), None);
    let _ = app.emit("clip", serde_json::json!({"direction":"in","kind":"text","text":text,"origin":ev.origin_device}));
}

fn human(n: usize) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

#[allow(clippy::too_many_arguments)]
fn add_history(
    hist: &Arc<Mutex<History>>,
    ts: &str,
    kind: &str,
    origin: &str,
    dir: &str,
    preview: &str,
    mime: &str,
    size: i64,
    blob_id: &str,
    name: &str,
) -> i64 {
    if let Ok(h) = hist.lock() {
        h.add(ts, kind, origin, dir, preview, mime, size, blob_id, name).unwrap_or(-1)
    } else {
        -1
    }
}

// ----------------------------------------------------------------- entrypoint

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .on_window_event(|window, event| match event {
            // Closing the main window hides it to the tray (the sync keeps running).
            WindowEvent::CloseRequested { api, .. } => {
                let _ = window.hide();
                api.prevent_close();
            }
            // The Quick Panel is dismissable: hide as soon as it loses focus.
            WindowEvent::Focused(false) if window.label() == "quickpanel" => {
                let _ = window.hide();
            }
            _ => {}
        })
        .setup(|app| {
            let dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("copysync"));
            let data = app.path().app_data_dir().unwrap_or_else(|_| dir.clone());
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::create_dir_all(&data);
            let hist = History::open(data.join("history.db"), Some(history_key(&data)))
                .map_err(|e| format!("open history: {e}"))?;
            let state = AppState {
                tx: Mutex::new(None),
                hist: Arc::new(Mutex::new(hist)),
                status: Arc::new(Mutex::new(Status::default())),
                roster: Arc::new(Mutex::new(Vec::new())),
                targets: Arc::new(Mutex::new(Targets::All)),
                tray: Mutex::new(None),
                cfg_path: dir.join("config.json"),
                downloads: data.join("downloads"),
                exclude_sensitive: Arc::new(AtomicBool::new(true)),
                quick_panel_shortcut: Arc::new(Mutex::new(
                    copysync_core::config::DEFAULT_QUICK_PANEL_SHORTCUT.to_string(),
                )),
                auto_clear_secs: Arc::new(AtomicU64::new(0)),
                mark_sensitive: Arc::new(AtomicBool::new(false)),
                reconnect: Arc::new(Notify::new()),
            };
            let cfg_path = state.cfg_path.clone();
            app.manage(state);
            let loaded = Config::load(&cfg_path).ok();
            // Adopt the saved Quick Panel hotkey (an old config without the field
            // deserializes to the default); unpaired clients keep the literal default.
            if let Some(accel) = loaded.as_ref().map(|c| c.quick_panel_shortcut.clone()) {
                *app.state::<AppState>().quick_panel_shortcut.lock().unwrap_or_else(|e| e.into_inner()) = accel;
            }
            if let Some(cfg) = loaded {
                start_sync(app.handle(), cfg);
            }

            // System tray: left-click opens the window; menu has 열기 / 풀 전환 / 종료.
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().expect("window icon"))
                .menu(&build_tray_menu(app.handle(), "", &[]).ok_or("tray menu")?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "show" => show_main(app),
                        "quit" => app.exit(0),
                        other => {
                            if let Some(name) = other.strip_prefix("pool:") {
                                let _ = enqueue(&app.state::<AppState>(), Cmd::SetPool(name.to_string()));
                            }
                        }
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            *app.state::<AppState>().tray.lock().unwrap_or_else(|e| e.into_inner()) = Some(tray);

            // Quick Panel: a configurable global hotkey (default Ctrl/Cmd+Shift+V,
            // editable in 설정 → 단축키) toggles a small always-on-top history
            // overlay for fast re-copy.
            {
                let accel = app
                    .state::<AppState>()
                    .quick_panel_shortcut
                    .lock()
                    .unwrap()
                    .clone();
                let _ = register_quick_panel(app.handle(), &accel);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_roster,
            set_targets,
            get_history,
            thumbnail,
            text_preview,
            send_text,
            send_file,
            get_autostart,
            set_autostart,
            get_privacy_filter,
            set_privacy_filter,
            get_auto_clear,
            set_auto_clear,
            get_mark_sensitive,
            set_mark_sensitive,
            discover_servers,
            reconnect,
            get_shortcut,
            set_shortcut,
            set_pool,
            pair,
            quickpanel_copy,
            hide_toast
        ])
        .run(tauri::generate_context!())
        .expect("error while running CopySync");
}
