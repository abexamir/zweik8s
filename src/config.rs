//! Runtime configuration: one JSON file, usually rendered by the Helm chart.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

/// Where a listener accepts traffic.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Bind {
    /// An import: callers in this cluster connect to the pod IP; traffic goes to the peer.
    Pod,
    /// An export: the peer connects to our tunnel address; traffic goes to a local target.
    Tunnel,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Listener {
    pub name: String,
    pub bind: Bind,
    pub protocol: Protocol,
    pub listen_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Keys {
    pub private_key: PathBuf,
    pub peer_public_key: PathBuf,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            private_key: "/keys/private-key".into(),
            peer_public_key: "/keys/peer-public-key".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Tuning {
    /// Concurrent sessions per listener; new ones beyond this are refused.
    pub max_sessions: usize,
    pub connect_timeout_seconds: u64,
    pub udp_idle_seconds: u64,
    /// UDP associations are re-created this often so DNS changes take effect.
    pub udp_refresh_seconds: u64,
    /// Congestion control for TCP legs that cross the tunnel. Empty keeps the kernel default.
    pub tcp_congestion: String,
    /// Receive/send buffer for the encrypted UDP sockets.
    pub socket_buffer_bytes: usize,
    pub keepalive_idle_seconds: u64,
    pub keepalive_interval_seconds: u64,
    pub keepalive_count: u32,
    pub probe_interval_seconds: u64,
    /// Without a probe reply for this long, the transport is reset.
    pub peer_timeout_seconds: u64,
    pub wireguard_keepalive_seconds: u16,
    /// On SIGTERM, wait this long for open TCP sessions to finish.
    pub drain_seconds: u64,
    /// Runtime worker threads; 0 uses every CPU the container may use.
    pub workers: usize,
    /// Use splice(2) to move TCP data without copying it through user space.
    pub splice: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            max_sessions: 1024,
            connect_timeout_seconds: 10,
            udp_idle_seconds: 60,
            udp_refresh_seconds: 15,
            tcp_congestion: "bbr".into(),
            socket_buffer_bytes: 4 << 20,
            keepalive_idle_seconds: 60,
            keepalive_interval_seconds: 10,
            keepalive_count: 3,
            probe_interval_seconds: 5,
            peer_timeout_seconds: 45,
            wireguard_keepalive_seconds: 15,
            drain_seconds: 10,
            workers: 0,
            splice: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Health {
    /// Health and metrics HTTP server.
    pub bind: SocketAddr,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            bind: "127.0.0.1:9090".parse().unwrap(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_interface")]
    pub interface: String,
    pub local_address: Ipv4Addr,
    pub peer_address: Ipv4Addr,
    /// The peer's public UDP endpoint, `host:port`. The host may be a DNS name.
    pub peer_endpoint: String,
    /// Public UDP port of this gateway. The kernel WireGuard socket uses the next port.
    pub listen_port: u16,
    #[serde(default = "default_mtu")]
    pub mtu: u16,
    #[serde(default = "default_control_port")]
    pub control_port: u16,
    #[serde(default)]
    pub keys: Keys,
    #[serde(default)]
    pub listeners: Vec<Listener>,
    #[serde(default)]
    pub tuning: Tuning,
    #[serde(default)]
    pub health: Health,
}

fn default_interface() -> String {
    "zweik8s".into()
}
fn default_mtu() -> u16 {
    1380
}
fn default_control_port() -> u16 {
    19999
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = serde_json::from_str(text).context("parsing config")?;
        config.validate()?;
        Ok(config)
    }

    /// Ports the gateway itself binds for UDP; ephemeral sockets must avoid them.
    pub fn reserved_udp_ports(&self) -> HashSet<u16> {
        let mut ports: HashSet<u16> = [self.listen_port, self.wireguard_port(), self.control_port].into();
        ports.extend(
            self.listeners
                .iter()
                .filter(|l| l.protocol == Protocol::Udp)
                .map(|l| l.listen_port),
        );
        ports
    }

    pub fn wireguard_port(&self) -> u16 {
        self.listen_port + 1
    }

    pub fn peer_endpoint_parts(&self) -> Result<(&str, u16)> {
        let (host, port) = self
            .peer_endpoint
            .rsplit_once(':')
            .context("peerEndpoint must be host:port")?;
        let port: u16 = port.parse().context("peerEndpoint port")?;
        ensure!(!host.is_empty() && port != 0, "peerEndpoint must be host:port");
        Ok((host, port))
    }

    pub fn validate(&self) -> Result<()> {
        let iface = &self.interface;
        ensure!(
            !iface.is_empty()
                && iface.len() <= 15
                && iface
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "interface must be 1-15 characters of [A-Za-z0-9_-]"
        );
        ensure!(
            self.local_address != self.peer_address,
            "localAddress and peerAddress must differ"
        );
        for a in [self.local_address, self.peer_address] {
            ensure!(
                !a.is_unspecified() && !a.is_multicast() && !a.is_broadcast(),
                "tunnel addresses must be unicast"
            );
        }
        self.peer_endpoint_parts()?;
        ensure!(
            self.listen_port >= 1024 && self.listen_port < u16::MAX,
            "listenPort must be 1024-65534"
        );
        ensure!((576..=1420).contains(&self.mtu), "mtu must be 576-1420");
        ensure!(
            self.control_port != self.listen_port && self.control_port != self.wireguard_port(),
            "controlPort conflicts with listenPort"
        );
        let t = &self.tuning;
        ensure!(t.max_sessions > 0, "maxSessions must be positive");
        ensure!(
            t.probe_interval_seconds > 0 && t.peer_timeout_seconds > t.probe_interval_seconds,
            "peerTimeoutSeconds must exceed probeIntervalSeconds"
        );
        ensure!(
            t.connect_timeout_seconds > 0 && t.udp_idle_seconds > 0 && t.udp_refresh_seconds > 0,
            "timeouts must be positive"
        );
        ensure!(
            t.socket_buffer_bytes >= 65536,
            "socketBufferBytes must be at least 65536"
        );
        ensure!(
            t.tcp_congestion.len() < 16
                && t.tcp_congestion
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "tcpCongestion must be a kernel congestion control name"
        );

        let mut names = HashSet::new();
        let mut ports = HashSet::new();
        for l in &self.listeners {
            ensure!(
                !l.name.is_empty() && l.name.len() <= 63,
                "listener name must be 1-63 characters"
            );
            ensure!(
                names.insert((&l.name, l.protocol)),
                "duplicate listener {} ({})",
                l.name,
                l.protocol.as_str()
            );
            ensure!(
                l.listen_port >= 1024,
                "listener {}: listenPort must be at least 1024",
                l.name
            );
            ensure!(
                ports.insert((l.listen_port, l.protocol)),
                "listener {}: port {}/{} used twice",
                l.name,
                l.listen_port,
                l.protocol.as_str()
            );
            ensure!(
                l.target_port != 0 && !l.target_host.is_empty(),
                "listener {}: target required",
                l.name
            );
            if l.protocol == Protocol::Udp
                && [self.listen_port, self.wireguard_port(), self.control_port].contains(&l.listen_port)
            {
                bail!(
                    "listener {}: UDP port {} is used by the tunnel",
                    l.name,
                    l.listen_port
                );
            }
            if l.bind == Bind::Pod && l.target_host != self.peer_address.to_string() {
                bail!(
                    "listener {}: an import must target the peer tunnel address",
                    l.name
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "localAddress": "169.254.99.2", "peerAddress": "169.254.99.1",
            "peerEndpoint": "203.0.113.10:31820", "listenPort": 51822,
            "listeners": [
                {"name": "db", "bind": "pod", "protocol": "TCP", "listenPort": 20101,
                 "targetHost": "169.254.99.1", "targetPort": 20101},
                {"name": "api", "bind": "tunnel", "protocol": "UDP", "listenPort": 20200,
                 "targetHost": "api.default.svc.cluster.local", "targetPort": 53}
            ]
        })
    }

    type Mutation = Box<dyn Fn(&mut serde_json::Value)>;

    fn parse(v: serde_json::Value) -> Result<Config> {
        Config::parse(&v.to_string())
    }

    #[test]
    fn defaults() {
        let c = parse(base()).unwrap();
        assert_eq!(c.interface, "zweik8s");
        assert_eq!(c.mtu, 1380);
        assert_eq!(c.wireguard_port(), 51823);
        assert_eq!(c.tuning.tcp_congestion, "bbr");
        assert!(c.reserved_udp_ports().contains(&20200));
        assert_eq!(c.peer_endpoint_parts().unwrap(), ("203.0.113.10", 31820));
    }

    #[test]
    fn rejects_bad_configs() {
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "same addresses",
                Box::new(|v| v["peerAddress"] = "169.254.99.2".into()),
            ),
            ("bad endpoint", Box::new(|v| v["peerEndpoint"] = "nohost".into())),
            ("mtu", Box::new(|v| v["mtu"] = 9000.into())),
            ("unknown field", Box::new(|v| v["extra"] = 1.into())),
            (
                "udp on wireguard port",
                Box::new(|v| v["listeners"][1]["listenPort"] = 51823.into()),
            ),
            (
                "duplicate port",
                Box::new(|v| {
                    v["listeners"][1]["protocol"] = "TCP".into();
                    v["listeners"][1]["listenPort"] = 20101.into();
                }),
            ),
            (
                "import to elsewhere",
                Box::new(|v| v["listeners"][0]["targetHost"] = "10.0.0.1".into()),
            ),
            (
                "long interface",
                Box::new(|v| v["interface"] = "averyveryverylongname".into()),
            ),
            (
                "timeouts",
                Box::new(|v| v["tuning"] = serde_json::json!({"peerTimeoutSeconds": 1})),
            ),
        ];
        for (name, mutate) in cases {
            let mut v = base();
            mutate(&mut v);
            assert!(parse(v).is_err(), "{name} should be rejected");
        }
    }
}
