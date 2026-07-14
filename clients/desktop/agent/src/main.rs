//! copysync-agent — the headless background sync daemon.
//!
//! M2 wires the real sync engine (`copysync_core::engine`) behind a per-user
//! local-socket IPC server that speaks the [`copysync_ipc`] vocabulary. The agent
//! owns the engine's shared state, drives the actor (`engine::run`) on a tokio
//! task, runs the OS-clipboard watcher (`engine::clipboard_loop`) on a std thread,
//! and fans engine callbacks out to subscribed GUI clients as IPC events.
//!
//!   copysync-agent          # serve (default)
//!   copysync-agent serve    # serve
//!   copysync-agent ping     # connect, GetStatus, print the reply (self-test)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use copysync_core::engine::{self, Cmd, EngineState, Emitter, RosterDevice as CoreRoster, Status as CoreStatus};
use copysync_core::history::History;
use copysync_core::protocol::Targets;
use copysync_core::{discovery, e2e, pairing, Config};

use copysync_ipc::{
    socket_label, ClipInfo, Event, FoundServer, HistRow, Outbound, Request, Response,
    RosterDevice as IpcRoster, Status as IpcStatus,
};

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Stream as TokioStream;
use interprocess::local_socket::{
    prelude::*, GenericNamespaced, ListenerOptions, Stream as SyncStream,
};

// ----------------------------------------------------------------- debug log
//
// The agent is headless — no window, and (when launched at boot or auto-spawned
// by the GUI) often no console either. So a file is the ONLY diagnostic. Detailed
// logging is gated by the `COPYSYNC_DEBUG=1` environment variable and written to
// `dirs::config_dir()/copysync/logs/agent.log`. We log IPC connect/disconnect,
// engine start/stop, and every error path with context.

use std::io::Write as _;
use std::sync::OnceLock;

/// Whether detailed debug logging is on, read once from `COPYSYNC_DEBUG`. Treated
/// as enabled when the var is exactly "1" or "true" (case-insensitive).
fn debug_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("COPYSYNC_DEBUG") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    })
}

/// The agent debug-log path: `dirs::config_dir()/copysync/logs/agent.log`, with a
/// temp-dir fallback so we always have somewhere to write.
fn agent_log_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    base.join("copysync").join("logs").join("agent.log")
}

/// A coarse wall-clock "HH:MM:SS" stamp (no date crate). Enough to order events
/// within a session.
fn debug_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Append one timestamped line to `agent.log` when `COPYSYNC_DEBUG` is set.
/// No-op (and zero filesystem work) otherwise. Best-effort: a logging failure is
/// swallowed because there's nowhere safer to report it.
fn dlog(msg: &str) {
    if !debug_enabled() {
        return;
    }
    let path = agent_log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "[{}] {}", debug_stamp(), msg);
    }
}

// ----------------------------------------------------------------- conversions

/// Convert the engine's `CoreStatus` to the IPC `Status`, folding in the three
/// live settings atomics (`privacy_filter`/`mark_sensitive`/`auto_clear_secs`) so
/// the GUI can seed its 설정 controls from the agent's REAL state, not hardcoded
/// defaults. Callers pass the current atomic values (the emitter's status callback
/// only holds `&CoreStatus`, so the settings are threaded in explicitly).
fn status_to_ipc(s: &CoreStatus, privacy_filter: bool, mark_sensitive: bool, auto_clear_secs: u64) -> IpcStatus {
    IpcStatus {
        paired: s.paired,
        connected: s.connected,
        reconnecting: s.reconnecting,
        server: s.server_name.clone(),
        device: s.device_name.clone(),
        e2e: s.e2e,
        pool: s.pool.clone(),
        pools: s.pools.clone(),
        privacy_filter,
        mark_sensitive,
        auto_clear_secs,
    }
}

fn roster_to_ipc(r: &[CoreRoster]) -> Vec<IpcRoster> {
    r.iter()
        .map(|d| IpcRoster {
            id: d.id.clone(),
            name: d.name.clone(),
            online: d.online,
        })
        .collect()
}

