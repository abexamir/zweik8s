//! Counters shared by the data path, rendered in Prometheus text format.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{Bind, Config, Listener};

#[derive(Default)]
pub struct ListenerStats {
    pub name: String,
    pub protocol: &'static str,
    pub kind: &'static str,
    pub accepted: AtomicU64,
    pub active: AtomicI64,
    pub rejected: AtomicU64,
    pub failed: AtomicU64,
    /// From the connecting side to the target.
    pub bytes_forward: AtomicU64,
    /// From the target back to the connecting side.
    pub bytes_reverse: AtomicU64,
}

impl ListenerStats {
    pub fn new(l: &Listener) -> Arc<ListenerStats> {
        Arc::new(ListenerStats {
            name: l.name.clone(),
            protocol: l.protocol.as_str(),
            kind: if l.bind == Bind::Pod { "import" } else { "export" },
            ..Default::default()
        })
    }
}

#[derive(Default)]
pub struct Stats {
    pub interface: String,
    pub ready: AtomicBool,
    pub listeners: std::sync::Mutex<Vec<Arc<ListenerStats>>>,
    /// Unix milliseconds of the last probe reply from the peer; 0 = never.
    pub peer_last_reply_ms: AtomicU64,
    pub peer_rtt_us: AtomicU64,
    /// Unix seconds of WireGuard's latest handshake; 0 = never.
    pub handshake_s: AtomicU64,
    pub transport_resets: AtomicU64,
    pub to_peer_packets: AtomicU64,
    pub to_peer_bytes: AtomicU64,
    pub from_peer_packets: AtomicU64,
    pub from_peer_bytes: AtomicU64,
    pub bridge_errors: AtomicU64,
    pub started_ms: AtomicU64,
}

