//! Tauri-free sync engine — the actor + helpers ported out of the Tauri desktop
//! `main.rs` so a native (non-WebView) client can reuse the exact same hardened
//! protocol/reconnect/crypto logic. The two Tauri couplings are abstracted away:
//!
//!   * [`Emitter`]    — UI sink (status/clip/roster/error/toast); the host wires
//!                      it to whatever UI it has (egui, Tauri `app.emit`, …).
//!   * [`EngineState`] — engine-owned shared state (history, status, roster,
//!                      targets, flags, reconnect signal, paths).
//!
//! Everything else is byte-for-byte the same as the Tauri copy: the watchdog +
//! exponential backoff reconnect loop, echo-dedup, on-demand blob holds, the
//! Windows sensitive-clipboard path, and the per-failure disconnect-reason logging.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use crate::clipboard::{self, Image};
use crate::history::History;
use crate::protocol::{
    self, BlobRequest, ClipEvent, DeviceInfo, EncMeta, Presence, Roster, Targets,
};
use crate::{blob, e2e, pinning, privacy, ws, Config};

/// Commands flowing into the sync actor.
pub enum Cmd {
    LocalText { text: String, html: Option<String> }, // OS-clipboard text/rich-text change
    LocalImage(Image),   // OS-clipboard image change (echo-guarded)
    SendText(String),    // explicit text send from the UI
    SendFile(String),    // explicit file send from the UI (path)
    LocalFiles(Vec<String>), // OS-clipboard file copy (CF_HDROP); echo-guarded
    SetPool(String),     // switch this device's share pool
}

/// A blob held for on-demand upload when the server asks (`blob_request`).
pub enum Hold {
    Sealed(Vec<u8>), // E2E: exact ciphertext (sha must match what was advertised)
    Plain(PathBuf),  // plaintext: re-read on demand
}

#[derive(Clone, Serialize, Default)]
pub struct Status {
    pub paired: bool,
    pub connected: bool,
    pub server_name: String,
    pub device_name: String,
    pub server_id: String,
    pub e2e: bool,
    pub pool: String,
    pub pools: Vec<String>,
    /// True while disconnected-but-paired and actively retrying.
    pub reconnecting: bool,
}

#[derive(Clone, Serialize, Default)]
pub struct RosterDevice {
    pub id: String,
    pub name: String,
    pub online: bool,
}

/// UI sink — the engine calls these instead of Tauri's app.emit / notify.
pub trait Emitter: Send + Sync + 'static {
    fn status(&self, s: &Status);
    fn clip(&self, payload: serde_json::Value); // the {"direction":..,"kind":..,..} json
    fn roster(&self, r: &[RosterDevice]);
    fn reconnect(&self, info: String);
    fn error(&self, msg: String); // dlog() + emit("error") sink
    fn cliplog(&self, msg: String); // emit("cliplog") verbose diagnostics
    fn notify(&self, title: &str, body: &str); // OS toast (notify()/show_toast)
}

