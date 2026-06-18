//! copysync-agent — the headless background sync daemon.
//!
//! M1 (this file) stands up the control plane: a per-user local-socket IPC
//! server that speaks the [`copysync_ipc`] vocabulary, plus a `ping` self-test
//! client. The real sync engine (lifted from the Tauri shell over
//! `copysync-core`) lands in M2; for now requests get stub answers so the
//! transport + framing can be verified end-to-end, headlessly.
//!
//!   copysync-agent          # serve (default)
//!   copysync-agent ping     # connect, GetStatus, print the reply

use std::io::{BufRead, BufReader, Write};

use anyhow::{Context, Result};
use copysync_ipc::{socket_label, Event, Outbound, Request, Response, Status};
use interprocess::local_socket::{
    prelude::*, GenericNamespaced, ListenerOptions, Stream,
};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("ping") => ping(),
        Some("serve") | None => serve(),
        Some(other) => {
            eprintln!("usage: copysync-agent [serve|ping] (got {other:?})");
            std::process::exit(2);
        }
    }
}

/// A placeholder status until M2 wires the real engine.
fn stub_status() -> Status {
    Status {
        connected: false,
        reconnecting: false,
        server: "(agent skeleton — engine lands in M2)".into(),
        device: "desktop".into(),
        e2e: false,
        pool: "default".into(),
        pools: vec!["default".into()],
    }
}

fn answer(req: &Request) -> Outbound {
    match req {
        Request::GetStatus => Outbound::Reply(Response::Status(stub_status())),
        Request::GetHistory { .. } => Outbound::Reply(Response::History(vec![])),
        Request::Subscribe => Outbound::Event(Event::Status(stub_status())),
        Request::SendText { .. }
        | Request::SendFile { .. }
        | Request::SetPool { .. }
        | Request::Reconnect => Outbound::Reply(Response::Ok),
    }
}

fn serve() -> Result<()> {
    let name = socket_label().to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_sync()
        .context("bind local socket (is another agent already running?)")?;
    eprintln!("copysync-agent: listening on {}", socket_label());
    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                std::thread::spawn(move || {
                    if let Err(e) = serve_conn(conn) {
                        eprintln!("copysync-agent: connection ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("copysync-agent: accept error: {e}"),
        }
    }
    Ok(())
}

/// One client connection: read newline-delimited [`Request`]s, write replies.
fn serve_conn(conn: Stream) -> Result<()> {
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // EOF — client closed
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let out = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => answer(&req),
            Err(e) => Outbound::Reply(Response::Error {
                message: format!("bad request: {e}"),
            }),
        };
        reader.get_mut().write_all(out.to_line().as_bytes())?;
        reader.get_mut().flush()?;
    }
}

/// Self-test client: connect, send GetStatus, print the reply.
fn ping() -> Result<()> {
    let name = socket_label().to_ns_name::<GenericNamespaced>()?;
    let conn = Stream::connect(name).context("connect to agent (is it running?)")?;
    let mut reader = BufReader::new(conn);
    let req = serde_json::to_string(&Request::GetStatus)? + "\n";
    reader.get_mut().write_all(req.as_bytes())?;
    reader.get_mut().flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    println!("agent replied: {}", line.trim());
    Ok(())
}