/// The engine hands clip events as loose JSON (`{"direction":..,"kind":..,..}`).
/// Parse it into a typed [`ClipInfo`]; missing fields default to `None`.
fn clip_to_info(v: &serde_json::Value) -> ClipInfo {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    ClipInfo {
        direction: s("direction").unwrap_or_default(),
        kind: s("kind").unwrap_or_default(),
        text: s("text"),
        name: s("name"),
        sensitive: s("sensitive"),
        size: v.get("size").and_then(|x| x.as_i64()),
        origin: s("origin"),
        path: s("path"),
        on_demand: v.get("onDemand").and_then(|x| x.as_bool()),
    }
}

fn entry_to_histrow(e: &copysync_core::history::Entry) -> HistRow {
    HistRow {
        ts: e.ts.clone(),
        kind: e.kind.clone(),
        direction: e.direction.clone(),
        origin: e.origin.clone(),
        preview: e.preview.clone(),
        size: e.size,
    }
}

// ----------------------------------------------------------------- emitter

/// The engine's UI sink, wired to broadcast each callback to subscribed IPC
/// clients (and OS toasts for `notify`).
struct BroadcastEmitter {
    events: broadcast::Sender<Event>,
    roster: Arc<Mutex<Vec<CoreRoster>>>,
    // Live settings atomics, cloned from `SharedState`, so a pushed `Event::Status`
    // carries the same real privacy_filter/mark_sensitive/auto_clear_secs values the
    // one-shot `GetStatus` snapshot does.
    exclude_sensitive: Arc<AtomicBool>,
    mark_sensitive: Arc<AtomicBool>,
    auto_clear_secs: Arc<AtomicU64>,
}

impl Emitter for BroadcastEmitter {
    fn status(&self, s: &CoreStatus) {
        let _ = self.events.send(Event::Status(status_to_ipc(
            s,
            self.exclude_sensitive.load(Ordering::Relaxed),
            self.mark_sensitive.load(Ordering::Relaxed),
            self.auto_clear_secs.load(Ordering::Relaxed),
        )));
    }
    fn clip(&self, payload: serde_json::Value) {
        let _ = self.events.send(Event::Clip(clip_to_info(&payload)));
    }
    fn roster(&self, r: &[CoreRoster]) {
        *self.roster.lock().unwrap_or_else(|e| e.into_inner()) = r.to_vec();
        let _ = self.events.send(Event::Roster(roster_to_ipc(r)));
    }
    fn reconnect(&self, info: String) {
        let _ = self.events.send(Event::Reconnect { info });
    }
    fn error(&self, msg: String) {
        let _ = self.events.send(Event::Error { message: msg });
    }
    fn cliplog(&self, msg: String) {
        let _ = self.events.send(Event::Cliplog { msg });
    }
    fn notify(&self, title: &str, body: &str) {
        let _ = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .show();
        let _ = self.events.send(Event::Notify {
            title: title.to_string(),
            body: body.to_string(),
        });
    }
}

// ----------------------------------------------------------------- engine host

/// The agent's engine host: owns the engine-shared state, the command channel to
/// the running actor, and the broadcast bus that fans engine callbacks out to
/// subscribed IPC clients.
struct Engine {
    state: SharedState,
    cmd_tx: Mutex<Option<UnboundedSender<Cmd>>>,
    events: broadcast::Sender<Event>,
    roster: Arc<Mutex<Vec<CoreRoster>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

/// The cloneable Arc bundle of engine-owned state (so we can hand a fresh
/// [`EngineState`] to each `engine::run` while the agent keeps its own handles).
#[derive(Clone)]
struct SharedState {
    hist: Arc<Mutex<History>>,
    status: Arc<Mutex<CoreStatus>>,
    roster: Arc<Mutex<Vec<CoreRoster>>>,
    targets: Arc<Mutex<Targets>>,
    exclude_sensitive: Arc<AtomicBool>,
    auto_clear_secs: Arc<AtomicU64>,
    mark_sensitive: Arc<AtomicBool>,
    reconnect: Arc<Notify>,
    cfg_path: PathBuf,
    downloads: PathBuf,
    data_dir: PathBuf,
}

impl SharedState {
    /// A fresh [`EngineState`] for one `engine::run` invocation (the engine takes
    /// it by value; the agent keeps the underlying Arcs via its own clone).
    fn engine_state(&self) -> EngineState {
        EngineState {
            hist: self.hist.clone(),
            status: self.status.clone(),
            roster: self.roster.clone(),
            targets: self.targets.clone(),
            exclude_sensitive: self.exclude_sensitive.clone(),
            auto_clear_secs: self.auto_clear_secs.clone(),
            mark_sensitive: self.mark_sensitive.clone(),
            reconnect: self.reconnect.clone(),
            cfg_path: self.cfg_path.clone(),
            downloads: self.downloads.clone(),
            data_dir: self.data_dir.clone(),
        }
    }
}

impl Engine {
    /// Read the three live settings atomics as a tuple, in the order
    /// `status_to_ipc` expects: `(privacy_filter, mark_sensitive, auto_clear_secs)`.
    fn settings_tuple(&self) -> (bool, bool, u64) {
        (
            self.state.exclude_sensitive.load(Ordering::Relaxed),
            self.state.mark_sensitive.load(Ordering::Relaxed),
            self.state.auto_clear_secs.load(Ordering::Relaxed),
        )
    }