/// Engine-owned shared state — replaces Tauri's AppState (non-Tauri fields only).
pub struct EngineState {
    pub hist: Arc<Mutex<History>>,
    pub status: Arc<Mutex<Status>>,
    pub roster: Arc<Mutex<Vec<RosterDevice>>>,
    pub targets: Arc<Mutex<Targets>>,
    pub exclude_sensitive: Arc<AtomicBool>,
    pub auto_clear_secs: Arc<AtomicU64>,
    pub mark_sensitive: Arc<AtomicBool>,
    pub reconnect: Arc<Notify>,
    pub cfg_path: PathBuf,
    pub downloads: PathBuf,
    pub data_dir: PathBuf, // for cache_outbound_image's clip-out dir
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

fn preview(s: &str) -> String {
    let t: String = s.chars().take(80).collect();
    if s.chars().count() > 80 {
        format!("{t}…")
    } else {
        t
    }
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

/// Delete a (sensitive) history row after a TTL so secrets don't linger.
fn schedule_purge(hist: Arc<Mutex<History>>, id: i64, ttl_secs: u64) {
    if id < 0 || ttl_secs == 0 {
        return;
    }
    tokio::spawn(async move {
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
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        if clipboard::get_text().map(|t| t == expected).unwrap_or(false) {
            let _ = clipboard::clear();
        }
    });
}

// ----------------------------------------------------------------- clipboard watcher

/// The OS-clipboard polling loop (runs on its own std::thread). Emits `Cmd`s for
/// text/image/file changes and surfaces odd/unhandled clipboard states via
/// `emit.cliplog`. Identical logic to the Tauri build.
pub fn clipboard_loop(tx: UnboundedSender<Cmd>, emit: Arc<dyn Emitter>) {
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
                    emit.cliplog(format!(
                        "RDP/가상 파일 복사 감지: {} — CF_HDROP가 아닌 스트리밍 파일(FileContents)이라 아직 동기화하지 못합니다. 포맷=[{}]",
                        names.join(", "),
                        formats.join(", ")
                    ));
                } else if text.is_err() {
                    emit.cliplog(format!(
                        "클립보드 읽기 실패 (RDP가 점유 중일 수 있음) · 포맷=[{}]",
                        formats.join(", ")
                    ));
                } else if !formats.is_empty() {
                    emit.cliplog(format!(
                        "처리하지 못한 클립보드 변경 · 포맷=[{}]",
                        formats.join(", ")
                    ));
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

// ----------------------------------------------------------------- the actor

/// The sync engine. Owns the WebSocket control channel: dials with pinned TLS,
/// runs the watchdog + exponential-backoff reconnect loop, and pumps `Cmd`s out /
/// frames in. Returns only when the command channel closes (host shut down).
pub async fn run(
    cfg: Config,
    state: EngineState,
    emit: Arc<dyn Emitter>,
    mut rx: UnboundedReceiver<Cmd>,
) {
    let mut cfg = cfg;
    let cfg_path = state.cfg_path.clone();
    let hist = state.hist.clone();
    let status = state.status.clone();
    let roster = state.roster.clone();
    let downloads = state.downloads.clone();

    // Liveness: ping this often; reconnect if no frame arrives within the watchdog
    // (the server pings every 30s, so 75s = ~2 missed pings = a dead link).
    const WS_PING_EVERY: Duration = Duration::from_secs(30);
    const WS_WATCHDOG: Duration = Duration::from_secs(75);

    let pin = match cfg.pin_bytes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bad pin: {e}");
            set_connected(&*emit, &status, false);
            emit.error("잘못된 SPKI 핀입니다 — 설정 → 기기 페어링에서 다시 연결하세요.".to_string());
            return;
        }
    };
    let key = cfg.e2e_key();
    let custom_res = cfg.custom_regexes();
    let exclude_flag = state.exclude_sensitive.clone();
    let http = pinning::http_client(pin);
    let pull = blob::pull_client(pin);
    let mut seq: u64 = 0;
    let mut recent_text: VecDeque<String> = VecDeque::new();
    let mut recent_img: VecDeque<String> = VecDeque::new();
    let mut recent_files: VecDeque<String> = VecDeque::new();
    let mut on_demand: HashMap<String, Hold> = HashMap::new();
    let reconnect = state.reconnect.clone();
    let mut attempt: u32 = 0;

    loop {
        match ws::connect(&cfg.server_url, pin, &cfg.device_id, &cfg.device_name, &cfg.token).await {
            Ok((mut sock, hello)) => {
                attempt = 0;
                let threshold = hello.on_demand_threshold;
                set_roster(&*emit, &roster, hello.roster.clone());
                {
                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                    s.pool = hello.pool.clone();
                    s.pools = hello.pools.clone();
                    s.reconnecting = false;
                }
                set_connected(&*emit, &status, true);
                // Liveness watchdog: ping every WS_PING_EVERY and reconnect if no
                // frame arrives within WS_WATCHDOG (the server pings every 30s).
                // Catches a silently dead TCP link — RDP/network drop, suspend.
                let mut last_recv = Instant::now();
                let mut ping_at = tokio::time::interval(WS_PING_EVERY);
                ping_at.tick().await;
                let mut watchdog = tokio::time::interval(Duration::from_secs(15));
                watchdog.tick().await;
                let why: String = 'inner: loop {
                    tokio::select! {
                        _ = ping_at.tick() => {
                            if let Err(e) = ws::ping(&mut sock).await {
                                break 'inner format!("핑(keepalive) 전송 실패: {e}");
                            }
                        }
                        _ = watchdog.tick() => {
                            if last_recv.elapsed() > WS_WATCHDOG {
                                break 'inner format!(
                                    "{}초 동안 서버 프레임 없음 — watchdog({}s) 초과로 연결 끊김 판단",
                                    last_recv.elapsed().as_secs(), WS_WATCHDOG.as_secs()
                                );
                            }
                        }
                        _ = reconnect.notified() => break 'inner "사용자가 재연결을 요청함".to_string(),
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
                                        emit.clip(serde_json::json!({"direction":"out","kind":"text","text":text,"sensitive":reason.label()}));
                                        continue;
                                    }
                                }
                                if !send_text_clip(&mut sock, &mut seq, &text, html.as_deref(), &key, current_targets(&state.targets), &*emit, &hist).await { break 'inner "텍스트 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::SendText(t)) => {
                                remember(&mut recent_text, sha_hex(t.as_bytes()));
                                if !send_text_clip(&mut sock, &mut seq, &t, None, &key, current_targets(&state.targets), &*emit, &hist).await { break 'inner "텍스트 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::LocalImage(img)) => {
                                let sha = sha_hex(&img.rgba);
                                if seen(&recent_img, &sha) { continue; }
                                remember(&mut recent_img, sha);
                                if !send_image_clip(&mut sock, &mut seq, &img, &key, current_targets(&state.targets), &*emit, &hist, &http, &cfg, &state.data_dir).await { break 'inner "이미지 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::SendFile(p)) => {
                                if !send_file_clip(&mut sock, &mut seq, &p, &key, current_targets(&state.targets), threshold, &mut on_demand, &*emit, &hist, &http, &cfg).await { break 'inner "파일 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::LocalFiles(files)) => {
                                for p in files {
                                    // Skip a file we just placed on the clipboard from an inbound clip (echo).
                                    if seen(&recent_files, &p) { continue; }
                                    if !send_file_clip(&mut sock, &mut seq, &p, &key, current_targets(&state.targets), threshold, &mut on_demand, &*emit, &hist, &http, &cfg).await { break 'inner "파일 전송 실패 — 제어 채널 끊김".to_string(); }
                                }
                            }
                            Some(Cmd::SetPool(name)) => {
                                if ws::send(&mut sock, protocol::T_SET_POOL, &protocol::SetPool { pool: name.clone() }).await.is_err() { break 'inner "풀 설정 전송 실패".to_string(); }
                                let snap = { let mut s = status.lock().unwrap_or_else(|e| e.into_inner()); s.pool = name; s.clone() };
                                emit.status(&snap);
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
                                            emit.error("토큰 회전 — 새 인증 토큰 저장됨".to_string());
                                        }
                                    }
                                } else {
                                    handle_frame(t, d, &key, &pull, &http, &cfg, &*emit, &state, &hist, &downloads, &roster, &mut recent_text, &mut recent_img, &mut recent_files, &on_demand).await;
                                }
                            }
                            Ok(None) => break 'inner "서버가 연결을 종료함".to_string(),
                            Err(e) => break 'inner format!("수신 오류: {e}"),
                        }
                    }
                };
                emit.error(format!("연결 종료 — {why} · 자동 재연결 시도"));
                set_connected(&*emit, &status, false);
            }
            Err(e) => {
                eprintln!("connect failed: {e}");
                emit.error(format!("연결 실패 — 재시도 중: {e}"));
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
        emit.status(&snap);
        emit.reconnect(format!("{attempt}회 · {}초 후 재시도", delay.as_secs().max(1)));
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

fn set_connected(emit: &dyn Emitter, status: &Arc<Mutex<Status>>, on: bool) {
    let snapshot = {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.connected = on;
        s.clone()
    };
    emit.status(&snapshot);
}

fn set_roster(emit: &dyn Emitter, roster: &Arc<Mutex<Vec<RosterDevice>>>, devices: Vec<DeviceInfo>) {
    let list: Vec<RosterDevice> = devices
        .iter()
        .map(|d| RosterDevice {
            id: d.device.id.clone(),
            name: d.device.name.clone(),
            online: d.online,
        })
        .collect();
    *roster.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
    emit.roster(&list);
}

fn apply_presence(emit: &dyn Emitter, roster: &Arc<Mutex<Vec<RosterDevice>>>, p: Presence) {
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
    emit.roster(&snapshot);
}

// ----------------------------------------------------------------- outbound

async fn send_text_clip(
    sock: &mut ws::Ws,
    seq: &mut u64,
    text: &str,
    html: Option<&str>,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    emit: &dyn Emitter,
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
        Err(e) => { emit.error(format!("텍스트 클립 생성 실패(E2E 암호화?) — 전송 안 함: {e}")); return true; }
    };
    if ws::send(sock, protocol::T_CLIP, &ev).await.is_err() {
        return false;
    }
    add_history(hist, &ev.ts, "text", "me", "out", text, "text/plain", text.len() as i64, "", "");
    emit.clip(serde_json::json!({"direction":"out","kind":"text","text":text}));
    true
}

/// Cache an outbound clipboard image locally so the history can render its
/// thumbnail (inbound images already persist a file; outbound only kept a label).
fn cache_outbound_image(data_dir: &Path, png: &[u8]) -> Option<String> {
    let dir = data_dir.join("clip-out");
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
    emit: &dyn Emitter,
    hist: &Arc<Mutex<History>>,
    http: &reqwest::Client,
    cfg: &Config,
    data_dir: &Path,
) -> bool {
    let png = match clipboard::encode_png(img) {
        Ok(p) => p,
        Err(e) => { emit.error(format!("이미지 PNG 인코딩 실패 — 전송 안 함: {e}")); return true; }
    };
    let payload = match key {
        Some((k, _)) => match e2e::seal(k, &png) {
            Ok(c) => c,
            Err(e) => { emit.error(format!("이미지 E2E 암호화 실패 — 전송 안 함: {e}")); return true; }
        },
        None => png.clone(),
    };
    let bid = match blob::put_blob(http, &cfg.server_url, &cfg.token, payload.clone()).await {
        Ok(b) => b,
        Err(e) => {
            emit.error(format!("이미지 blob 업로드 실패 — 전송 안 함: {e}"));
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
    let prev = cache_outbound_image(data_dir, &png).unwrap_or_else(|| "(클립보드 이미지)".into());
    add_history(hist, &ev.ts, "image", "me", "out", &prev, "image/png", png.len() as i64, &ev.blob_id, "clipboard.png");
    emit.clip(serde_json::json!({"direction":"out","kind":"image","name":"clipboard.png","size":png.len()}));
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
    emit: &dyn Emitter,
    hist: &Arc<Mutex<History>>,
    http: &reqwest::Client,
    cfg: &Config,
) -> bool {
    let p = Path::new(path);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) => {
            emit.notify("CopySync", &format!("파일을 열 수 없습니다: {e}"));
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
                    Err(e) => { emit.notify("CopySync", &format!("읽기 실패: {e}")); return true; }
                };
                let ct = match e2e::seal(k, &data) { Ok(c) => c, Err(e) => { emit.error(format!("파일 E2E 암호화 실패 — 전송 안 함: {e}")); return true; } };
                let sha = sha_hex(&ct);
                let bid = format!("sha256:{sha}");
                on_demand.insert(bid.clone(), Hold::Sealed(ct));
                (bid, sha)
            }
            None => {
                let sha = match file_sha_hex(p) { Ok(s) => s, Err(e) => { emit.notify("CopySync", &format!("읽기 실패: {e}")); return true; } };
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
            Err(e) => { emit.notify("CopySync", &format!("읽기 실패: {e}")); return true; }
        };
        let payload = match key {
            Some((k, _)) => match e2e::seal(k, &data) { Ok(c) => c, Err(e) => { emit.error(format!("파일 E2E 암호화 실패 — 전송 안 함: {e}")); return true; } },
            None => data,
        };
        let bid = match blob::put_blob(http, &cfg.server_url, &cfg.token, payload.clone()).await {
            Ok(b) => b,
            Err(e) => { emit.notify("CopySync", &format!("업로드 실패: {e}")); return true; }
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
    emit.clip(serde_json::json!({"direction":"out","kind":kind,"name":name,"size":size,"onDemand":ev.on_demand}));
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
    emit: &dyn Emitter,
    state: &EngineState,
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
            match serde_json::from_value::<ClipEvent>(d) {
                Ok(ev) => handle_incoming(ev, key, pull, cfg, emit, state, hist, downloads, recent_text, recent_img, recent_files).await,
                Err(e) => emit.error(format!("받은 클립 디코딩 실패: {e}")),
            }
        }
        protocol::T_BLOB_REQUEST => {
            if let Ok(br) = serde_json::from_value::<BlobRequest>(d) {
                if let Some(hold) = on_demand.get(&br.id) {
                    let bytes = match hold {
                        Hold::Sealed(b) => b.clone(),
                        Hold::Plain(p) => match std::fs::read(p) {
                            Ok(b) => b,
                            Err(e) => { emit.error(format!("요청받은 파일 읽기 실패: {e}")); return; }
                        },
                    };
                    let _ = blob::put_blob(http, &cfg.server_url, &cfg.token, bytes).await;
                }
            }
        }
        protocol::T_ROSTER => {
            if let Ok(r) = serde_json::from_value::<Roster>(d) {
                set_roster(emit, roster, r.devices);
            }
        }
        protocol::T_PRESENCE => {
            if let Ok(p) = serde_json::from_value::<Presence>(d) {
                apply_presence(emit, roster, p);
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
    emit: &dyn Emitter,
    state: &EngineState,
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
                emit.notify("CopySync", &format!("파일 받기 실패: {e}"));
                return;
            }
        };
        let plain = match (&ev.enc, key) {
            (Some(_), Some((k, _))) => match e2e::open(k, &data) {
                Ok(p) => p,
                Err(_) => {
                    emit.notify("CopySync", "받은 파일을 복호화할 수 없습니다 (암호문?)");
                    return;
                }
            },
            (Some(_), None) => {
                emit.notify("CopySync", "암호화된 파일을 받았지만 암호문이 설정되지 않았습니다");
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
            match clipboard::decode_image(&plain) {
                Ok(img) => {
                    remember(recent_img, sha_hex(&img.rgba));
                    if let Err(e) = clipboard::set_image(&img) {
                        emit.error(format!("받은 이미지 클립보드 적용 실패(RDP 경합?): {e}"));
                    }
                }
                Err(e) => emit.error(format!("받은 이미지 디코딩 실패: {e}")),
            }
            emit.notify(&clip_origin_name(state, &ev.origin_device), &format!("🖼 {name} · {}", human(plain.len())));
        } else {
            // Eager (≤ threshold) file: put it on the clipboard so it's pasteable.
            if !ev.on_demand {
                let p = path.to_string_lossy().to_string();
                remember(recent_files, p.clone());
                if let Err(e) = clipboard::set_files(&[p]) {
                    emit.error(format!("받은 파일 클립보드 적용 실패(RDP 경합?): {e}"));
                }
            }
            emit.notify(&clip_origin_name(state, &ev.origin_device), &format!("📎 {name} · {}", human(plain.len())));
        }
        add_history(hist, &ev.ts, ev.kind(), &ev.origin_device, "in", &path.to_string_lossy(),
            ev.mime.first().map(|s| s.as_str()).unwrap_or(""), plain.len() as i64, &ev.blob_id, &name);
        emit.clip(serde_json::json!({"direction":"in","kind":ev.kind(),"name":name,"size":plain.len(),"path":path.to_string_lossy()}));
        return;
    }

    // Text.
    let text = match (&ev.enc, key) {
        (Some(_), Some((k, _))) => {
            use base64::{engine::general_purpose::STANDARD, Engine};
            let raw = match STANDARD.decode(&ev.inline_text) {
                Ok(r) => r,
                Err(e) => { emit.error(format!("받은 텍스트 base64 디코딩 실패: {e}")); return; }
            };
            match e2e::open(k, &raw) {
                Ok(p) => String::from_utf8_lossy(&p).to_string(),
                Err(_) => {
                    emit.notify("CopySync", "받은 텍스트를 복호화할 수 없습니다 (암호문?)");
                    return;
                }
            }
        }
        (Some(_), None) => {
            emit.notify("CopySync", "암호화된 텍스트를 받았지만 암호문이 설정되지 않았습니다");
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
    let mark = state.mark_sensitive.load(Ordering::Relaxed);
    let auto_clear = state.auto_clear_secs.load(Ordering::Relaxed);
    let applied = match &html {
        Some(h) => clipboard::set_html(h, &text),
        None if mark => clipboard::set_text_sensitive(&text),
        None => clipboard::set_text(&text),
    };
    if let Err(e) = applied {
        emit.error(format!("받은 텍스트 클립보드 적용 실패(RDP 경합?): {e}"));
    }
    if auto_clear > 0 {
        schedule_clear_text(text.clone(), auto_clear);
    }
    let row = add_history(hist, &ev.ts, "text", &ev.origin_device, "in", &text, "text/plain", text.len() as i64, "", "");
    if privacy::classify(&text, &[]).is_some() {
        // Received password-like clip: purge from local history after the TTL.
        schedule_purge(hist.clone(), row, cfg.sensitive_ttl_secs);
    }
    emit.notify(&clip_origin_name(state, &ev.origin_device), &preview(&text));
    emit.clip(serde_json::json!({"direction":"in","kind":"text","text":text,"origin":ev.origin_device}));
}

/// Resolve a sender device-id to its friendly roster name for toast titles
/// (replaces the Tauri `show_toast` name lookup). Falls back to "다른 기기".
fn clip_origin_name(state: &EngineState, origin: &str) -> String {
    let r = state.roster.lock().unwrap_or_else(|e| e.into_inner());
    r.iter()
        .find(|d| d.id == origin)
        .map(|d| d.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "다른 기기".to_string())
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
