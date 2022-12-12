//! UDP relay: one upstream socket per client address, expired when idle and re-created
//! periodically so the target name is resolved again.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep_until};

use crate::config::{Config, Listener};
use crate::metrics::ListenerStats;
use crate::warn;

const QUEUE: usize = 256;

struct Session {
    id: u64,
    tx: mpsc::Sender<Vec<u8>>,
}

type Sessions = Arc<Mutex<HashMap<SocketAddr, Session>>>;

pub async fn bind(addr: SocketAddr, spec: &Listener) -> Result<Arc<UdpSocket>> {
    let socket = UdpSocket::bind(addr)
        .await
        .with_context(|| format!("binding UDP {addr} for {}", spec.name))?;
    Ok(Arc::new(socket))
}

pub async fn serve(
    socket: Arc<UdpSocket>,
    spec: Listener,
    config: Arc<Config>,
    stats: Arc<ListenerStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let sessions: Sessions = Arc::default();
    let spec = Arc::new(spec);
    let ids = AtomicU64::new(0);
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, client) = tokio::select! {
            _ = shutdown.changed() => return,
            r = socket.recv_from(&mut buf) => match r { Ok(v) => v, Err(_) => continue },
        };
        let packet = buf[..n].to_vec();
        let mut map = sessions.lock().unwrap();
        if let Some(s) = map.get(&client) {
            let _ = s.tx.try_send(packet); // a full queue drops, as the network would
            continue;
        }
        if map.len() >= config.tuning.max_sessions {
            stats.rejected.fetch_add(1, Relaxed);
            continue;
        }
        let (tx, rx) = mpsc::channel(QUEUE);
        let _ = tx.try_send(packet);
        let id = ids.fetch_add(1, Relaxed);
        map.insert(client, Session { id, tx });
        drop(map);
        let (socket, spec, config, stats, sessions) = (
            socket.clone(),
            spec.clone(),
            config.clone(),
            stats.clone(),
            sessions.clone(),
        );
        tokio::spawn(async move {
            stats.accepted.fetch_add(1, Relaxed);
            stats.active.fetch_add(1, Relaxed);
            if let Err(e) = session(&socket, client, rx, &spec, &config, &stats).await {
                stats.failed.fetch_add(1, Relaxed);
                warn!(
                    "UDP target unreachable",
                    listener = spec.name,
                    error = format!("{e:#}")
                );
            }
            stats.active.fetch_sub(1, Relaxed);
            let mut map = sessions.lock().unwrap();
            if map.get(&client).is_some_and(|s| s.id == id) {
                map.remove(&client);
            }
        });
    }
}

async fn session(
    listener: &UdpSocket,
    client: SocketAddr,
    mut rx: mpsc::Receiver<Vec<u8>>,
    spec: &Listener,
    config: &Config,
    stats: &ListenerStats,
) -> Result<()> {
    let t = &config.tuning;
    let target = tokio::time::timeout(
        Duration::from_secs(t.connect_timeout_seconds),
        tokio::net::lookup_host((spec.target_host.as_str(), spec.target_port)),
    )
    .await
    .map_err(|_| anyhow!("resolving {} timed out", spec.target_host))??
    .next()
    .ok_or_else(|| anyhow!("{} has no addresses", spec.target_host))?;
    let upstream = UdpSocket::bind(if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await?;
    upstream.connect(target).await?;
    let refresh = Instant::now() + Duration::from_secs(t.udp_refresh_seconds);
    let idle = Duration::from_secs(t.udp_idle_seconds);
    let mut expires = Instant::now() + idle;
    let mut buf = vec![0u8; 65536];
    loop {
        tokio::select! {
            packet = rx.recv() => {
                let Some(packet) = packet else { return Ok(()) };
                if upstream.send(&packet).await.is_ok() {
                    stats.bytes_forward.fetch_add(packet.len() as u64, Relaxed);
                }
                expires = Instant::now() + idle;
            }
            r = upstream.recv(&mut buf) => {
                if let Ok(n) = r {
                    if listener.send_to(&buf[..n], client).await.is_ok() {
                        stats.bytes_reverse.fetch_add(n as u64, Relaxed);
                    }
                    expires = Instant::now() + idle;
                }
            }
            _ = sleep_until(expires.min(refresh)) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn relays_datagrams_both_ways() {
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let (n, from) = echo.recv_from(&mut buf).await.unwrap();
                echo.send_to(&buf[..n], from).await.unwrap();
            }
        });
        let config = Config::parse(
            &serde_json::json!({
            "localAddress": "169.254.99.1", "peerAddress": "169.254.99.2",
            "peerEndpoint": "127.0.0.1:1", "listenPort": 51822,
            "listeners": [{"name": "echo", "bind": "tunnel", "protocol": "UDP", "listenPort": 20000,
                "targetHost": "localhost", "targetPort": echo_port}]})
            .to_string(),
        )
        .unwrap();
        let spec = config.listeners[0].clone();
        let socket = bind("127.0.0.1:0".parse().unwrap(), &spec).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let stats = ListenerStats::new(&spec);
        let (_tx, rx) = watch::channel(false);
        tokio::spawn(serve(socket, spec, Arc::new(config), stats.clone(), rx));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buf = [0u8; 2048];
        for i in 0..50u8 {
            client.send_to(&[i; 1200], addr).await.unwrap();
            let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], &[i; 1200]);
        }
        assert_eq!(stats.accepted.load(Relaxed), 1, "one association per client");
        assert_eq!(stats.bytes_reverse.load(Relaxed), 50 * 1200);
    }
}
