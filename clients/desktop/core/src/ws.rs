//! WebSocket control channel: pinned-TLS dial + `hello`/`hello_ok` handshake,
//! plus thin send/recv helpers. The connection is single-owner; concurrent
//! senders and the read loop should live in one task (`select!`) or actor.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

use crate::protocol::{self, Hello, HelloErr, HelloOk};
use crate::{decode, encode, PROTO};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn to_ws_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let b = b.replacen("https://", "wss://", 1);
    let b = b.replacen("http://", "ws://", 1);
    format!("{b}/ws")
}

/// Dial `/ws`, enforce the SPKI pin, and complete the hello handshake.
pub async fn connect(
    base: &str,
    pin: [u8; 32],
    device_id: &str,
    device_name: &str,
    token: &str,
) -> Result<(Ws, HelloOk)> {
    let connector = Connector::Rustls(Arc::new(crate::pinning::build_config(pin)));
    let (mut ws, _resp) =
        connect_async_tls_with_config(to_ws_url(base), None, false, Some(connector)).await?;

    let hello = Hello {
        device_id: device_id.to_string(),
        device_name: device_name.to_string(),
        token: token.to_string(),
        platform: "linux".to_string(),
        proto: PROTO,
    };
    send(&mut ws, protocol::T_HELLO, &hello).await?;

    let first = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("connection closed before hello_ok"))??;
    let data = first.into_data();
    let (t, d) = decode(data.as_ref())?;
    match t.as_str() {
        protocol::T_HELLO_OK => Ok((ws, serde_json::from_value(d)?)),
        protocol::T_HELLO_ERR => {
            let he: HelloErr = serde_json::from_value(d).unwrap_or_default();
            Err(anyhow!(
                "server rejected connection: {} ({})",
                he.message,
                he.code
            ))
        }
        other => Err(anyhow!("unexpected first frame {other:?}")),
    }
}

/// Send one control frame.
pub async fn send<T: Serialize>(ws: &mut Ws, t: &str, d: &T) -> Result<()> {
    ws.send(Message::Binary(encode(t, d)?.into())).await?;
    Ok(())
}

/// Read the next control frame, skipping ping/pong and stopping on close.
/// Returns the type tag and the raw `d` payload, or `None` at end of stream.
pub async fn recv(ws: &mut Ws) -> Result<Option<(String, serde_json::Value)>> {
    loop {
        match ws.next().await {
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(msg)) => {
                let data = msg.into_data();
                if data.is_empty() {
                    continue;
                }
                return Ok(Some(decode(data.as_ref())?));
            }
            Some(Err(e)) => return Err(e.into()),
        }
    }
}
