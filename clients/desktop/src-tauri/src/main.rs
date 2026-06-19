// CopySync desktop client (Tauri + webkit/WebView2). The UI is a thin
// no-framework SPA; all protocol/crypto/networking lives in copysync-core, the
// same crate that is headlessly interop-tested against the Go server and copyctl.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

use copysync_core::clipboard;
use copysync_core::engine::{self, Cmd, Emitter as EngineEmitter, EngineState, RosterDevice, Status};
use copysync_core::history::{Entry, History};
use copysync_core::protocol::Targets;
use copysync_core::{pairing, Config};

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
    /// The app data dir (for the engine's outbound-image cache).
    data_dir: PathBuf,
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

/// The engine's UI sink, wired to Tauri's `app.emit` (the WebView listens to these
/// exact event names) plus an OS notification for `notify`.
struct TauriEmitter(AppHandle);

impl EngineEmitter for TauriEmitter {
    fn status(&self, s: &Status) {
        let _ = self.0.emit("status", s);
        rebuild_tray(&self.0); // keep the tray's pool submenu in sync with status
    }
    fn clip(&self, payload: serde_json::Value) {
        let _ = self.0.emit("clip", payload);
    }
    fn roster(&self, r: &[RosterDevice]) {
        let _ = self.0.emit("roster", r);
    }
    fn reconnect(&self, info: String) {
        let _ = self.0.emit("reconnect", info);
    }
    fn error(&self, msg: String) {
        // dlog(): also land it on stderr + the Debug tab ("error" event).
        eprintln!("copysync: {msg}");
        let _ = self.0.emit("error", msg);
    }
    fn cliplog(&self, msg: String) {
        let _ = self.0.emit("cliplog", msg);
    }
    fn notify(&self, title: &str, body: &str) {
        let _ = self.0.notification().builder().title(title).body(body).show();
    }
}

#[tauri::command]
fn hide_toast(app: AppHandle) {
    if let Some(win) = app.get_webview_window("toast") {
        let _ = win.hide();
    }
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

    // Build the engine's shared state from the (re-used) AppState Arcs.
    let engine_state = EngineState {
        hist: state.hist.clone(),
        status: state.status.clone(),
        roster: state.roster.clone(),
        targets: state.targets.clone(),
        exclude_sensitive: state.exclude_sensitive.clone(),
        auto_clear_secs: state.auto_clear_secs.clone(),
        mark_sensitive: state.mark_sensitive.clone(),
        reconnect: state.reconnect.clone(),
        cfg_path: state.cfg_path.clone(),
        downloads: state.downloads.clone(),
        data_dir: state.data_dir.clone(),
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    *state.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());

    // The engine's UI sink: maps its callbacks onto the same Tauri events the
    // WebView already listens to (status/clip/roster/reconnect/error/cliplog).
    let emit: Arc<dyn EngineEmitter> = Arc::new(TauriEmitter(app.clone()));

    // OS-clipboard watcher on its own std thread (the engine API requires it).
    {
        let emit_cb = emit.clone();
        std::thread::spawn(move || engine::clipboard_loop(tx, emit_cb));
    }

    let emit_sup = emit.clone();
    let status = state.status.clone();
    tauri::async_runtime::spawn(async move {
        engine::run(cfg, engine_state, emit, rx).await;
        // The sync engine stopped (bad config / fatal error). Surface it instead of
        // letting later actions fail silently with "sync not running".
        emit_sup.error(
            "동기화가 멈췄습니다 — 설정 → 기기 페어링에서 다시 연결하거나 앱을 재시작하세요.".to_string(),
        );
        let snap = {
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            s.connected = false;
            s.reconnecting = false;
            s.clone()
        };
        emit_sup.status(&snap);
    });
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
                data_dir: data.clone(),
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
