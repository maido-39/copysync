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

// ----------------------------------------------------------------- conversions

fn status_to_ipc(s: &CoreStatus) -> IpcStatus {
    IpcStatus {
        connected: s.connected,
        reconnecting: s.reconnecting,
        server: s.server_name.clone(),
        device: s.device_name.clone(),
        e2e: s.e2e,
        pool: s.pool.clone(),
        pools: s.pools.clone(),
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
}

impl Emitter for BroadcastEmitter {
    fn status(&self, s: &CoreStatus) {
        let _ = self.events.send(Event::Status(status_to_ipc(s)));
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
    fn status_snapshot(&self) -> IpcStatus {
        status_to_ipc(&self.state.status.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn new_emitter(&self) -> Arc<dyn Emitter> {
        Arc::new(BroadcastEmitter {
            events: self.events.clone(),
            roster: self.roster.clone(),
        })
    }

    /// Start (or restart) the sync actor for `cfg`. Ports `start_sync`: seeds the
    /// privacy/clear atomics + status from the config, spins the clipboard watcher
    /// on a std thread, and runs `engine::run` on a tokio task. A re-pair replaces
    /// the old command sender (dropping it closes the channel → the old actor and
    /// clipboard thread exit) and aborts the previous actor task.
    fn start(self: &Arc<Self>, cfg: Config) {
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

        let (tx, rx) = unbounded_channel::<Cmd>();
        // Swap in the new sender; dropping the previous one closes its channel so
        // the prior actor's `rx.recv()` returns None and it shuts down cleanly.
        *self.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        // Abort any previous actor task so we never run two engines at once.
        if let Some(prev) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            prev.abort();
        }

        let emit = self.new_emitter();

        // OS-clipboard watcher on its own std thread (the engine API requires it).
        {
            let emit_cb = emit.clone();
            std::thread::spawn(move || engine::clipboard_loop(tx, emit_cb));
        }

        let state = self.state.engine_state();
        let events = self.events.clone();
        let status = self.state.status.clone();
        let handle = tokio::spawn(async move {
            engine::run(cfg, state, emit, rx).await;
            // The actor returned (command channel closed / fatal). Surface it and
            // mark the status disconnected so later sends fail loudly, not silently.
            let _ = events.send(Event::Error {
                message: "동기화가 멈췄습니다 — 다시 페어링하거나 에이전트를 재시작하세요.".into(),
            });
            let snap = {
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.connected = false;
                s.reconnecting = false;
                status_to_ipc(&s)
            };
            let _ = events.send(Event::Status(snap));
        });
        *self.join.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }
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
                Err(e) => Outbound::Reply(Response::Error {
                    message: format!("기록 조회 실패: {e}"),
                }),
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
                Ok(Err(e)) => Outbound::Reply(Response::Error {
                    message: format!("검색 실패: {e}"),
                }),
                Err(e) => Outbound::Reply(Response::Error {
                    message: format!("검색 작업 실패: {e}"),
                }),
            }
        }

        Request::Pair {
            server,
            otp,
            name,
            pin,
            e2e_pass,
        } => match pairing::claim(&server, &pin, &otp, &name, &e2e_pass).await {
            Ok(cfg) => {
                if let Err(e) = cfg.save(&engine.state.cfg_path) {
                    return Outbound::Reply(Response::Error {
                        message: format!("설정 저장 실패: {e}"),
                    });
                }
                engine.start(cfg);
                Outbound::Reply(Response::Paired(engine.status_snapshot()))
            }
            Err(e) => Outbound::Reply(Response::Error {
                message: e.to_string(),
            }),
        },

        // The streaming is handled in the connection loop; this is just an ack.
        Request::Subscribe => Outbound::Reply(Response::Ok),
    }
}

/// Forward a command to the running actor, or report that pairing is required.
fn send_cmd(engine: &Arc<Engine>, cmd: Cmd) -> Outbound {
    match engine.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(tx) => match tx.send(cmd) {
            Ok(()) => Outbound::Reply(Response::Ok),
            Err(_) => Outbound::Reply(Response::Error {
                message: "동기화가 중단됨 — 다시 페어링하세요.".into(),
            }),
        },
        None => Outbound::Reply(Response::Error {
            message: "페어링이 필요합니다.".into(),
        }),
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
    loop {
        match listener.accept().await {
            Ok(conn) => {
                let eng = engine.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(eng, conn).await {
                        eprintln!("copysync-agent: connection ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("copysync-agent: accept error: {e}"),
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
                        // Dropped some events under backpressure — resync state.
                        let out = Outbound::Event(Event::Status(engine.status_snapshot()));
                        write.write_all(out.to_line().as_bytes()).await?;
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
        Err(e) => Some(Outbound::Reply(Response::Error {
            message: format!("bad request: {e}"),
        })),
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
        engine.start(cfg);
    }
    Ok(engine)
}

fn run_serve() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async {
        let engine = build_engine()?;
        serve(engine).await
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
        Some("serve") | None => run_serve(),
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