    fn status_snapshot(&self) -> IpcStatus {
        let (pf, ms, ac) = self.settings_tuple();
        status_to_ipc(
            &self.state.status.lock().unwrap_or_else(|e| e.into_inner()),
            pf,
            ms,
            ac,
        )
    }

    fn new_emitter(&self) -> Arc<dyn Emitter> {
        Arc::new(BroadcastEmitter {
            events: self.events.clone(),
            roster: self.roster.clone(),
            exclude_sensitive: self.state.exclude_sensitive.clone(),
            mark_sensitive: self.state.mark_sensitive.clone(),
            auto_clear_secs: self.state.auto_clear_secs.clone(),
        })
    }

    /// Start (or restart) the sync actor for `cfg`. Ports `start_sync`: seeds the
    /// privacy/clear atomics + status from the config, then spins a RESTART
    /// SUPERVISOR task (stored in `self.join`) that owns the engine lifecycle.
    ///
    /// The supervisor loops: build a fresh `Cmd` channel (+ its clipboard watcher
    /// std thread), run `engine::run` on an inner task, and await its JoinHandle. If
    /// that inner task PANICKED (`JoinError::is_panic`), the supervisor logs, backs
    /// off (bounded, jittered), and relaunches — sync recovers on its own instead of
    /// staying dead until the daemon restarts. A clean `Ok(())` return from
    /// `engine::run` means an intentional/fatal stop (the actor's command channel
    /// closed, e.g. bad config early-return); the supervisor surfaces it and stops
    /// (no hot-loop). A re-pair or `Shutdown` aborts THIS supervisor task from the
    /// outside (see `start`'s abort below / the `Shutdown` handler), which tears
    /// down the whole loop cleanly — the supervisor never fights that lifecycle.
    fn start(self: &Arc<Self>, cfg: Config) {
        dlog(&format!(
            "engine start: server={:?} device={:?} e2e={}",
            cfg.server_name,
            cfg.device_name,
            !cfg.e2e_pass.is_empty()
        ));
        self.state
            .exclude_sensitive
            .store(cfg.exclude_sensitive, Ordering::Relaxed);
        self.state
            .auto_clear_secs
            .store(cfg.auto_clear_secs, Ordering::Relaxed);
        self.state
            .mark_sensitive
            .store(cfg.mark_received_sensitive, Ordering::Relaxed);
        {
            let mut s = self.state.status.lock().unwrap_or_else(|e| e.into_inner());
            s.paired = true;
            s.server_name = cfg.server_name.clone();
            s.device_name = cfg.device_name.clone();
            s.server_id = cfg.server_id.clone();
            s.e2e = !cfg.e2e_pass.is_empty();
        }

        // Abort any previous supervisor (a re-pair): dropping/aborting it stops the
        // old engine+clipboard loop so we never run two engines at once. The new
        // supervisor below installs its own fresh command channel.
        if let Some(prev) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            dlog("engine restart: aborting previous supervisor task");
            prev.abort();
        }

