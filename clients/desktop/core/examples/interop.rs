//! Headless interop driver for copysync-core. Exercises the real protocol against
//! a running Go server so we can cross-check with `copyctl` (and prove the
//! zero-knowledge server) without a GUI.
//!
//!   interop e2ekey  <pass> <serverId>
//!   interop pair    <server> <otp> <name> <outConfig> [e2ePass] [pin]
//!   interop send    <config> <text>
//!   interop sendfile <config> <path>
//!   interop recv    <config> <seconds>

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};

use copysync_core::protocol::{self, Ack, ClipEvent, EncMeta, Targets};
use copysync_core::{blob, e2e, pairing, pinning, ws, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "e2ekey" => {
            let pass = arg(&args, 1)?;
            let sid = arg(&args, 2)?;
            println!("{}", e2e::key_id(&e2e::derive_key(pass, sid)));
        }
        "pair" => {
            let server = arg(&args, 1)?;
            let otp = arg(&args, 2)?;
            let name = arg(&args, 3)?;
            let out = arg(&args, 4)?;
            let e2e_pass = args.get(5).map(|s| s.as_str()).unwrap_or("");
            let pin = args.get(6).map(|s| s.as_str()).unwrap_or("");
            let cfg = pairing::claim(server, pin, otp, name, e2e_pass).await?;
            cfg.save(out)?;
            println!(
                "paired device_id={} server_id={} e2e={}",
                cfg.device_id,
                cfg.server_id,
                !cfg.e2e_pass.is_empty()
            );
        }
        "send" => send_text(arg(&args, 1)?, arg(&args, 2)?).await?,
        "sendfile" => send_file(arg(&args, 1)?, arg(&args, 2)?).await?,
        "recv" => {
            let secs: u64 = arg(&args, 2)?.parse()?;
            recv(arg(&args, 1)?, secs).await?
        }
        other => return Err(anyhow!("unknown command {other:?}")),
    }
    Ok(())
}

fn arg<'a>(args: &'a [String], i: usize) -> Result<&'a str> {
    args.get(i)
        .map(|s| s.as_str())
        .ok_or_else(|| anyhow!("missing argument #{i}"))
}

async fn send_text(config: &str, text: &str) -> Result<()> {
    let cfg = Config::load(config)?;
    let pin = cfg.pin_bytes()?;
    let (mut ws, _hello) =
        ws::connect(&cfg.server_url, pin, &cfg.device_id, &cfg.device_name, &cfg.token).await?;
    let key = cfg.e2e_key();
    let ev = ClipEvent::new_text(
        1,
        text,
        key.as_ref().map(|(k, id)| (k.as_slice(), id.as_str())),
        Targets::All,
    )?;
    let id = ev.id.clone();
    ws::send(&mut ws, protocol::T_CLIP, &ev).await?;
    println!("sent id={id} enc={}", ev.enc.is_some());
    wait_ack(&mut ws, &id).await
}

async fn send_file(config: &str, path: &str) -> Result<()> {
    let cfg = Config::load(config)?;
    let pin = cfg.pin_bytes()?;
    let content = std::fs::read(path)?;
    let key = cfg.e2e_key();
    let payload = match &key {
        Some((k, _)) => e2e::seal(k, &content)?,
        None => content.clone(),
    };
    let http = pinning::http_client(pin);
    let bid = blob::put_blob(&http, &cfg.server_url, &cfg.token, payload.clone()).await?;
    let (mut ws, _hello) =
        ws::connect(&cfg.server_url, pin, &cfg.device_id, &cfg.device_name, &cfg.token).await?;
    let ev = ClipEvent {
        id: protocol::new_id(),
        seq: 1,
        ts: protocol::now_ts(),
        mime: vec![mime_of(path)],
        name: file_name(path),
        blob_id: bid,
        size: content.len() as i64,
        sha256: hex::encode(Sha256::digest(&payload)),
        targets: Targets::All,
        enc: key.as_ref().map(|(_, kid)| EncMeta {
            alg: e2e::ALG.into(),
            key_id: kid.clone(),
            nonce: String::new(),
        }),
        ..Default::default()
    };
    let id = ev.id.clone();
    ws::send(&mut ws, protocol::T_CLIP, &ev).await?;
    println!(
        "sent file id={id} name={} bytes={} enc={}",
        ev.name,
        content.len(),
        ev.enc.is_some()
    );
    wait_ack(&mut ws, &id).await
}

async fn recv(config: &str, secs: u64) -> Result<()> {
    let cfg = Config::load(config)?;
    let pin = cfg.pin_bytes()?;
    let (mut ws, hello) =
        ws::connect(&cfg.server_url, pin, &cfg.device_id, &cfg.device_name, &cfg.token).await?;
    eprintln!("connected to {} ({})", hello.server_name, hello.server_id);
    let http = blob::pull_client(pin);
    let key = cfg.e2e_key();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        let remaining = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, ws::recv(&mut ws)).await {
            Err(_) => break,            // idle timeout
            Ok(Ok(None)) => break,      // closed
            Ok(Err(e)) => return Err(e),
            Ok(Ok(Some((t, d)))) => {
                if t != protocol::T_CLIP {
                    continue;
                }
                let ev: ClipEvent = serde_json::from_value(d)?;
                if ev.is_blob() {
                    let data =
                        blob::get_blob(&http, &cfg.server_url, &cfg.token, &ev.blob_id).await?;
                    let plain = decrypt_bytes(&ev, data, &key)?;
                    println!(
                        "CLIP {} name={} bytes={} sha_in={}",
                        ev.kind(),
                        ev.name,
                        plain.len(),
                        hex::encode(Sha256::digest(&plain))
                    );
                } else {
                    println!("CLIP text {}", decrypt_text(&ev, &key)?);
                }
            }
        }
    }
    Ok(())
}

fn decrypt_text(ev: &ClipEvent, key: &Option<(Vec<u8>, String)>) -> Result<String> {
    if ev.enc.is_none() {
        return Ok(ev.inline_text.clone());
    }
    match key {
        Some((k, _)) => {
            let raw = STANDARD.decode(&ev.inline_text)?;
            Ok(String::from_utf8(e2e::open(k, &raw)?)?)
        }
        None => Ok("<encrypted: no passphrase>".into()),
    }
}

fn decrypt_bytes(ev: &ClipEvent, data: Vec<u8>, key: &Option<(Vec<u8>, String)>) -> Result<Vec<u8>> {
    if ev.enc.is_none() {
        return Ok(data);
    }
    match key {
        Some((k, _)) => e2e::open(k, &data),
        None => Ok(data),
    }
}

async fn wait_ack(ws: &mut ws::Ws, id: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, ws::recv(ws)).await {
            Err(_) => break,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e),
            Ok(Ok(Some((t, d)))) => {
                if t == protocol::T_ACK {
                    let a: Ack = serde_json::from_value(d)?;
                    if a.id == id {
                        println!("ack:{}", a.status);
                        return Ok(());
                    }
                }
            }
        }
    }
    Err(anyhow!("timed out waiting for ack"))
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string()
}

fn mime_of(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}
