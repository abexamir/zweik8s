//! The encrypted UDP path and its watchdog.
//!
//! Kernel WireGuard talks only to a pod-local loopback socket. The bridge forwards its
//! packets to the peer's *configured* public endpoint from an ephemeral socket, and hands
//! everything that arrives back to WireGuard. WireGuard therefore never learns a transient
//! NAT address. When the peer stops answering probes, the bridge opens a new ephemeral
//! socket (a fresh NAT/conntrack tuple), re-resolves the endpoint, and resets the session.
//!
//! The three directions run as independent tasks on a multi-threaded runtime and move
//! packets in batches with recvmmsg/sendmmsg.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use socket2::{Domain, Socket, Type};
use tokio::io::Interest;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::metrics::{Stats, now_ms};
use crate::mmsg::{Batch, Dest};
use crate::{info, warn, wg};

/// Never reset the transport more often than this.
const MIN_RESET_SPACING_MS: u64 = 12_000;

pub struct Transport {
    config: Arc<Config>,
    stats: Arc<Stats>,
    reserved: HashSet<u16>,
    loopback: Arc<UdpSocket>,
    public: Arc<UdpSocket>,
    outgoing: watch::Sender<Arc<UdpSocket>>,
    outgoing_task: Mutex<Option<JoinHandle<()>>>,
    peer: watch::Sender<SocketAddrV4>,
    wireguard: SocketAddrV4,
}

fn set_buffers(socket: &Socket, bytes: usize) {
    let fd = socket.as_raw_fd();
    let value = bytes as libc::c_int;
    for (force, plain) in [
        (libc::SO_RCVBUFFORCE, libc::SO_RCVBUF),
        (libc::SO_SNDBUFFORCE, libc::SO_SNDBUF),
    ] {
        // SAFETY: setsockopt with a c_int value. FORCE needs CAP_NET_ADMIN; fall back if denied.
        let ok = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                force,
                &value as *const _ as *const libc::c_void,
                4,
            )
        } == 0;
        if !ok {
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    plain,
                    &value as *const _ as *const libc::c_void,
                    4,
                )
            };
        }
    }
}

/// Bind a UDP socket. For port 0, keep drawing ports until one is not reserved by a
/// listener that may not be bound yet.
fn bind_udp(addr: SocketAddrV4, reserved: &HashSet<u16>, buffer: usize) -> Result<Arc<UdpSocket>> {
    let mut rejected = Vec::new();
    loop {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?;
        socket.set_nonblocking(true)?;
        socket.set_cloexec(true)?;
        set_buffers(&socket, buffer);
        socket
            .bind(&SocketAddr::V4(addr).into())
            .with_context(|| format!("binding UDP {addr}"))?;
        let port = socket.local_addr()?.as_socket().map(|a| a.port()).unwrap_or(0);
        if addr.port() == 0 && reserved.contains(&port) {
            rejected.push(socket); // keep it bound so the next draw differs
            continue;
        }
        return Ok(Arc::new(UdpSocket::from_std(socket.into())?));
    }
}

async fn resolve(endpoint: &str) -> Result<SocketAddrV4> {
    tokio::net::lookup_host(endpoint)
        .await
        .with_context(|| format!("resolving {endpoint}"))?
        .find_map(|a| match a {
            SocketAddr::V4(v4) => Some(v4),
            SocketAddr::V6(_) => None,
        })
        .ok_or_else(|| anyhow!("{endpoint} has no IPv4 address"))
}

/// Send the whole batch, waiting for socket space; drops the rest on a hard error.
async fn send_batch(socket: &UdpSocket, batch: &Batch, dest: &Dest, stats: &Stats) {
    let mut sent = 0;
    while sent < batch.count {
        match socket
            .async_io(Interest::WRITABLE, || batch.send(socket.as_raw_fd(), dest, sent))
            .await
        {
            Ok(n) => sent += n.max(1),
            Err(_) => {
                stats
                    .bridge_errors
                    .fetch_add((batch.count - sent) as u64, Relaxed);
                return;
            }
        }
    }
}

