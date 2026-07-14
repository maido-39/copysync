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

// ----------------------------------------------------------------- detailed debug log
//
// A process-wide, opt-in verbose event log. When enabled it appends timestamped,
// target-tagged lines to `dirs::config_dir()/copysync/logs/engine.log` (with a
// simple size-based rotation to `engine.log.1`). Enabled by either the env var
// `COPYSYNC_DEBUG=1` (also accepts `true`/`yes`/`on`) or `Config.debug = true`
// (see `set_debug_from_config`). This is the sink behind the `dlog!` macro that
// the engine + clipboard layer use so no error path is silently swallowed.

/// Global debug switch. Starts from the env var; `set_debug_from_config` can also
/// flip it on from the persisted config flag at engine start.
static DEBUG_ON: AtomicBool = AtomicBool::new(false);
/// One-time init guard for the env-var probe.
static DEBUG_INIT: std::sync::Once = std::sync::Once::new();
/// Serializes writes + rotation across the (few) threads that log.
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// ~5 MB before we rotate `engine.log` → `engine.log.1`.
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// True if detailed debug logging is enabled. Reads (and caches) `COPYSYNC_DEBUG`
/// on first call; thereafter just an atomic load.
pub fn debug_enabled() -> bool {
    DEBUG_INIT.call_once(|| {
        let on = std::env::var("COPYSYNC_DEBUG")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes" || v == "on"
            })
            .unwrap_or(false);
        if on {
            DEBUG_ON.store(true, Ordering::Relaxed);
        }
    });
    DEBUG_ON.load(Ordering::Relaxed)
}

/// Turn on detailed debug logging from the host's persisted config (the
/// `COPYSYNC_DEBUG` env var still wins / is additive). Called once at engine
/// start. `Config::debug_logging()` lets a host opt in programmatically.
pub fn set_debug_from_config(cfg: &Config) {
    let _ = debug_enabled(); // ensure the env probe has run
    if cfg.debug_logging() {
        DEBUG_ON.store(true, Ordering::Relaxed);
    }
}

/// Force detailed debug logging on for the rest of the process (host opt-in,
/// e.g. from a UI toggle). The `COPYSYNC_DEBUG` env var is the primary switch;
/// this is the programmatic equivalent.
pub fn force_debug() {
    let _ = debug_enabled(); // ensure the env probe has run
    DEBUG_ON.store(true, Ordering::Relaxed);
}

/// Resolve the engine log file path, creating the parent dir. Returns None if no
/// config dir is available (then logging silently no-ops, like before).
fn log_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("copysync").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("engine.log"))
}

