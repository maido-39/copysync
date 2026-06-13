//! LAN server discovery via mDNS (`_copysync._tcp`), mirroring `copyctl discover`
//! and the Android NsdManager "search" button. Pure network I/O — no display.

use std::time::{Duration, Instant};

/// A CopySync server found on the local network.
#[derive(serde::Serialize, Clone, Debug)]
pub struct Found {
    pub name: String,
    pub url: String,
}

/// Browse for CopySync servers for up to `timeout_ms`, returning de-duplicated
/// `https://ip:port` URLs. Blocking (mDNS uses a blocking channel) — call it from
/// a blocking context, e.g. `spawn_blocking`.
pub fn discover(timeout_ms: u64) -> anyhow::Result<Vec<Found>> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon: {e}"))?;
    let recv = daemon
        .browse("_copysync._tcp.local.")
        .map_err(|e| anyhow::anyhow!("mdns browse: {e}"))?;

    let mut out: Vec<Found> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    while Instant::now() < deadline {
        match recv.recv_timeout(Duration::from_millis(250)) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                let name = info
                    .get_property_val_str("name")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| info.get_hostname().trim_end_matches('.').to_string());
                for addr in info.get_addresses() {
                    if addr.is_ipv4() {
                        let url = format!("https://{addr}:{port}");
                        if seen.insert(url.clone()) {
                            out.push(Found { name: name.clone(), url });
                        }
                    }
                }
            }
            Ok(_) => {}
            // recv timeout tick — keep looping until the overall deadline.
            Err(_) => {}
        }
    }
    let _ = daemon.shutdown();
    Ok(out)
}