        let this = self.clone();
        let supervisor = tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                // (Re)build the command channel + clipboard watcher for this launch.
                let (tx, rx) = unbounded_channel::<Cmd>();
                *this.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
                let emit = this.new_emitter();
                {
                    // OS-clipboard watcher on its own std thread. It exits by itself
                    // when `tx` is dropped (its `tx.is_closed()` check), which happens
                    // when this loop iteration's `tx` + the actor's copy are gone.
                    let emit_cb = emit.clone();
                    std::thread::spawn(move || engine::clipboard_loop(tx, emit_cb));
                }

                let state = this.state.engine_state();
                let cfg_run = cfg.clone();
                let inner: JoinHandle<()> =
                    tokio::spawn(async move { engine::run(cfg_run, state, emit, rx).await });
                // If THIS supervisor is aborted (re-pair / Shutdown) while parked on
                // `inner.await`, the guard's Drop aborts `engine::run` too — otherwise
                // the inner task would detach and leak (see AbortOnDrop). On the normal
                // completion paths below the task is already finished, so the guard's
                // abort at scope-end is a harmless no-op.
                let _abort_inner = AbortOnDrop(inner.abort_handle());

                match inner.await {
                    Ok(()) => {
                        // The actor returned normally: its command channel closed or
                        // it hit a fatal early-return (bad pin/config). Surface it and
                        // STOP — relaunching would just fail again the same way.
                        dlog("engine stop: actor returned (channel closed / fatal) — marking disconnected, supervisor stops");
                        let _ = this.events.send(Event::Error {
                            message: "동기화가 멈췄습니다 — 다시 페어링하거나 에이전트를 재시작하세요.".into(),
                        });
                        let (pf, ms, ac) = this.settings_tuple();
                        let snap = {
                            let mut s = this.state.status.lock().unwrap_or_else(|e| e.into_inner());
                            s.connected = false;
                            s.reconnecting = false;
                            status_to_ipc(&s, pf, ms, ac)
                        };
                        let _ = this.events.send(Event::Status(snap));
                        break;
                    }
                    Err(e) if e.is_cancelled() => {
                        // The inner task was aborted from the outside (only the
                        // supervisor holds its handle, and it doesn't abort it) — treat
                        // as an intentional stop and exit without relaunching.
                        dlog("engine stop: actor task cancelled — supervisor stops");
                        break;
                    }
                    Err(e) => {
                        // Panic (or a runtime shutdown): log, mark disconnected, back
                        // off, then relaunch so a single malformed-payload/bug panic
                        // doesn't leave sync dead until the daemon is restarted.
                        attempt = attempt.saturating_add(1);
                        let delay = supervisor_backoff(attempt);
                        dlog(&format!(
                            "engine SUPERVISOR: actor task ended unexpectedly (panic={}) — relaunch #{} in {}ms",
                            e.is_panic(),
                            attempt,
                            delay.as_millis()
                        ));
                        let _ = this.events.send(Event::Error {
                            message: "동기화 엔진이 예기치 않게 중단되어 자동으로 재시작합니다.".into(),
                        });
                        let (pf, ms, ac) = this.settings_tuple();
                        let snap = {
                            let mut s = this.state.status.lock().unwrap_or_else(|e| e.into_inner());
                            s.connected = false;
                            s.reconnecting = true;
                            status_to_ipc(&s, pf, ms, ac)
                        };
                        let _ = this.events.send(Event::Status(snap));
                        tokio::time::sleep(delay).await;
                        // loop → relaunch
                    }
                }
            }
        });
        *self.join.lock().unwrap_or_else(|e| e.into_inner()) = Some(supervisor);
    }
}

/// Aborts the wrapped task when this guard is dropped.
///
/// The engine supervisor needs this because a re-pair / `Shutdown` stops the
/// engine by calling `abort()` on the SUPERVISOR task — but that only cancels the
/// supervisor future. The supervisor owns the inner `engine::run` handle as a
/// plain `JoinHandle`, and dropping a `JoinHandle` DETACHES its task rather than
/// aborting it. So without this guard the aborted supervisor left `engine::run`
/// running forever; because that leaked actor keeps the `Cmd` receiver alive, the
/// clipboard OS thread (which exits only on `tx.is_closed()`) also never stopped —
/// leaking a thread, a WS/TLS connection, and its buffers on every re-pair, and
/// running two engines at once. Holding the inner task's `AbortHandle` in a
/// drop-guard makes aborting the supervisor also abort `engine::run`, which drops
/// the receiver and lets the clipboard thread exit on its next poll tick.
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Bounded, jittered backoff for the engine restart supervisor: 1,2,4,8,16,30,30…
/// seconds plus up to ~700ms jitter so a crash-looping build doesn't hammer in a
/// tight loop and many devices don't relaunch in lockstep.
fn supervisor_backoff(attempt: u32) -> std::time::Duration {
    let secs = (1u64 << attempt.min(5)).min(30);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.subsec_millis() as u64) % 700)
        .unwrap_or(0);
    std::time::Duration::from_millis(secs * 1000 + jitter)
}