/// Append one already-formatted line (target + message) to the engine log with an
/// RFC3339 UTC timestamp. Never panics; failures to write are themselves dropped
/// (there is nowhere safer to report them). No-op unless debug is enabled.
pub fn debug_log(target: &str, msg: &str) {
    if !debug_enabled() {
        return;
    }
    let ts = {
        use time::{format_description::well_known::Rfc3339, OffsetDateTime};
        OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
    };
    let line = format!("{ts} [{target}] {msg}\n");
    let _guard = LOG_LOCK.lock();
    let Some(path) = log_path() else { return };
    // Size-based rotation: keep one previous file.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() >= LOG_ROTATE_BYTES {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Verbose debug logging to the engine file. `dlog!(target, "fmt", args…)`.
/// Cheap when disabled (an atomic load short-circuits before formatting).
macro_rules! dlog {
    ($target:expr, $($arg:tt)*) => {{
        if $crate::engine::debug_enabled() {
            $crate::engine::debug_log($target, &format!($($arg)*));
        }
    }};
}

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

/// Result of an OFF-LOOP blob network op, sent back to the connection's `select!`
/// loop over an internal channel. Moving `put_blob`/`get_blob` off the loop (into
/// `tokio::spawn`ed workers) is what keeps `ws::recv` polled while a transfer is in
/// flight, so the WS pong/ping/watchdog arms stay live and the server doesn't reap
/// a genuinely-alive connection mid-transfer (see the on-demand long-poll case).
enum BlobResult {
    /// An outbound upload finished. On success the loop emits the already-built
    /// `T_CLIP` frame; on failure it surfaces the error and emits nothing.
    Upload {
        outcome: Result<(), (&'static str, String)>, // Ok(()) | Err((class, msg))
        ev: ClipEvent,
        // history bookkeeping (recorded on the loop after a successful send)
        hist_kind: &'static str,
        hist_preview: String,
        hist_mime: String,
        hist_size: i64,
        hist_name: String,
        // the {"direction":"out",...} json to emit on success
        emit_json: serde_json::Value,
        // user-facing error sink: true => emit.error, false => emit.notify
        err_via_error: bool,
        // idx 9: content hash to record in the local echo-dedup ring ONLY after a
        // successful upload+advertise (images set this; files leave it None). This
        // ties echo suppression to clips that genuinely went out, so a re-copy of
        // an image whose upload failed isn't silently swallowed.
        echo_img_sha: Option<String>,
    },
    /// An on-demand `blob_request` upload finished. On success the loop drops the
    /// hold from `on_demand` so the map can't grow without bound.
    Serve {
        id: String,
        ok: bool,
    },
    /// An inbound blob download finished. On success the loop decrypts + applies
    /// the bytes to the clipboard / history (the tail of `handle_incoming`).
    Download {
        outcome: Result<Vec<u8>, (&'static str, String)>,
        ev: ClipEvent,
    },
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

/// A content-hash echo-dedup ring entry: the hash plus the instant it was
/// recorded, so `seen` can suppress only a *recent* echo (the inbound
/// set_*→watcher round-trip) and NOT a deliberate later re-copy of identical
/// content (idx 10).
type Dedup = VecDeque<(String, Instant)>;

/// How long a remembered hash suppresses a matching clip. Long enough to swallow
/// the inbound-apply → clipboard-watcher re-read round-trip (which happens within
/// a poll tick or two, i.e. well under a second), short enough that a deliberate
/// later re-copy of the same content re-syncs (idx 10).
const ECHO_TTL: Duration = Duration::from_secs(3);

fn remember(q: &mut Dedup, sha: String) {
    let now = Instant::now();
    // Refresh the timestamp if we already hold this hash, so a fresh echo window
    // starts; otherwise append. Either way keep the ring bounded to 64.
    if let Some(slot) = q.iter_mut().find(|(x, _)| x == &sha) {
        slot.1 = now;
        return;
    }
    if q.len() >= 64 {
        q.pop_front();
    }
    q.push_back((sha, now));
}

fn seen(q: &Dedup, sha: &str) -> bool {
    q.iter()
        .any(|(x, t)| x == sha && t.elapsed() < ECHO_TTL)
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
/// Per-run clipboard watcher state: the last-seen sequence number plus per-format
/// last-hashes for echo/dedup. Threaded through [`poll_once`] so the same read
/// body serves both the event-driven (Windows) and polling (other OSes / fallback)
/// drivers below.
struct ClipState {
    last_seq: Option<u32>,
    last_text: String,
    last_img: String,
    last_files: String,
    had_rich_last: bool,
}

impl ClipState {
    fn new() -> Self {
        ClipState {
            last_seq: None,
            last_text: String::new(),
            last_img: String::new(),
            last_files: String::new(),
            had_rich_last: false,
        }
    }
}

/// The OS-clipboard watcher.
///
/// Windows: **event-driven** via `AddClipboardFormatListener` (through
/// `clipboard_win::Monitor`) — a hidden message-only window that unblocks the
/// moment the clipboard changes, so there is no idle polling and no up-to-800ms
/// latency. A tiny reaper thread drops the monitor's `Shutdown` handle when the
/// engine drops the `Cmd` receiver (`tx.is_closed()`), which unblocks `recv()` so
/// this thread exits — preserving the exact self-exit contract the polling loop
/// had. If the listener can't be created we fall back to polling.
///
/// Other OSes: polling (`arboard` exposes no change event). Same read body.
pub fn clipboard_loop(tx: UnboundedSender<Cmd>, emit: Arc<dyn Emitter>) {
    #[cfg(windows)]
    {
        match clipboard_win::Monitor::new() {
            Ok(mut monitor) => {
                // Reaper: translate "engine dropped rx" into a monitor shutdown so
                // the blocking recv() below returns and this thread ends. Holding a
                // tx clone does NOT keep rx alive (is_closed tracks the receiver).
                let shutdown = monitor.shutdown_channel();
                let tx_reaper = tx.clone();
                std::thread::spawn(move || {
                    while !tx_reaper.is_closed() {
                        std::thread::sleep(Duration::from_millis(1000));
                    }
                    drop(shutdown); // PostMessage → unblocks monitor.recv()
                });
                let mut st = ClipState::new();
                // Process whatever is already on the clipboard at startup, then react
                // to each change event.
                poll_once(&tx, &emit, &mut st);
                loop {
                    match monitor.recv() {
                        Ok(true) => {
                            if tx.is_closed() {
                                return;
                            }
                            poll_once(&tx, &emit, &mut st);
                        }
                        Ok(false) => return, // shutdown requested (engine stopped)
                        Err(e) => {
                            debug_log("clipboard", &format!("monitor.recv error: {e}; falling back to a poll tick"));
                            if tx.is_closed() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(300));
                            poll_once(&tx, &emit, &mut st);
                        }
                    }
                }
            }
            Err(e) => {
                debug_log("clipboard", &format!("AddClipboardFormatListener unavailable ({e}); using polling watcher"));
                clipboard_poll_loop(tx, emit);
            }
        }
    }
    #[cfg(not(windows))]
    clipboard_poll_loop(tx, emit);
}

/// Polling driver: run one read every 800ms until the engine drops the receiver.
/// Used on non-Windows and as the Windows fallback if the event listener fails.
fn clipboard_poll_loop(tx: UnboundedSender<Cmd>, emit: Arc<dyn Emitter>) {
    let mut st = ClipState::new();
    loop {
        poll_once(&tx, &emit, &mut st);
        std::thread::sleep(Duration::from_millis(800));
        if tx.is_closed() {
            return;
        }
    }
}

/// Read the current clipboard once, diff each format against `st`, and dispatch a
/// `Cmd` for whatever genuinely changed. Shared by the event-driven and polling
/// drivers. On Windows the sequence-number guard short-circuits an unchanged
/// clipboard (so a duplicate event is cheap); elsewhere `seq_num()` is None and
/// every call reads content.
fn poll_once(tx: &UnboundedSender<Cmd>, emit: &Arc<dyn Emitter>, st: &mut ClipState) {
    let last_seq = &mut st.last_seq;
    let last_text = &mut st.last_text;
    let last_img = &mut st.last_img;
    let last_files = &mut st.last_files;
    let had_rich_last = &mut st.had_rich_last;
    {
        // On Windows, only touch the clipboard when its sequence number changes —
        // re-opening it every tick contends with RDP's redirector and drops copies.
        // Elsewhere seq_num() is None, so we keep polling content every tick.
        let seq = clipboard::seq_num();
        let changed = seq.map_or(true, |s| Some(s) != *last_seq);
        if changed {
            // NOTE: do NOT commit `last_seq` yet — only after at least one format
            // read succeeds. A transiently-locked clipboard generation (RDP
            // redirector / a just-copied app still holding the clipboard) would
            // otherwise be marked "seen" and lost forever; instead we let it be
            // retried on the next tick.
            let mut read_ok = false;

            // FORMAT PRIORITY FIX: probe IMAGE and FILES *before* text. Many apps
            // put a text/URL fallback alongside an image or a file list; checking
            // text first used to shadow the richer format. So: if a CF_DIBV5 image
            // or a CF_HDROP file list is present, sync that; only treat the change
            // as plain text when neither an image nor a file format is available.
            // (Per-kind last-hash is preserved so a coexisting URL text is still
            // handled on its own generation per the existing logic.)
            let img_res = clipboard::get_image();
            let files_opt = clipboard::get_files();
            let text_res = clipboard::get_text();
            let had_image = img_res.is_ok();
            let had_files = files_opt.is_some();
            let mut handled = false;

            // Per-format dedup is now DECOUPLED from a whole-generation short-circuit.
            // We evaluate image, files, and text INDEPENDENTLY against their own
            // last-hash so a coexisting format whose hash didn't change (e.g. a
            // screenshot/clipboard-history tool keeping a persistent image present)
            // can never shadow a genuinely-new text/file copy in the same generation.
            // When MULTIPLE formats are simultaneously NEW, we prefer image > files >
            // text (preserving the original "richer format wins" fix) by only sending
            // one rich format and clearing the others' last-hash so they re-sync next.
            let img_changed = match &img_res {
                Ok(img) => {
                    read_ok = true;
                    sha_hex(&img.rgba) != *last_img
                }
                Err(_) => false,
            };
            let files_changed = match &files_opt {
                Some(files) => {
                    read_ok = true;
                    files.join("\u{1}") != *last_files
                }
                None => false,
            };
            let text_changed = match &text_res {
                Ok(t) if !t.is_empty() => {
                    read_ok = true;
                    *t != *last_text
                }
                Ok(_) => { read_ok = true; false } // empty text, nothing to send
                Err(e) => { dlog!("clipboard", "gen: text read failed: {e}"); false }
            };

            if img_changed {
                let img = img_res.expect("img_changed implies Ok");
                *last_img = sha_hex(&img.rgba);
                // idx 3 fix: a NEW image arrived; we prefer it over any coexisting
                // text/file fallback this generation. But do NOT clear last_text/
                // last_files to empty — that would make the SAME still-present
                // companion text/file compare unequal next tick and get spuriously
                // re-synced as its own clip. Instead snapshot whatever companion is
                // currently present as "already seen", so an unchanged companion is
                // deduped next tick while a genuinely-new later change still fires.
                *last_text = text_res.as_ref().ok().filter(|t| !t.is_empty()).cloned().unwrap_or_default();
                *last_files = files_opt.as_ref().map(|f| f.join("\u{1}")).unwrap_or_default();
                let (w, hh, bytes) = (img.width, img.height, img.rgba.len());
                dlog!("clipboard", "gen: chose IMAGE {w}x{hh} ({bytes}B rgba); preferred over text/files");
                let _ = tx.send(Cmd::LocalImage(img));
                handled = true;
            } else if files_changed {
                // Windows Explorer file copy (CF_HDROP).
                let files = files_opt.clone().expect("files_changed implies Some");
                *last_files = files.join("\u{1}");
                // idx 3 fix: same as the image branch — snapshot the coexisting
                // text/image companions as already-seen instead of clearing to
                // empty, so an unchanged companion isn't spuriously re-synced next
                // tick while a genuinely-new later change still fires.
                *last_text = text_res.as_ref().ok().filter(|t| !t.is_empty()).cloned().unwrap_or_default();
                *last_img = img_res.as_ref().ok().map(|img| sha_hex(&img.rgba)).unwrap_or_default();
                dlog!("clipboard", "gen: chose FILES ({} path(s)); preferred over text", files.len());
                let _ = tx.send(Cmd::LocalFiles(files));
                handled = true;
            } else if text_changed {
                // Neither a NEW image nor a NEW file list this generation — but the
                // text genuinely changed (even if a stale image/file is still present),
                // so sync it instead of letting an unchanged rich format shadow it.
                let t = text_res.as_ref().expect("text_changed implies Ok").clone();
                *last_text = t.clone();
                // Don't clear last_img/last_files here: an unchanged coexisting
                // image/file is still valid and must NOT be force-resynced next tick.
                let html = clipboard::get_html().ok().filter(|h| !h.is_empty());
                dlog!(
                    "clipboard",
                    "gen: chose TEXT ({} chars, html={}); no NEW image/file this generation",
                    t.chars().count(),
                    html.is_some()
                );
                let _ = tx.send(Cmd::LocalText { text: t, html });
                handled = true;
            } else if had_image {
                dlog!("clipboard", "gen: IMAGE unchanged (dedup) — skip");
            } else if had_files {
                dlog!("clipboard", "gen: FILES unchanged (dedup) — skip");
            } else if matches!(&text_res, Ok(t) if !t.is_empty()) {
                dlog!("clipboard", "gen: TEXT unchanged (dedup) — skip");
            }

            // Observability: if an image/file format *was* present this generation,
            // record it so any future re-introduction of text-shadowing is visible
            // in the debug feed even when dedup meant we sent nothing. idx 13: only
            // emit on the rising edge (a rich format newly appeared) — NOT every
            // tick — so a persistent image doesn't flood the feed on non-Windows
            // where every 800ms tick is a "generation".
            let had_rich = had_image || had_files;
            if had_rich && !*had_rich_last {
                emit.cliplog(format!(
                    "클립보드 우선순위: 이미지/파일 포맷 감지 (image={had_image}, files={had_files}) → 텍스트보다 우선 동기화"
                ));
            }
            *had_rich_last = had_rich;

            if read_ok {
                // Only now mark this generation as consumed.
                if let Some(s) = seq {
                    *last_seq = Some(s);
                }
            } else if seq.is_some() {
                dlog!(
                    "clipboard",
                    "gen: no format read succeeded (clipboard locked?) — NOT committing last_seq, will retry"
                );
            }

            // A clipboard change we couldn't turn into a normal text/image/file
            // event — surface what was actually there (this is where RDP file
            // copies and odd formats land). Shows up in the 디버깅 event log.
            if !handled && seq.is_some() {
                let formats = clipboard::list_formats();
                dlog!(
                    "clipboard",
                    "gen: unhandled change (read_ok={read_ok}) · formats=[{}]",
                    formats.join(", ")
                );
                if let Some(names) = clipboard::get_virtual_file_names() {
                    emit.cliplog(format!(
                        "RDP/가상 파일 복사 감지: {} — CF_HDROP가 아닌 스트리밍 파일(FileContents)이라 아직 동기화하지 못합니다. 포맷=[{}]",
                        names.join(", "),
                        formats.join(", ")
                    ));
                } else if !read_ok {
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
    // Enable detailed debug logging from the persisted flag (env var is additive).
    set_debug_from_config(&cfg);
    dlog!(
        "engine",
        "run() start · server={} device={} e2e={} debug_enabled={}",
        cfg.server_url,
        cfg.device_name,
        cfg.e2e_key().is_some(),
        debug_enabled()
    );
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
            dlog!("engine", "run() early-return: bad SPKI pin: {e}");
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
    let mut recent_text: Dedup = VecDeque::new();
    let mut recent_img: Dedup = VecDeque::new();
    let mut recent_files: Dedup = VecDeque::new();
    let mut on_demand: HashMap<String, Hold> = HashMap::new();
    // Insertion order for `on_demand`, so the bounded ring can evict the oldest
    // advertised-but-unfetched hold (see hold_on_demand) — the leak fix.
    let mut on_demand_order: VecDeque<String> = VecDeque::new();
    let reconnect = state.reconnect.clone();
    let mut attempt: u32 = 0;

    // idx 0 fix: the OFF-LOOP blob result channel lives for the WHOLE run, NOT
    // per-connection. A transfer that started on a prior connection (a slow
    // upload/download, or the server's up-to-60s on-demand long-poll) finishes
    // by sending its BlobResult here; if the channel were re-created per
    // connection, that result would land in a dropped receiver and be silently
    // lost — the clip would never be advertised (outbound) or applied (inbound).
    // Keeping it persistent means the result is still delivered on the NEW
    // socket. Declared like seq/recent_*/on_demand above.
    let (blob_tx, mut blob_rx) = tokio::sync::mpsc::unbounded_channel::<BlobResult>();
    // idx 0 fix: when an Upload result arrives but the current socket's T_CLIP
    // send fails (e.g. the link dropped between the upload finishing and the
    // advertise), we don't discard the clip — we stash the already-built
    // ClipEvent (plus its history/emit bookkeeping) here and re-advertise it at
    // the top of the next connection so the peers still learn about the blob.
    let mut pending_uploads: Vec<BlobResult> = Vec::new();

    loop {
        dlog!("ws", "connect: dialing {} (attempt #{attempt})", cfg.server_url);
        match ws::connect(&cfg.server_url, pin, &cfg.device_id, &cfg.device_name, &cfg.token).await {
            Ok((mut sock, hello)) => {
                attempt = 0;
                let threshold = hello.on_demand_threshold;
                // The server's per-frame WS read limit (idx 17). A text/HTML clip
                // whose JSON frame exceeds this is rejected by the server's read
                // pump and tears the control connection down, silently losing the
                // clip; send_text_clip refuses oversized text instead of sending a
                // frame guaranteed to be dropped. 0 = unknown/unset => no cap.
                let max_msg = hello.max_msg;
                dlog!(
                    "ws",
                    "connect: OK · pool={} pools={:?} on_demand_threshold={} roster={} device(s)",
                    hello.pool,
                    hello.pools,
                    threshold,
                    hello.roster.len()
                );
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
                // The OFF-LOOP blob result channel (blob_tx/blob_rx) is owned by
                // the WHOLE run (declared before the reconnect loop) — see the
                // idx 0 fix there. We poll blob_rx in this select! alongside
                // ws::recv/ping/watchdog so the WS read half keeps flushing pongs
                // while a (possibly 60s long-poll) transfer is in flight, AND so a
                // transfer that started on a previous connection still delivers
                // its result here instead of being silently dropped.
                //
                // idx 0 fix: re-advertise any uploads whose T_CLIP send failed on
                // a prior (now-dead) connection. The blob bytes are already on the
                // server; we just need to tell the peers about them on this fresh
                // socket. Done before entering the select! so it can't be starved.
                if !pending_uploads.is_empty() {
                    let stash = std::mem::take(&mut pending_uploads);
                    let mut requeue_failed = false;
                    for res in stash {
                        if requeue_failed {
                            // Socket already proven down this pass — keep the rest.
                            pending_uploads.push(res);
                            continue;
                        }
                        match res {
                            BlobResult::Upload { outcome, ev, hist_kind, hist_preview, hist_mime, hist_size, hist_name, emit_json, err_via_error, echo_img_sha } => {
                                dlog!("send", "re-advertising pending upload seq={} blob_id={} on new connection", ev.seq, ev.blob_id);
                                if ws::send(&mut sock, protocol::T_CLIP, &ev).await.is_err() {
                                    dlog!("send", "re-advertise seq={}: ws::send FAILED — control channel down again", ev.seq);
                                    requeue_failed = true;
                                    pending_uploads.push(BlobResult::Upload {
                                        outcome, ev, hist_kind, hist_preview, hist_mime, hist_size, hist_name, emit_json, err_via_error, echo_img_sha,
                                    });
                                } else {
                                    // idx 9: the clip finally went out — record the echo hash now.
                                    if let Some(sha) = echo_img_sha { remember(&mut recent_img, sha); }
                                    add_history(&hist, &ev.ts, hist_kind, "me", "out", &hist_preview, &hist_mime, hist_size, &ev.blob_id, &hist_name);
                                    emit.clip(emit_json);
                                }
                            }
                            // Only Upload results are ever stashed (downloads/serves
                            // are not re-advertised); ignore anything else defensively.
                            _ => {}
                        }
                    }
                    if requeue_failed {
                        // Don't even enter the select! on a socket we already know
                        // is dead; fall through to the reconnect/backoff path.
                        emit.error("재연결 직후 제어 채널이 다시 끊겨 보류 중인 클립을 다음 연결에서 다시 시도합니다".to_string());
                        set_connected(&*emit, &status, false);
                        attempt = attempt.saturating_add(1);
                        let delay = backoff_delay(attempt);
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = reconnect.notified() => {}
                        }
                        continue;
                    }
                }
                let why: String = 'inner: loop {
                    tokio::select! {
                        _ = ping_at.tick() => {
                            dlog!("ws", "ping: sending keepalive");
                            if let Err(e) = ws::ping(&mut sock).await {
                                break 'inner format!("핑(keepalive) 전송 실패: {e}");
                            }
                        }
                        _ = watchdog.tick() => {
                            let idle = last_recv.elapsed();
                            dlog!("ws", "watchdog: tick · idle={}s (limit {}s)", idle.as_secs(), WS_WATCHDOG.as_secs());
                            if idle > WS_WATCHDOG {
                                break 'inner format!(
                                    "{}초 동안 서버 프레임 없음 — watchdog({}s) 초과로 연결 끊김 판단",
                                    idle.as_secs(), WS_WATCHDOG.as_secs()
                                );
                            }
                        }
                        _ = reconnect.notified() => break 'inner "사용자가 재연결을 요청함".to_string(),
                        cmd = rx.recv() => match cmd {
                            None => { dlog!("engine", "run() return: command channel closed (host shutdown)"); return; }
                            Some(Cmd::LocalText { text, html }) => {
                                let sha = sha_hex(text.as_bytes());
                                if seen(&recent_text, &sha) {
                                    dlog!("send", "LocalText SKIPPED (echo/dedup) sha={}", &sha[..16.min(sha.len())]);
                                    continue;
                                }
                                // idx 9: do NOT remember pre-send — send_text_clip
                                // records the hash only after a successful ws::send,
                                // so a clip that never reached the wire (oversize,
                                // E2E/encode error) won't suppress an identical re-copy.
                                if exclude_flag.load(Ordering::Relaxed) {
                                    if let Some(reason) = privacy::classify(&text, &custom_res) {
                                        // Privacy filter: never sync; record locally + auto-purge.
                                        let id = hist.lock().unwrap_or_else(|e| e.into_inner())
                                            .add(&protocol::now_ts(), "text", "me", "out", &text, "text/plain", text.len() as i64, "", "")
                                            .unwrap_or(-1);
                                        schedule_purge(hist.clone(), id, cfg.sensitive_ttl_secs);
                                        dlog!("send", "text SKIPPED by privacy filter (reason={}) — recorded locally, not synced", reason.label());
                                        emit.clip(serde_json::json!({"direction":"out","kind":"text","text":text,"sensitive":reason.label()}));
                                        continue;
                                    }
                                }
                                if !send_text_clip(&mut sock, &mut seq, &text, html.as_deref(), &key, current_targets(&state.targets), max_msg, &*emit, &hist, &mut recent_text).await { break 'inner "텍스트 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::SendText(t)) => {
                                // idx 9: remember only on a successful send (inside
                                // send_text_clip), so a failed manual send no longer
                                // poisons the dedup ring against a later re-copy.
                                if !send_text_clip(&mut sock, &mut seq, &t, None, &key, current_targets(&state.targets), max_msg, &*emit, &hist, &mut recent_text).await { break 'inner "텍스트 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::LocalImage(img)) => {
                                let sha = sha_hex(&img.rgba);
                                if seen(&recent_img, &sha) {
                                    dlog!("send", "LocalImage SKIPPED (echo/dedup) sha={}", &sha[..16.min(sha.len())]);
                                    continue;
                                }
                                // idx 9: do NOT remember pre-send. The sha is carried
                                // through the BlobResult::Upload and recorded only after
                                // a successful upload+advertise, so a re-copy of an image
                                // whose upload failed isn't silently swallowed.
                                // Upload runs OFF the loop; the T_CLIP frame is emitted
                                // from the blob_rx arm when the upload completes.
                                spawn_image_clip(&mut seq, &img, &key, current_targets(&state.targets), &*emit, &http, &cfg, &state.data_dir, &blob_tx, sha);
                            }
                            Some(Cmd::SendFile(p)) => {
                                if !send_file_clip(&mut sock, &mut seq, &p, &key, current_targets(&state.targets), threshold, &mut on_demand, &mut on_demand_order, &*emit, &hist, &http, &cfg, &blob_tx).await { break 'inner "파일 전송 실패 — 제어 채널 끊김".to_string(); }
                            }
                            Some(Cmd::LocalFiles(files)) => {
                                for p in files {
                                    // Skip a file we just placed on the clipboard from an inbound clip (echo).
                                    if seen(&recent_files, &p) {
                                        dlog!("send", "LocalFiles entry SKIPPED (echo) path={p}");
                                        continue;
                                    }
                                    if !send_file_clip(&mut sock, &mut seq, &p, &key, current_targets(&state.targets), threshold, &mut on_demand, &mut on_demand_order, &*emit, &hist, &http, &cfg, &blob_tx).await { break 'inner "파일 전송 실패 — 제어 채널 끊김".to_string(); }
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
                                    dlog!("ws", "recv: pong/keepalive");
                                } else if t == protocol::T_TOKEN_ROTATE {
                                    dlog!("ws", "recv: token_rotate");
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
                                    dlog!("ws", "recv: frame t={t}");
                                    handle_frame(t, d, &key, &pull, &http, &cfg, &*emit, &state, &hist, &roster, &mut recent_text, &mut on_demand, &blob_tx).await;
                                }
                            }
                            Ok(None) => break 'inner "서버가 연결을 종료함".to_string(),
                            Err(e) => break 'inner format!("수신 오류: {e}"),
                        },
                        // Results from OFF-LOOP blob workers (idx 1). Polling this arm
                        // alongside ws::recv is exactly what keeps the read half live
                        // (pongs flow) while a transfer is in flight.
                        Some(res) = blob_rx.recv() => match res {
                            BlobResult::Upload { outcome, ev, hist_kind, hist_preview, hist_mime, hist_size, hist_name, emit_json, err_via_error, echo_img_sha } => {
                                match outcome {
                                    Ok(()) => {
                                        dlog!("send", "upload done seq={} blob_id={} → control channel", ev.seq, ev.blob_id);
                                        if ws::send(&mut sock, protocol::T_CLIP, &ev).await.is_err() {
                                            // idx 0 fix: the upload completed (bytes are on the
                                            // server) but the control channel just died before we
                                            // could advertise. Do NOT drop the clip — stash it so
                                            // the next connection re-advertises it (history/emit
                                            // are recorded only after a successful advertise).
                                            dlog!("send", "upload done seq={}: ws::send FAILED — stashing for re-advertise on reconnect", ev.seq);
                                            pending_uploads.push(BlobResult::Upload {
                                                outcome: Ok(()),
                                                ev, hist_kind, hist_preview, hist_mime, hist_size, hist_name, emit_json, err_via_error, echo_img_sha,
                                            });
                                            break 'inner "클립 전송 실패 — 제어 채널 끊김(보류 후 재연결 시 재전송)".to_string();
                                        }
                                        // idx 9: record the echo-dedup hash only now that the
                                        // clip actually went on the wire (images only).
                                        if let Some(sha) = echo_img_sha { remember(&mut recent_img, sha); }
                                        add_history(&hist, &ev.ts, hist_kind, "me", "out", &hist_preview, &hist_mime, hist_size, &ev.blob_id, &hist_name);
                                        emit.clip(emit_json);
                                    }
                                    Err((_class, msg)) => {
                                        // Blob-channel failure (not the control channel) — surface and drop.
                                        if err_via_error { emit.error(msg); } else { emit.notify("CopySync", &msg); }
                                    }
                                }
                            }
                            BlobResult::Serve { id, ok } => {
                                if ok {
                                    // The server pulls each advertised on-demand blob exactly once.
                                    // Drop the served hold so the map (full file-sized ciphertext for
                                    // E2E) can't grow without bound (the leak fix). Only on success,
                                    // so a failed upload can still be retried on a later request.
                                    if on_demand.remove(&id).is_some() {
                                        on_demand_order.retain(|k| k != &id);
                                    }
                                }
                            }
                            BlobResult::Download { outcome, ev } => match outcome {
                                // idx defense: a malformed/adversarial inbound blob
                                // (bad image decode, unexpected bytes) must not unwind
                                // the whole engine actor. catch_unwind around the sync
                                // apply so a panic is logged + the frame skipped.
                                Ok(data) => {
                                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        apply_blob(ev, data, &key, &*emit, &state, &hist, &downloads, &mut recent_img, &mut recent_files)
                                    }));
                                    if r.is_err() {
                                        dlog!("recv", "apply_blob PANICKED — inbound blob skipped (engine kept alive)");
                                        emit.error("받은 파일 처리 중 내부 오류 — 이 항목만 건너뜁니다".to_string());
                                    }
                                }
                                Err((_class, msg)) => emit.notify("CopySync", &msg),
                            },
                        }
                    }
                };
                dlog!("ws", "disconnect: {why}");
                emit.error(format!("연결 종료 — {why} · 자동 재연결 시도"));
                set_connected(&*emit, &status, false);
            }
            Err(e) => {
                eprintln!("connect failed: {e}");
                dlog!("ws", "connect: FAILED — {e}");
                emit.error(format!("연결 실패 — 재시도 중: {e}"));
            }
        }
        // Exponential backoff with jitter (1→2→4…→30s), surfaced to the UI; a
        // manual "재연결" (reconnect.notified) skips the wait.
        attempt = attempt.saturating_add(1);
        let delay = backoff_delay(attempt);
        dlog!("ws", "reconnect: backoff attempt #{attempt}, waiting {}ms", delay.as_millis());
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

#[allow(clippy::too_many_arguments)]
async fn send_text_clip(
    sock: &mut ws::Ws,
    seq: &mut u64,
    text: &str,
    html: Option<&str>,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    max_msg: i64,
    emit: &dyn Emitter,
    hist: &Arc<Mutex<History>>,
    recent_text: &mut Dedup,
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
        Err(e) => {
            dlog!("send", "text clip build FAILED (E2E seal?) — not sent: {e}");
            emit.error(format!("텍스트 클립 생성 실패(E2E 암호화?) — 전송 안 함: {e}"));
            return true;
        }
    };
    // idx 17: refuse a frame larger than the server's advertised read limit. With
    // E2E on, base64 of the ciphertext expands ~33%, so an oversized clip is easy
    // to hit; sending it anyway would exceed the server read limit and tear the
    // control connection down, silently losing this clip. Refuse + surface an error
    // (and do NOT record it in history as a successful send) instead. The frame is
    // encoded once here purely to measure; ws::send re-encodes identically.
    let encoded = match crate::encode(protocol::T_CLIP, &ev) {
        Ok(b) => b,
        Err(e) => {
            dlog!("send", "text seq={}: frame encode FAILED — not sent: {e}", *seq);
            emit.error(format!("텍스트 클립 인코딩 실패 — 전송 안 함: {e}"));
            return true;
        }
    };
    if max_msg > 0 && encoded.len() as i64 > max_msg {
        dlog!(
            "send",
            "text seq={}: frame {}B exceeds server limit {}B — refusing (would drop connection)",
            *seq, encoded.len(), max_msg
        );
        emit.error(format!(
            "텍스트가 서버 전송 한도({}KB)를 초과하여 동기화하지 않았습니다 ({}KB)",
            (max_msg / 1024).max(1),
            (encoded.len() as i64 / 1024).max(1)
        ));
        return true; // not a control-channel failure; just skip this clip
    }
    dlog!(
        "send",
        "text seq={} size={}B frame={}B hash={} html={} → control channel",
        *seq, text.len(), encoded.len(), &sha_hex(text.as_bytes())[..16], html.is_some()
    );
    if ws::send(sock, protocol::T_CLIP, &ev).await.is_err() {
        dlog!("send", "text seq={}: ws::send FAILED — control channel down", *seq);
        return false;
    }
    // idx 9: remember the hash ONLY after the clip actually went on the wire. The
    // early `return true` failure branches above (E2E/build, encode, max_msg
    // refusal) must NOT poison the echo ring, so an immediate re-copy of the same
    // text that previously failed to send is not silently suppressed.
    remember(recent_text, sha_hex(text.as_bytes()));
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

/// Prepare an image clip and spawn its blob upload OFF the connection loop. The
/// full `ClipEvent` is built here (its `blob_id`/`sha256` are pure functions of the
/// payload, so no network is needed to know them); the actual `put_blob` runs in a
/// detached task and reports back via `blob_tx`, where the loop emits the `T_CLIP`
/// frame. Returns immediately so the loop keeps polling `ws::recv` (pongs flow).
#[allow(clippy::too_many_arguments)]
fn spawn_image_clip(
    seq: &mut u64,
    img: &Image,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    emit: &dyn Emitter,
    http: &reqwest::Client,
    cfg: &Config,
    data_dir: &Path,
    blob_tx: &UnboundedSender<BlobResult>,
    echo_sha: String, // idx 9: remembered for echo-dedup only on a successful upload
) {
    let png = match clipboard::encode_png(img) {
        Ok(p) => p,
        Err(e) => {
            dlog!("send", "image PNG encode FAILED — not sent: {e}");
            emit.error(format!("이미지 PNG 인코딩 실패 — 전송 안 함: {e}"));
            return;
        }
    };
    let payload = match key {
        Some((k, _)) => match e2e::seal(k, &png) {
            Ok(c) => c,
            Err(e) => {
                dlog!("send", "image E2E seal FAILED — not sent: {e}");
                emit.error(format!("이미지 E2E 암호화 실패 — 전송 안 함: {e}"));
                return;
            }
        },
        None => png.clone(),
    };
    // blob_id and sha256 are pure functions of the bytes we're about to upload, so
    // we can build the whole ClipEvent now and upload asynchronously.
    let bid = blob::blob_id(&payload);
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
    let preview = cache_outbound_image(data_dir, &png).unwrap_or_else(|| "(클립보드 이미지)".into());
    let emit_json = serde_json::json!({"direction":"out","kind":"image","name":"clipboard.png","size":png.len()});
    dlog!("blob", "image upload spawn: {}B payload (e2e={}) seq={}", payload.len(), key.is_some(), *seq);
    let (client, base, token) = (http.clone(), cfg.server_url.clone(), cfg.token.clone());
    let tx = blob_tx.clone();
    let png_len = png.len() as i64;
    tokio::spawn(async move {
        let outcome = match blob::put_blob(&client, &base, &token, payload).await {
            Ok(b) => { dlog!("blob", "image upload OK: blob_id={b}"); Ok(()) }
            Err(e) => {
                let class = classify_blob_err(&e);
                dlog!("blob", "image upload FAILED ({class}): {e}");
                Err((class, format!("이미지 blob 업로드 실패({class}) — 전송 안 함: {e}")))
            }
        };
        let _ = tx.send(BlobResult::Upload {
            outcome,
            ev,
            hist_kind: "image",
            hist_preview: preview,
            hist_mime: "image/png".to_string(),
            hist_size: png_len,
            hist_name: "clipboard.png".to_string(),
            emit_json,
            err_via_error: true,
            echo_img_sha: Some(echo_sha),
        });
    });
}

/// Prepare a file clip. The on-demand path advertises immediately on the loop
/// (a small control frame, no blob I/O) and holds the bytes for a later
/// `blob_request`. The eager path computes the blob id locally, builds the full
/// `ClipEvent`, and spawns the `put_blob` OFF the loop (reporting back via
/// `blob_tx`, where the loop emits the `T_CLIP`). Returns `false` only on a real
/// control-channel send failure (the on-demand advertisement), so the caller can
/// trigger a reconnect; the eager path never blocks the loop on the network.
#[allow(clippy::too_many_arguments)]
async fn send_file_clip(
    sock: &mut ws::Ws,
    seq: &mut u64,
    path: &str,
    key: &Option<(Vec<u8>, String)>,
    targets: Targets,
    threshold: i64,
    on_demand: &mut HashMap<String, Hold>,
    on_demand_order: &mut VecDeque<String>,
    emit: &dyn Emitter,
    hist: &Arc<Mutex<History>>,
    http: &reqwest::Client,
    cfg: &Config,
    blob_tx: &UnboundedSender<BlobResult>,
) -> bool {
    let p = Path::new(path);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) => {
            dlog!("send", "file metadata FAILED path={path}: {e}");
            emit.notify("CopySync", &format!("파일을 열 수 없습니다: {e}"));
            return true;
        }
    };
    let size = meta.len() as i64;
    let name = file_name(path);
    let mime = mime_of(path);
    let kind = if mime.starts_with("image/") { "image" } else { "file" };
    let on_demand_mode = threshold > 0 && size > threshold;
    dlog!(
        "send",
        "file path={path} name={name} mime={mime} size={size}B on_demand={on_demand_mode} e2e={}",
        key.is_some()
    );

    *seq += 1;
    if on_demand_mode {
        // On-demand: advertise now, upload when the server asks. No blob I/O here,
        // so this small advertisement frame is fine to send on the loop.
        let (bid, sha) = match key {
            Some((k, _)) => {
                let data = match std::fs::read(p) {
                    Ok(d) => d,
                    Err(e) => { dlog!("send", "on-demand file read FAILED path={path}: {e}"); emit.notify("CopySync", &format!("읽기 실패: {e}")); return true; }
                };
                let ct = match e2e::seal(k, &data) { Ok(c) => c, Err(e) => { dlog!("send", "on-demand file E2E seal FAILED: {e}"); emit.error(format!("파일 E2E 암호화 실패 — 전송 안 함: {e}")); return true; } };
                let sha = sha_hex(&ct);
                let bid = format!("sha256:{sha}");
                hold_on_demand(on_demand, on_demand_order, bid.clone(), Hold::Sealed(ct));
                (bid, sha)
            }
            None => {
                let sha = match file_sha_hex(p) { Ok(s) => s, Err(e) => { dlog!("send", "on-demand file hash FAILED path={path}: {e}"); emit.notify("CopySync", &format!("읽기 실패: {e}")); return true; } };
                let bid = format!("sha256:{sha}");
                hold_on_demand(on_demand, on_demand_order, bid.clone(), Hold::Plain(p.to_path_buf()));
                (bid, sha)
            }
        };
        dlog!("send", "file advertised on-demand: blob held for server request");
        let ev = ClipEvent {
            id: protocol::new_id(), seq: *seq, ts: protocol::now_ts(),
            mime: vec![mime.clone()], name: name.clone(), blob_id: bid, size,
            sha256: sha, on_demand: true, targets,
            enc: key.as_ref().map(|(_, kid)| EncMeta { alg: e2e::ALG.into(), key_id: kid.clone(), nonce: String::new() }),
            ..Default::default()
        };
        dlog!("send", "file seq={} name={} blob_id={} → control channel", *seq, name, ev.blob_id);
        if ws::send(sock, protocol::T_CLIP, &ev).await.is_err() {
            dlog!("send", "file seq={}: ws::send FAILED — control channel down", *seq);
            return false;
        }
        add_history(hist, &ev.ts, kind, "me", "out", &name, &mime, size, &ev.blob_id, &name);
        emit.clip(serde_json::json!({"direction":"out","kind":kind,"name":name,"size":size,"onDemand":ev.on_demand}));
        true
    } else {
        // Eager: read + (encrypt), build the event with a locally-computed blob id,
        // then upload OFF the loop and emit the T_CLIP frame when it completes.
        let data = match std::fs::read(p) {
            Ok(d) => d,
            Err(e) => { dlog!("send", "eager file read FAILED path={path}: {e}"); emit.notify("CopySync", &format!("읽기 실패: {e}")); return true; }
        };
        let payload = match key {
            Some((k, _)) => match e2e::seal(k, &data) { Ok(c) => c, Err(e) => { dlog!("send", "eager file E2E seal FAILED: {e}"); emit.error(format!("파일 E2E 암호화 실패 — 전송 안 함: {e}")); return true; } },
            None => data,
        };
        let bid = blob::blob_id(&payload);
        let ev = ClipEvent {
            id: protocol::new_id(), seq: *seq, ts: protocol::now_ts(),
            mime: vec![mime.clone()], name: name.clone(), blob_id: bid, size,
            sha256: sha_hex(&payload), targets,
            enc: key.as_ref().map(|(_, kid)| EncMeta { alg: e2e::ALG.into(), key_id: kid.clone(), nonce: String::new() }),
            ..Default::default()
        };
        let emit_json = serde_json::json!({"direction":"out","kind":kind,"name":name,"size":size,"onDemand":false});
        dlog!("blob", "file upload spawn: {}B payload (e2e={}) seq={}", payload.len(), key.is_some(), *seq);
        let (client, base, token) = (http.clone(), cfg.server_url.clone(), cfg.token.clone());
        let tx = blob_tx.clone();
        let (hist_name, hist_mime) = (name.clone(), mime.clone());
        tokio::spawn(async move {
            let outcome = match blob::put_blob(&client, &base, &token, payload).await {
                Ok(b) => { dlog!("blob", "file upload OK: blob_id={b}"); Ok(()) }
                Err(e) => {
                    let class = classify_blob_err(&e);
                    dlog!("blob", "file upload FAILED ({class}): {e}");
                    Err((class, format!("업로드 실패({class}): {e}")))
                }
            };
            let _ = tx.send(BlobResult::Upload {
                outcome,
                ev,
                hist_kind: kind,
                hist_preview: hist_name.clone(),
                hist_mime,
                hist_size: size,
                hist_name,
                emit_json,
                err_via_error: false, // file upload errors went to emit.notify
                echo_img_sha: None,   // files don't use the image echo ring
            });
        });
        true
    }
}

/// Insert an on-demand hold, keeping the map bounded to a small insertion-ordered
/// ring so a blob that is advertised but never fetched can't accumulate without
/// bound (the leak fix). Re-advertising the same content is a no-op resize since
/// it reuses the same key. Holds are also dropped right after they're served.
fn hold_on_demand(
    map: &mut HashMap<String, Hold>,
    order: &mut VecDeque<String>,
    key: String,
    hold: Hold,
) {
    // idx 10: keep the ring generous so a realistic multi-file burst doesn't evict
    // a still-unfetched hold before the recipient's lazy blob_request arrives. Bound
    // it (Sealed holds keep full ciphertext in RAM) but well above a typical batch.
    const MAX_HOLDS: usize = 64;
    if map.insert(key.clone(), hold).is_none() {
        order.push_back(key);
        while order.len() > MAX_HOLDS {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }
}

/// Classify a blob-upload/-download failure as transient (worth retrying — a
/// network blip, server 5xx, timeout, rate-limit) vs permanent (a 4xx the client
/// caused — too large, bad request, unauthorized). Best-effort string sniffing of
/// the `anyhow` error from `blob::put_blob`/`get_blob`, which embeds the HTTP
/// status (e.g. "blob PUT 413: …") or a reqwest transport message.
fn classify_blob_err(e: &anyhow::Error) -> &'static str {
    let s = e.to_string().to_ascii_lowercase();
    // 4xx the client caused → permanent (don't hammer the server).
    for code in ["400", "401", "403", "404", "413", "422"] {
        if s.contains(code) {
            return "permanent";
        }
    }
    // Explicitly-retryable HTTP statuses + transport-level failures → transient.
    if s.contains("408")
        || s.contains("429")
        || s.contains("500")
        || s.contains("502")
        || s.contains("503")
        || s.contains("504")
        || s.contains("timeout")
        || s.contains("timed out")
        || s.contains("connection")
        || s.contains("connect")
        || s.contains("reset")
        || s.contains("refused")
        || s.contains("dns")
        || s.contains("tls")
        || s.contains("eof")
    {
        return "transient";
    }
    // Unknown → assume transient (safer to allow a retry than to drop silently).
    "transient"
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

/// Handle one inbound control frame. Blob-bearing clips spawn their `get_blob`
/// download OFF the loop (so `ws::recv` keeps being polled and pongs flow during
/// the server's up-to-60s on-demand long-poll); a `blob_request` likewise spawns
/// its `put_blob` upload off the loop. Text clips (no network) are applied inline.
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
    roster: &Arc<Mutex<Vec<RosterDevice>>>,
    recent_text: &mut Dedup,
    on_demand: &mut HashMap<String, Hold>,
    blob_tx: &UnboundedSender<BlobResult>,
) {
    match t.as_str() {
        protocol::T_CLIP => {
            match serde_json::from_value::<ClipEvent>(d) {
                Ok(ev) => {
                    dlog!("recv", "clip: kind={} blob={} on_demand={} origin={} size={}", ev.kind(), ev.is_blob(), ev.on_demand, ev.origin_device, ev.size);
                    if ev.is_blob() {
                        // Download off the loop; the loop applies the bytes when the
                        // BlobResult::Download arrives (see apply_blob).
                        dlog!("recv", "blob pull spawn: blob_id={} ({}B advertised)", ev.blob_id, ev.size);
                        let (client, base, token) = (pull.clone(), cfg.server_url.clone(), cfg.token.clone());
                        let tx = blob_tx.clone();
                        tokio::spawn(async move {
                            let outcome = match blob::get_blob(&client, &base, &token, &ev.blob_id).await {
                                Ok(d) => { dlog!("recv", "blob pull OK: {}B", d.len()); Ok(d) }
                                Err(e) => {
                                    let class = classify_blob_err(&e);
                                    dlog!("recv", "blob pull FAILED ({class}) blob_id={}: {e}", ev.blob_id);
                                    Err((class, format!("파일 받기 실패({class}): {e}")))
                                }
                            };
                            let _ = tx.send(BlobResult::Download { outcome, ev });
                        });
                    } else {
                        // idx defense: apply the inbound text/HTML clip under
                        // catch_unwind so a malformed crypto/base64/clipboard payload
                        // logs + is skipped instead of unwinding the engine actor.
                        // (handle_incoming_text is fully synchronous — no await — so it
                        // is safe to catch_unwind around.)
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            handle_incoming_text(ev, key, cfg, emit, state, hist, recent_text)
                        }));
                        if r.is_err() {
                            dlog!("recv", "handle_incoming_text PANICKED — inbound clip skipped (engine kept alive)");
                            emit.error("받은 클립 처리 중 내부 오류 — 이 항목만 건너뜁니다".to_string());
                        }
                    }
                }
                Err(e) => { dlog!("recv", "clip decode FAILED: {e}"); emit.error(format!("받은 클립 디코딩 실패: {e}")); }
            }
        }
        protocol::T_BLOB_REQUEST => {
            if let Ok(br) = serde_json::from_value::<BlobRequest>(d) {
                if let Some(hold) = on_demand.get(&br.id) {
                    dlog!("blob", "blob_request: serving held blob id={}", br.id);
                    // Clone/read the bytes on the loop (cheap for Sealed; a local
                    // disk read for Plain), then upload OFF the loop. The hold is
                    // dropped from on_demand by the loop on a successful Serve.
                    let bytes = match hold {
                        Hold::Sealed(b) => b.clone(),
                        Hold::Plain(p) => match std::fs::read(p) {
                            Ok(b) => b,
                            Err(e) => { dlog!("blob", "blob_request read FAILED id={}: {e}", br.id); emit.error(format!("요청받은 파일 읽기 실패: {e}")); return; }
                        },
                    };
                    let (client, base, token) = (http.clone(), cfg.server_url.clone(), cfg.token.clone());
                    let tx = blob_tx.clone();
                    let id = br.id.clone();
                    tokio::spawn(async move {
                        let ok = match blob::put_blob(&client, &base, &token, bytes).await {
                            Ok(b) => { dlog!("blob", "blob_request upload OK id={id} blob_id={b}"); true }
                            Err(e) => { let class = classify_blob_err(&e); dlog!("blob", "blob_request upload FAILED ({class}) id={id}: {e}"); false }
                        };
                        let _ = tx.send(BlobResult::Serve { id, ok });
                    });
                } else {
                    // idx 10: the server asked for an on-demand blob we no longer
                    // hold (evicted from the bounded ring before the recipient
                    // fetched it, or already served). Surface it instead of only
                    // logging, so a dropped large/E2E clip is observable rather than
                    // silently lost (the recipient's pull then 504s).
                    dlog!("blob", "blob_request: no held blob for id={} (already gone?)", br.id);
                    emit.error(format!(
                        "요청받은 파일을 더 이상 보관하고 있지 않습니다 (대기 중 제거됨) — 다시 복사해 주세요 (id={})",
                        br.id
                    ));
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

/// Apply an already-downloaded blob clip's bytes (decrypt, persist, push to the
/// clipboard, record history). Runs on the connection loop after the off-loop
/// `get_blob` completes — the tail of the old `handle_incoming` blob branch.
#[allow(clippy::too_many_arguments)]
fn apply_blob(
    ev: ClipEvent,
    data: Vec<u8>,
    key: &Option<(Vec<u8>, String)>,
    emit: &dyn Emitter,
    state: &EngineState,
    hist: &Arc<Mutex<History>>,
    downloads: &Path,
    recent_img: &mut Dedup,
    recent_files: &mut Dedup,
) {
    {
        let plain = match (&ev.enc, key) {
            (Some(meta), Some((k, kid))) => {
                // idx 12 fix: surface a precise "different passphrase" diagnostic
                // when the sender's key_id is known and doesn't match ours, instead
                // of the generic GCM-tag-failure "can't decrypt" message (parity
                // with Android's SyncService).
                if !meta.key_id.is_empty() && &meta.key_id != kid {
                    dlog!("recv", "no matching E2E key for blob: sender kid={} local kid={} blob_id={}", meta.key_id, kid, ev.blob_id);
                    emit.notify("CopySync", &format!("다른 암호로 암호화된 파일을 받았습니다 (keyId={})", meta.key_id));
                    return;
                }
                match e2e::open(k, &data) {
                    Ok(p) => p,
                    Err(_) => {
                        dlog!("recv", "blob decrypt FAILED (wrong passphrase / ciphertext) blob_id={}", ev.blob_id);
                        emit.notify("CopySync", "받은 파일을 복호화할 수 없습니다 (암호문?)");
                        return;
                    }
                }
            }
            (Some(_), None) => {
                dlog!("recv", "blob is encrypted but no E2E key set — cannot apply");
                emit.notify("CopySync", "암호화된 파일을 받았지만 암호문이 설정되지 않았습니다");
                return;
            }
            (None, _) => data,
        };
        let _ = std::fs::create_dir_all(downloads);
        // idx 0 (path traversal / arbitrary file write): `ev.name` is an untrusted,
        // peer/server-controlled string. `Path::join` lets an absolute path replace
        // the base and `..` components traverse upward, so writing to
        // `downloads.join(ev.name)` verbatim is an arbitrary-file-write primitive
        // (e.g. "../../.bashrc" or "/home/u/.config/autostart/x.desktop"). Reduce to
        // a pure basename via file_name() (same sanitization the OUTBOUND side does,
        // see file_name() above), rejecting empty / "." / ".." and falling back to
        // the blob id. NOTE: a plain path.starts_with(downloads) guard is NOT enough
        // because Rust's Path does not normalize "..".
        let blob_fallback = || ev.blob_id.trim_start_matches("sha256:").to_string();
        let name = std::path::Path::new(&ev.name)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty() && *s != "." && *s != "..")
            .map(|s| s.to_string())
            .unwrap_or_else(blob_fallback);
        let path = downloads.join(&name);
        let _ = std::fs::write(&path, &plain);
        let is_image = ev.mime.first().map(|m| m.starts_with("image/")).unwrap_or(false);
        if is_image {
            match clipboard::decode_image(&plain) {
                Ok(img) => {
                    remember(recent_img, sha_hex(&img.rgba));
                    if let Err(e) = clipboard::set_image(&img) {
                        dlog!("recv", "apply image to clipboard FAILED (RDP contention?): {e}");
                        emit.error(format!("받은 이미지 클립보드 적용 실패(RDP 경합?): {e}"));
                    } else {
                        dlog!("recv", "applied image to clipboard: {name} ({}B)", plain.len());
                    }
                }
                Err(e) => { dlog!("recv", "received image decode FAILED: {e}"); emit.error(format!("받은 이미지 디코딩 실패: {e}")); }
            }
            emit.notify(&clip_origin_name(state, &ev.origin_device), &format!("🖼 {name} · {}", human(plain.len())));
        } else {
            // Put the received file on the clipboard so Ctrl+V pastes it. This
            // applies to on-demand transfers too: by the time apply_blob runs the
            // bytes are FULLY downloaded — on-demand only changed how the transfer
            // was brokered, and gating this on `!ev.on_demand` made every large
            // (> threshold) file from Android/desktop land silently in the
            // downloads folder with paste doing nothing (user-reported).
            let p = path.to_string_lossy().to_string();
            remember(recent_files, p.clone());
            if let Err(e) = clipboard::set_files(&[p]) {
                dlog!("recv", "apply file to clipboard FAILED (RDP contention?): {e}");
                emit.error(format!("받은 파일 클립보드 적용 실패(RDP 경합?): {e}"));
            } else {
                dlog!("recv", "applied file to clipboard: {}", path.to_string_lossy());
            }
            emit.notify(&clip_origin_name(state, &ev.origin_device), &format!("📎 {name} · {}", human(plain.len())));
        }
        add_history(hist, &ev.ts, ev.kind(), &ev.origin_device, "in", &path.to_string_lossy(),
            ev.mime.first().map(|s| s.as_str()).unwrap_or(""), plain.len() as i64, &ev.blob_id, &name);
        emit.clip(serde_json::json!({"direction":"in","kind":ev.kind(),"name":name,"size":plain.len(),"path":path.to_string_lossy()}));
    }
}

/// Apply an inbound text/HTML clip (no network involved) — the tail of the old
/// `handle_incoming` text branch. Runs inline on the connection loop.
#[allow(clippy::too_many_arguments)]
fn handle_incoming_text(
    ev: ClipEvent,
    key: &Option<(Vec<u8>, String)>,
    cfg: &Config,
    emit: &dyn Emitter,
    state: &EngineState,
    hist: &Arc<Mutex<History>>,
    recent_text: &mut Dedup,
) {
    let text = match (&ev.enc, key) {
        (Some(meta), Some((k, kid))) => {
            // idx 12 fix: if the sender's key_id is known and differs from ours,
            // this clip was encrypted with a DIFFERENT passphrase/group key.
            // GCM would just fail the tag and we'd show a generic "can't decrypt"
            // — instead give the precise, actionable diagnostic Android gives
            // (SyncService.kt) so a mismatched-passphrase misconfig is obvious.
            if !meta.key_id.is_empty() && &meta.key_id != kid {
                dlog!("recv", "no matching E2E key: sender kid={} local kid={}", meta.key_id, kid);
                emit.notify("CopySync", &format!("다른 암호로 암호화된 클립을 받았습니다 (keyId={})", meta.key_id));
                return;
            }
            use base64::{engine::general_purpose::STANDARD, Engine};
            let raw = match STANDARD.decode(&ev.inline_text) {
                Ok(r) => r,
                Err(e) => { dlog!("recv", "text base64 decode FAILED: {e}"); emit.error(format!("받은 텍스트 base64 디코딩 실패: {e}")); return; }
            };
            match e2e::open(k, &raw) {
                Ok(p) => String::from_utf8_lossy(&p).to_string(),
                Err(_) => {
                    dlog!("recv", "text decrypt FAILED (wrong passphrase / ciphertext)");
                    emit.notify("CopySync", "받은 텍스트를 복호화할 수 없습니다 (암호문?)");
                    return;
                }
            }
        }
        (Some(_), None) => {
            dlog!("recv", "text is encrypted but no E2E key set — cannot apply");
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
        dlog!("recv", "apply text to clipboard FAILED (RDP contention?): {e}");
        emit.error(format!("받은 텍스트 클립보드 적용 실패(RDP 경합?): {e}"));
    } else {
        dlog!("recv", "applied text to clipboard: {} chars (html={}, mark_sensitive={})", text.chars().count(), html.is_some(), mark);
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