impl Transport {
    pub async fn bind(config: Arc<Config>, stats: Arc<Stats>) -> Result<Arc<Transport>> {
        let reserved = config.reserved_udp_ports();
        let buffer = config.tuning.socket_buffer_bytes;
        let peer = resolve(&config.peer_endpoint).await?;
        let loopback = bind_udp(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &reserved, buffer)?;
        let outgoing = bind_udp(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), &reserved, buffer)?;
        let public = bind_udp(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.listen_port),
            &reserved,
            buffer,
        )?;
        let wireguard = SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.wireguard_port());
        info!("transport bound", peer = peer, public_port = config.listen_port);
        Ok(Arc::new(Transport {
            reserved,
            loopback,
            public,
            outgoing: watch::Sender::new(outgoing),
            outgoing_task: Mutex::new(None),
            peer: watch::Sender::new(peer),
            wireguard,
            config,
            stats,
        }))
    }

    /// The loopback port WireGuard uses as its peer endpoint.
    pub fn bridge_port(&self) -> u16 {
        self.loopback.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Start the forwarding tasks. They run until the process exits.
    pub fn start(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let me = self.clone();
        let to_peer = tokio::spawn(async move { me.forward_to_peer().await });
        let me = self.clone();
        let public = self.public.clone();
        let from_public = tokio::spawn(async move { me.forward_from_peer(public).await });
        let me = self.clone();
        let outgoing = self.outgoing.borrow().clone();
        *self.outgoing_task.lock().unwrap() =
            Some(tokio::spawn(async move { me.forward_from_peer(outgoing).await }));
        vec![to_peer, from_public]
    }

    async fn forward_to_peer(&self) {
        let mut batch = Batch::default();
        let mut peer_rx = self.peer.subscribe();
        let mut out_rx = self.outgoing.subscribe();
        loop {
            if self
                .loopback
                .async_io(Interest::READABLE, || batch.recv(self.loopback.as_raw_fd()))
                .await
                .is_err()
                || batch.count == 0
            {
                continue;
            }
            let dest = Dest::new(*peer_rx.borrow_and_update());
            let socket = out_rx.borrow_and_update().clone();
            self.stats.to_peer_packets.fetch_add(batch.count as u64, Relaxed);
            self.stats.to_peer_bytes.fetch_add(batch.bytes(), Relaxed);
            send_batch(&socket, &batch, &dest, &self.stats).await;
        }
    }

    async fn forward_from_peer(&self, socket: Arc<UdpSocket>) {
        let mut batch = Batch::default();
        let dest = Dest::new(self.wireguard);
        loop {
            if socket
                .async_io(Interest::READABLE, || batch.recv(socket.as_raw_fd()))
                .await
                .is_err()
                || batch.count == 0
            {
                continue;
            }
            self.stats
                .from_peer_packets
                .fetch_add(batch.count as u64, Relaxed);
            self.stats.from_peer_bytes.fetch_add(batch.bytes(), Relaxed);
            send_batch(&self.loopback, &batch, &dest, &self.stats).await;
        }
    }

    /// New ephemeral source socket, re-resolved endpoint, fresh WireGuard session.
    async fn reset(self: &Arc<Self>, public_key: &str) -> Result<()> {
        match resolve(&self.config.peer_endpoint).await {
            Ok(peer) => {
                self.peer.send_replace(peer);
            }
            Err(e) => warn!("keeping the previous peer address", error = e),
        }
        // The old socket stays bound until the new one exists, so the port differs.
        let socket = bind_udp(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            &self.reserved,
            self.config.tuning.socket_buffer_bytes,
        )?;
        let me = self.clone();
        let task = tokio::spawn({
            let socket = socket.clone();
            async move { me.forward_from_peer(socket).await }
        });
        self.outgoing.send_replace(socket);
        if let Some(old) = self.outgoing_task.lock().unwrap().replace(task) {
            old.abort();
        }
        let (config, key, port) = (self.config.clone(), public_key.to_string(), self.bridge_port());
        tokio::task::spawn_blocking(move || wg::reset_peer(&config, &key, port)).await??;
        Ok(())
    }

    /// Answer the peer's probes, probe the peer, and reset the transport when it goes silent.
    pub async fn watchdog(self: Arc<Self>, public_key: String) -> Result<()> {
        let config = &self.config;
        let socket = UdpSocket::bind((config.local_address, config.control_port))
            .await
            .context("binding the probe socket")?;
        let socket = Arc::new(socket);
        let peer = SocketAddr::from((config.peer_address, config.control_port));
        let pending: Arc<Mutex<([u8; 16], Instant)>> = Arc::new(Mutex::new(([0; 16], Instant::now())));

        let responder = {
            let (socket, pending, stats) = (socket.clone(), pending.clone(), self.stats.clone());
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                        continue;
                    };
                    if from != peer || n != 17 {
                        continue;
                    }
                    match buf[0] {
                        b'P' => {
                            buf[0] = b'R';
                            let _ = socket.send_to(&buf[..17], peer).await;
                        }
                        b'R' => {
                            let (nonce, sent) = *pending.lock().unwrap();
                            if buf[1..17] == nonce {
                                stats
                                    .peer_rtt_us
                                    .store(sent.elapsed().as_micros() as u64, Relaxed);
                                stats.peer_last_reply_ms.store(now_ms(), Relaxed);
                            }
                        }
                        _ => {}
                    }
                }
            })
        };

        let interval = Duration::from_secs(config.tuning.probe_interval_seconds);
        let timeout = config.tuning.peer_timeout_seconds * 1000;
        let mut last_reset = now_ms();
        let mut was_up = false;
        let result = loop {
            let mut packet = [b'P'; 17];
            // SAFETY: getrandom writes at most 16 bytes into the buffer.
            unsafe { libc::getrandom(packet[1..].as_mut_ptr() as *mut libc::c_void, 16, 0) };
            *pending.lock().unwrap() = (packet[1..].try_into().unwrap(), Instant::now());
            let _ = socket.send_to(&packet, peer).await;
            tokio::time::sleep(interval).await;

            self.stats
                .handshake_s
                .store(wg::latest_handshake(&config.interface).await, Relaxed);
            let now = now_ms();
            let last_reply = self.stats.peer_last_reply_ms.load(Relaxed);
            let silent = now.saturating_sub(last_reply.max(self.stats.started_ms.load(Relaxed)));
            let up = now.saturating_sub(last_reply) < 15_000;
            if up != was_up {
                if up {
                    info!(
                        "peer reachable",
                        rtt_ms = self.stats.peer_rtt_us.load(Relaxed) as f64 / 1000.0
                    );
                } else {
                    warn!("peer not answering probes");
                }
                was_up = up;
            }
            // WireGuard retries a handshake every 5 s; resetting faster than two retries would
            // throw away handshakes that are about to complete.
            if silent > timeout && now.saturating_sub(last_reset) > timeout.max(MIN_RESET_SPACING_MS) {
                warn!(
                    "resetting the encrypted transport",
                    silent_seconds = silent / 1000
                );
                if let Err(e) = self.reset(&public_key).await {
                    break Err(e);
                }
                last_reset = now;
                self.stats.transport_resets.fetch_add(1, Relaxed);
            }
        };
        responder.abort();
        result
    }
}