// ----------------------------------------------------------------- history key

/// 32-byte key for at-rest history encryption. OS keyring on macOS/Windows; a
/// 0600 key file fallback elsewhere. Ported from the Tauri shell's `history_key`.
fn history_key(data_dir: &Path) -> [u8; 32] {
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
            let k = e2e::random_key();
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
    let k = e2e::random_key();
    let _ = std::fs::write(&kf, k);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&kf, std::fs::Permissions::from_mode(0o600));
    }
    k
}

// ----------------------------------------------------------------- request handling

async fn handle_request(engine: &Arc<Engine>, req: Request) -> Outbound {
    match req {
        Request::GetStatus => Outbound::Reply(Response::Status(engine.status_snapshot())),

        Request::GetHistory { query } => {
            let rows = {
                let h = engine.state.hist.lock().unwrap_or_else(|e| e.into_inner());
                match query {
                    Some(q) if !q.trim().is_empty() => h.search(q.trim(), 200),
                    _ => h.recent(200),
                }
            };
            match rows {
                Ok(list) => {
                    Outbound::Reply(Response::History(list.iter().map(entry_to_histrow).collect()))
                }
                Err(e) => {
                    dlog(&format!("GetHistory failed: {e:#}"));
                    Outbound::Reply(Response::Error {
                        message: format!("기록 조회 실패: {e}"),
                    })
                }
            }
        }

        Request::GetRoster => {
            let r = engine.roster.lock().unwrap_or_else(|e| e.into_inner());
            Outbound::Reply(Response::Roster(roster_to_ipc(&r)))
        }

        Request::SendText { text } => {
            if text.is_empty() {
                return Outbound::Reply(Response::Ok);
            }
            send_cmd(engine, Cmd::SendText(text))
        }
        Request::SendFile { path } => send_cmd(engine, Cmd::SendFile(path)),
        Request::SetPool { pool } => send_cmd(engine, Cmd::SetPool(pool)),

        Request::SetTargets { ids } => {
            let t = if ids.is_empty() {
                Targets::All
            } else {
                Targets::Devices(ids)
            };
            *engine.state.targets.lock().unwrap_or_else(|e| e.into_inner()) = t;
            Outbound::Reply(Response::Ok)
        }

        Request::SetPrivacyFilter { on } => {
            engine.state.exclude_sensitive.store(on, Ordering::Relaxed);
            if let Ok(mut cfg) = Config::load(&engine.state.cfg_path) {
                cfg.exclude_sensitive = on;
                let _ = cfg.save(&engine.state.cfg_path);
            }
            Outbound::Reply(Response::Ok)
        }
        Request::SetAutoClear { secs } => {
            engine.state.auto_clear_secs.store(secs, Ordering::Relaxed);
            if let Ok(mut cfg) = Config::load(&engine.state.cfg_path) {
                cfg.auto_clear_secs = secs;
                let _ = cfg.save(&engine.state.cfg_path);
            }
            Outbound::Reply(Response::Ok)
        }
        Request::SetMarkSensitive { on } => {
            engine.state.mark_sensitive.store(on, Ordering::Relaxed);
            if let Ok(mut cfg) = Config::load(&engine.state.cfg_path) {
                cfg.mark_received_sensitive = on;
                let _ = cfg.save(&engine.state.cfg_path);
            }
            Outbound::Reply(Response::Ok)
        }

        Request::Reconnect => {
            engine.state.reconnect.notify_waiters();
            Outbound::Reply(Response::Ok)
        }

        Request::DiscoverServers => {
            match tokio::task::spawn_blocking(|| discovery::discover(2500)).await {
                Ok(Ok(found)) => Outbound::Reply(Response::Found(
                    found
                        .into_iter()
                        .map(|f| FoundServer {
                            name: f.name,
                            url: f.url,
                        })
                        .collect(),
                )),
                Ok(Err(e)) => {
                    dlog(&format!("DiscoverServers failed: {e:#}"));
                    Outbound::Reply(Response::Error {
                        message: format!("검색 실패: {e}"),
                    })
                }
                Err(e) => {
                    dlog(&format!("DiscoverServers join error: {e:#}"));
                    Outbound::Reply(Response::Error {
                        message: format!("검색 작업 실패: {e}"),
                    })
                }
            }
        }

        Request::Pair {
            server,
            otp,
            name,
            pin,
            e2e_pass,
        } => {
            dlog(&format!("Pair attempt: server={server:?} name={name:?}"));
            match pairing::claim(&server, &pin, &otp, &name, &e2e_pass).await {
                Ok(cfg) => {
                    if let Err(e) = cfg.save(&engine.state.cfg_path) {
                        dlog(&format!("Pair config save failed: {e:#}"));
                        return Outbound::Reply(Response::Error {
                            message: format!("설정 저장 실패: {e}"),
                        });
                    }
                    dlog("Pair succeeded — starting engine");
                    engine.start(cfg);
                    Outbound::Reply(Response::Paired(engine.status_snapshot()))
                }
                Err(e) => {
                    dlog(&format!("Pair failed: {e:#}"));
                    Outbound::Reply(Response::Error {
                        message: e.to_string(),
                    })
                }
            }
        }

        // The streaming is handled in the connection loop; this is just an ack.
        Request::Subscribe => Outbound::Reply(Response::Ok),

        // Real shutdown: stop the engine and terminate this process. We schedule
        // the `process::exit(0)` on a short delay so the `Ok` reply below has a
        // chance to flush back to the caller (the GUI's tray "종료") before the
        // daemon dies; the GUI ignores the reply either way. Aborting the supervisor
        // task tears down its engine actor + clipboard watcher, and it won't relaunch
        // (an aborted supervisor is gone, not restarted).
        Request::Shutdown => {
            dlog("Shutdown requested — stopping engine and exiting process");
            // Clear the stored sender so no late IPC command races the teardown, then
            // abort the SUPERVISOR (which owns the engine actor + clipboard thread).
            *engine.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if let Some(prev) = engine.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
                prev.abort();
            }
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(150));
                std::process::exit(0);
            });
            Outbound::Reply(Response::Ok)
        }
    }
}