/// name, type, help, value
type Series = (
    &'static str,
    &'static str,
    &'static str,
    fn(&ListenerStats) -> String,
);

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Stats {
    pub fn new(config: &Config) -> Arc<Stats> {
        let s = Stats {
            interface: config.interface.clone(),
            ..Default::default()
        };
        s.started_ms.store(now_ms(), Relaxed);
        Arc::new(s)
    }

    pub fn add_listener(&self, l: Arc<ListenerStats>) {
        self.listeners.lock().unwrap().push(l);
    }

    /// Seconds since the peer last answered a probe, or None if it never has.
    pub fn peer_reply_age(&self) -> Option<f64> {
        match self.peer_last_reply_ms.load(Relaxed) {
            0 => None,
            t => Some(now_ms().saturating_sub(t) as f64 / 1000.0),
        }
    }

    pub fn active_sessions(&self) -> i64 {
        self.listeners
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.active.load(Relaxed))
            .sum()
    }

    fn interface_counter(&self, name: &str) -> u64 {
        std::fs::read_to_string(format!("/sys/class/net/{}/statistics/{name}", self.interface))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn status_json(&self) -> String {
        let listeners: Vec<_> = self
            .listeners
            .lock()
            .unwrap()
            .iter()
            .map(|l| {
                serde_json::json!({
                    "name": l.name, "protocol": l.protocol, "kind": l.kind,
                    "active": l.active.load(Relaxed), "accepted": l.accepted.load(Relaxed),
                    "rejected": l.rejected.load(Relaxed), "failed": l.failed.load(Relaxed),
                    "bytesForward": l.bytes_forward.load(Relaxed), "bytesReverse": l.bytes_reverse.load(Relaxed),
                })
            })
            .collect();
        let handshake = self.handshake_s.load(Relaxed);
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "ready": self.ready.load(Relaxed),
            "peerReplyAgeSeconds": self.peer_reply_age(),
            "peerRttMilliseconds": self.peer_rtt_us.load(Relaxed) as f64 / 1000.0,
            "handshakeAgeSeconds": (handshake > 0).then(|| (now_ms() / 1000).saturating_sub(handshake)),
            "transportResets": self.transport_resets.load(Relaxed),
            "listeners": listeners,
        })
        .to_string()
            + "\n"
    }

    pub fn prometheus(&self) -> String {
        let mut o = String::with_capacity(4096);
        let gauge = |o: &mut String, name: &str, help: &str, kind: &str, value: String| {
            let _ = writeln!(
                o,
                "# HELP zweik8s_{name} {help}\n# TYPE zweik8s_{name} {kind}\nzweik8s_{name} {value}"
            );
        };
        let _ = writeln!(
            o,
            "# HELP zweik8s_build_info Build information.\n# TYPE zweik8s_build_info gauge\nzweik8s_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        gauge(
            &mut o,
            "ready",
            "1 when every listener is serving.",
            "gauge",
            (self.ready.load(Relaxed) as u8).to_string(),
        );
        let age = self.peer_reply_age();
        gauge(
            &mut o,
            "peer_up",
            "1 when the peer answered a probe within the last 15 seconds.",
            "gauge",
            (age.is_some_and(|a| a < 15.0) as u8).to_string(),
        );
        gauge(
            &mut o,
            "peer_reply_age_seconds",
            "Seconds since the peer last answered a probe (-1: never).",
            "gauge",
            age.map_or("-1".into(), |a| format!("{a:.3}")),
        );
        gauge(
            &mut o,
            "peer_rtt_seconds",
            "Round-trip time of the last probe through the tunnel.",
            "gauge",
            format!("{:.6}", self.peer_rtt_us.load(Relaxed) as f64 / 1e6),
        );
        let hs = self.handshake_s.load(Relaxed);
        gauge(
            &mut o,
            "wireguard_handshake_age_seconds",
            "Seconds since the latest WireGuard handshake (-1: never).",
            "gauge",
            if hs == 0 {
                "-1".into()
            } else {
                (now_ms() / 1000).saturating_sub(hs).to_string()
            },
        );
        gauge(
            &mut o,
            "transport_resets_total",
            "Times the encrypted transport was reset after the peer went silent.",
            "counter",
            self.transport_resets.load(Relaxed).to_string(),
        );
        gauge(
            &mut o,
            "bridge_errors_total",
            "Encrypted packets the UDP bridge could not forward.",
            "counter",
            self.bridge_errors.load(Relaxed).to_string(),
        );
        let _ = writeln!(
            o,
            "# HELP zweik8s_encrypted_packets_total Encrypted packets through the UDP bridge.\n# TYPE zweik8s_encrypted_packets_total counter"
        );
        let _ = writeln!(
            o,
            "zweik8s_encrypted_packets_total{{direction=\"to_peer\"}} {}",
            self.to_peer_packets.load(Relaxed)
        );
        let _ = writeln!(
            o,
            "zweik8s_encrypted_packets_total{{direction=\"from_peer\"}} {}",
            self.from_peer_packets.load(Relaxed)
        );
        let _ = writeln!(
            o,
            "# HELP zweik8s_encrypted_bytes_total Encrypted bytes through the UDP bridge.\n# TYPE zweik8s_encrypted_bytes_total counter"
        );
        let _ = writeln!(
            o,
            "zweik8s_encrypted_bytes_total{{direction=\"to_peer\"}} {}",
            self.to_peer_bytes.load(Relaxed)
        );
        let _ = writeln!(
            o,
            "zweik8s_encrypted_bytes_total{{direction=\"from_peer\"}} {}",
            self.from_peer_bytes.load(Relaxed)
        );
        let _ = writeln!(
            o,
            "# HELP zweik8s_tunnel_bytes_total Decrypted bytes on the WireGuard interface.\n# TYPE zweik8s_tunnel_bytes_total counter"
        );
        let _ = writeln!(
            o,
            "zweik8s_tunnel_bytes_total{{direction=\"rx\"}} {}",
            self.interface_counter("rx_bytes")
        );
        let _ = writeln!(
            o,
            "zweik8s_tunnel_bytes_total{{direction=\"tx\"}} {}",
            self.interface_counter("tx_bytes")
        );

        let listeners = self.listeners.lock().unwrap();
        let series: [Series; 6] = [
            ("sessions_active", "gauge", "Open sessions.", |l| {
                l.active.load(Relaxed).to_string()
            }),
            ("sessions_total", "counter", "Sessions accepted.", |l| {
                l.accepted.load(Relaxed).to_string()
            }),
            (
                "sessions_rejected_total",
                "counter",
                "Sessions refused because maxSessions was reached.",
                |l| l.rejected.load(Relaxed).to_string(),
            ),
            (
                "sessions_failed_total",
                "counter",
                "Sessions whose target could not be reached.",
                |l| l.failed.load(Relaxed).to_string(),
            ),
            (
                "forward_bytes_total",
                "counter",
                "Bytes from the connecting side to the target.",
                |l| l.bytes_forward.load(Relaxed).to_string(),
            ),
            (
                "reverse_bytes_total",
                "counter",
                "Bytes from the target back to the connecting side.",
                |l| l.bytes_reverse.load(Relaxed).to_string(),
            ),
        ];
        for (name, kind, help, value) in series {
            let _ = writeln!(o, "# HELP zweik8s_{name} {help}\n# TYPE zweik8s_{name} {kind}");
            for l in listeners.iter() {
                let _ = writeln!(
                    o,
                    "zweik8s_{name}{{listener=\"{}\",protocol=\"{}\",kind=\"{}\"}} {}",
                    l.name,
                    l.protocol,
                    l.kind,
                    value(l)
                );
            }
        }
        o
    }
}