/// Forward a command to the running actor, or report that pairing is required.
fn send_cmd(engine: &Arc<Engine>, cmd: Cmd) -> Outbound {
    match engine.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(tx) => match tx.send(cmd) {
            Ok(()) => Outbound::Reply(Response::Ok),
            Err(_) => {
                dlog("send_cmd failed: actor channel closed (sync stopped)");
                Outbound::Reply(Response::Error {
                    message: "동기화가 중단됨 — 다시 페어링하세요.".into(),
                })
            }
        },
        None => {
            dlog("send_cmd rejected: not paired (no command channel)");
            Outbound::Reply(Response::Error {
                message: "페어링이 필요합니다.".into(),
            })
        }
    }
}

// ----------------------------------------------------------------- IPC server

async fn serve(engine: Arc<Engine>) -> Result<()> {
    let name = socket_label().to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .context("bind local socket (is another agent already running?)")?;
    eprintln!("copysync-agent: listening on {}", socket_label());
    dlog(&format!("IPC listening on {}", socket_label()));
    loop {
        match listener.accept().await {
            Ok(conn) => {
                dlog("IPC client connected");
                let eng = engine.clone();
                tokio::spawn(async move {
                    match handle_conn(eng, conn).await {
                        Ok(()) => dlog("IPC client disconnected (clean EOF)"),
                        Err(e) => {
                            eprintln!("copysync-agent: connection ended: {e}");
                            dlog(&format!("IPC connection ended with error: {e:#}"));
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("copysync-agent: accept error: {e}");
                dlog(&format!("IPC accept error: {e:#}"));
            }
        }
    }
}

async fn handle_conn(engine: Arc<Engine>, conn: TokioStream) -> Result<()> {
    let (read, mut write) = conn.split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let mut subscribed = false;
    let mut rx: Option<broadcast::Receiver<Event>> = None;

    loop {
        if let Some(events) = rx.as_mut() {
            // Subscribed: multiplex client requests with pushed events.
            tokio::select! {
                n = reader.read_line(&mut line) => {
                    let n = n?;
                    if n == 0 { return Ok(()); }
                    if let Some(out) = process_line(&engine, line.trim(), &mut subscribed).await {
                        write.write_all(out.to_line().as_bytes()).await?;
                        write.flush().await?;
                    }
                    line.clear();
                }
                ev = events.recv() => match ev {
                    Ok(ev) => {
                        let out = Outbound::Event(ev);
                        write.write_all(out.to_line().as_bytes()).await?;
                        write.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Dropped some events under backpressure — resync the full
                        // pushed state. Status alone is not enough: a Roster event
                        // (or a clip storm displacing one) inside the dropped window
                        // would otherwise leave the GUI's device list frozen, since
                        // the GUI only updates its roster from pushed Roster events.
                        // History is pulled on demand, so Status + Roster suffices.
                        let status = Outbound::Event(Event::Status(engine.status_snapshot()));
                        write.write_all(status.to_line().as_bytes()).await?;
                        let roster = {
                            let r = engine.roster.lock().unwrap_or_else(|e| e.into_inner());
                            Outbound::Event(Event::Roster(roster_to_ipc(&r)))
                        };
                        write.write_all(roster.to_line().as_bytes()).await?;
                        write.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => { rx = None; }
                }
            }
        } else {
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(());
            }
            let was_subscribed = subscribed;
            if let Some(out) = process_line(&engine, line.trim(), &mut subscribed).await {
                write.write_all(out.to_line().as_bytes()).await?;
                write.flush().await?;
            }
            line.clear();
            // A fresh subscription: push a status snapshot, then start streaming.
            if subscribed && !was_subscribed {
                rx = Some(engine.events.subscribe());
                let out = Outbound::Event(Event::Status(engine.status_snapshot()));
                write.write_all(out.to_line().as_bytes()).await?;
                write.flush().await?;
            }
        }
    }
}

/// Parse one request line and produce its reply; flips `subscribed` on Subscribe.
async fn process_line(
    engine: &Arc<Engine>,
    trimmed: &str,
    subscribed: &mut bool,
) -> Option<Outbound> {
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<Request>(trimmed) {
        Ok(req) => {
            if matches!(req, Request::Subscribe) {
                *subscribed = true;
            }
            Some(handle_request(engine, req).await)
        }
        Err(e) => {
            dlog(&format!("bad request line: {e} (raw: {trimmed:?})"));
            Some(Outbound::Reply(Response::Error {
                message: format!("bad request: {e}"),
            }))
        }
    }
}

// ----------------------------------------------------------------- bootstrap

fn build_engine() -> Result<Arc<Engine>> {
    let config_dir = dirs::config_dir()
        .map(|d| d.join("copysync"))
        .unwrap_or_else(|| std::env::temp_dir().join("copysync"));
    let data_dir = dirs::data_dir()
        .map(|d| d.join("copysync"))
        .unwrap_or_else(|| config_dir.clone());
    std::fs::create_dir_all(&config_dir).ok();
    std::fs::create_dir_all(&data_dir).ok();

    let cfg_path = config_dir.join("config.json");
    let downloads = data_dir.join("downloads");

    let key = history_key(&data_dir);
    let hist = History::open(data_dir.join("history.db"), Some(key))
        .context("open history db")?;

    // Seed the privacy/clear flags from a saved config if there is one.
    let loaded = Config::load(&cfg_path).ok();
    let exclude = loaded.as_ref().map(|c| c.exclude_sensitive).unwrap_or(true);
    let auto_clear = loaded.as_ref().map(|c| c.auto_clear_secs).unwrap_or(0);
    let mark = loaded
        .as_ref()
        .map(|c| c.mark_received_sensitive)
        .unwrap_or(false);

    let state = SharedState {
        hist: Arc::new(Mutex::new(hist)),
        status: Arc::new(Mutex::new(CoreStatus::default())),
        roster: Arc::new(Mutex::new(Vec::new())),
        targets: Arc::new(Mutex::new(Targets::All)),
        exclude_sensitive: Arc::new(AtomicBool::new(exclude)),
        auto_clear_secs: Arc::new(AtomicU64::new(auto_clear)),
        mark_sensitive: Arc::new(AtomicBool::new(mark)),
        reconnect: Arc::new(Notify::new()),
        cfg_path,
        downloads,
        data_dir,
    };

    let (events, _) = broadcast::channel::<Event>(256);
    let roster = state.roster.clone();
    let engine = Arc::new(Engine {
        state,
        cmd_tx: Mutex::new(None),
        events,
        roster,
        join: Mutex::new(None),
    });

    // Auto-start sync if a saved pairing exists.
    if let Some(cfg) = loaded {
        dlog("found saved pairing — auto-starting sync");
        engine.start(cfg);
    } else {
        dlog("no saved pairing — idle until Pair");
    }
    Ok(engine)
}

fn run_serve() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async {
        dlog("agent serve starting (COPYSYNC_DEBUG on)");
        let engine = build_engine().inspect_err(|e| {
            dlog(&format!("build_engine failed: {e:#}"));
        })?;
        let result = serve(engine).await;
        if let Err(e) = &result {
            dlog(&format!("serve exited with error: {e:#}"));
        }
        result
    })
}

/// CLI client: connect, send one request, print the reply line. Sync I/O is fine
/// — these are one-shot probes, not the long-lived server. Doubles as a headless
/// control surface for the daemon (no GUI needed).
fn client_send(req: &Request) -> Result<()> {
    use std::io::{BufRead, BufReader as SyncBufReader, Write};
    let name = socket_label().to_ns_name::<GenericNamespaced>()?;
    let conn = SyncStream::connect(name).context("connect to agent (is it running?)")?;
    let mut reader = SyncBufReader::new(conn);
    reader
        .get_mut()
        .write_all((serde_json::to_string(req)? + "\n").as_bytes())?;
    reader.get_mut().flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    println!("{}", line.trim());
    Ok(())
}

/// Subscribe and stream every pushed event until killed (live debug feed).
fn client_watch() -> Result<()> {
    use std::io::{BufRead, BufReader as SyncBufReader, Write};
    let name = socket_label().to_ns_name::<GenericNamespaced>()?;
    let conn = SyncStream::connect(name).context("connect to agent (is it running?)")?;
    let mut reader = SyncBufReader::new(conn);
    reader
        .get_mut()
        .write_all((serde_json::to_string(&Request::Subscribe)? + "\n").as_bytes())?;
    reader.get_mut().flush()?;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        print!("{line}");
        std::io::stdout().flush().ok();
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") | None => {
            // Detach from any console so the background daemon never shows a
            // terminal window — covers login-autostart + double-click. (The GUI
            // already spawns us with CREATE_NO_WINDOW.) Only the serve daemon hits
            // this arm, so the CLI subcommands below keep their stdout.
            #[cfg(windows)]
            {
                extern "system" {
                    fn FreeConsole() -> i32;
                }
                let _ = unsafe { FreeConsole() };
            }
            run_serve()
        }
        Some("ping") | Some("status") => client_send(&Request::GetStatus),
        Some("history") => client_send(&Request::GetHistory { query: None }),
        Some("discover") => client_send(&Request::DiscoverServers),
        Some("reconnect") => client_send(&Request::Reconnect),
        Some("watch") => client_watch(),
        Some("send") => client_send(&Request::SendText {
            text: args.get(2..).map(|r| r.join(" ")).unwrap_or_default(),
        }),
        Some("pair") => {
            let r = &args[2..];
            client_send(&Request::Pair {
                server: flag(r, "--server").context("--server required")?,
                otp: flag(r, "--otp").context("--otp required")?,
                name: flag(r, "--name").unwrap_or_else(|| "desktop".into()),
                pin: flag(r, "--pin").unwrap_or_default(),
                e2e_pass: flag(r, "--e2e").unwrap_or_default(),
            })
        }
        Some(other) => {
            eprintln!("usage: copysync-agent [serve|status|pair|send <text>|history|watch|discover|reconnect] (got {other:?})");
            std::process::exit(2);
        }
    }
}
