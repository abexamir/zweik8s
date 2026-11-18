//! Kernel WireGuard interface and the pod-local nftables table. Everything stays inside the
//! pod's own network namespace.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{Bind, Config, Protocol};

pub const TABLE: &str = "zweik8s";

fn run(args: &[&str], input: Option<&str>) -> Result<()> {
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..]).stdout(Stdio::null()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", args[0]))?;
    if let Some(text) = input {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(text.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn quiet(args: &[&str]) {
    let _ = Command::new(args[0])
        .args(&args[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Create the interface, keys, peer, route and firewall. `bridge_port` is the loopback UDP
/// port WireGuard sends encrypted packets to.
pub fn setup(config: &Config, bridge_port: u16) -> Result<String> {
    let iface = config.interface.as_str();
    // A restarted container finds its previous interface still in the pod namespace.
    quiet(&["ip", "link", "del", iface]);
    let local = format!("{}/32", config.local_address);
    let peer = format!("{}/32", config.peer_address);
    let mtu = config.mtu.to_string();
    let wg_port = config.wireguard_port().to_string();
    let private_key = config.keys.private_key.to_str().context("private key path")?;
    run(&["ip", "link", "add", iface, "type", "wireguard"], None)?;
    run(&["ip", "address", "add", &local, "dev", iface], None)?;
    run(&["ip", "link", "set", iface, "mtu", &mtu], None)?;
    run(
        &[
            "wg",
            "set",
            iface,
            "private-key",
            private_key,
            "listen-port",
            &wg_port,
        ],
        None,
    )?;
    let public_key = std::fs::read_to_string(&config.keys.peer_public_key)
        .with_context(|| format!("reading {}", config.keys.peer_public_key.display()))?
        .trim()
        .to_string();
    if public_key.len() != 44 {
        bail!("peer public key must be a base64 WireGuard key");
    }
    configure_peer(config, &public_key, bridge_port)?;
    install_firewall(config)?;
    run(&["ip", "link", "set", iface, "up"], None)?;
    run(&["ip", "route", "add", &peer, "dev", iface], None)?;
    Ok(public_key)
}

pub fn configure_peer(config: &Config, public_key: &str, bridge_port: u16) -> Result<()> {
    let endpoint = format!("127.0.0.1:{bridge_port}");
    let allowed = format!("{}/32", config.peer_address);
    let keepalive = config.tuning.wireguard_keepalive_seconds.to_string();
    run(
        &[
            "wg",
            "set",
            &config.interface,
            "peer",
            public_key,
            "endpoint",
            &endpoint,
            "allowed-ips",
            &allowed,
            "persistent-keepalive",
            &keepalive,
        ],
        None,
    )
}

/// Drop the peer's session state so the next packet starts a fresh handshake.
pub fn reset_peer(config: &Config, public_key: &str, bridge_port: u16) -> Result<()> {
    run(
        &["wg", "set", &config.interface, "peer", public_key, "remove"],
        None,
    )?;
    configure_peer(config, public_key, bridge_port)
}

/// Unix time of the peer's latest handshake, 0 if none yet.
pub async fn latest_handshake(interface: &str) -> u64 {
    let out = tokio::process::Command::new("wg")
        .args(["show", interface, "latest-handshakes"])
        .output()
        .await;
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().nth(1).and_then(|t| t.parse().ok()))
        .unwrap_or(0)
}

/// A raw peer may send any IP packet; only declared exports and the probe get through.
pub fn firewall(config: &Config) -> String {
    let iface = &config.interface;
    let (peer, local) = (config.peer_address, config.local_address);
    let mut rules = vec![
        format!("table inet {TABLE} {{"),
        "  chain forward { type filter hook forward priority 0; policy drop; counter drop; }".to_string(),
        "  chain input { type filter hook input priority 0; policy accept;".to_string(),
        format!("    iifname \"{iface}\" ct state established,related accept"),
        format!(
            "    iifname \"{iface}\" ip saddr {peer} ip daddr {local} udp dport {} accept",
            config.control_port
        ),
    ];
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        let mut ports: Vec<u16> = config
            .listeners
            .iter()
            .filter(|l| l.bind == Bind::Tunnel && l.protocol == protocol)
            .map(|l| l.listen_port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        if !ports.is_empty() {
            let list = ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
            rules.push(format!(
                "    iifname \"{iface}\" ip saddr {peer} ip daddr {local} {} dport {{ {list} }} accept",
                protocol.as_str()
            ));
        }
    }
    rules.push(format!("    iifname \"{iface}\" counter drop"));
    rules.push("  }".into());
    rules.push("}".into());
    rules.join("\n") + "\n"
}

fn install_firewall(config: &Config) -> Result<()> {
    // Replace atomically: one transaction deletes the old table (if any) and adds the new one.
    let rules = format!(
        "table inet {TABLE} {{}}\ndelete table inet {TABLE}\n{}",
        firewall(config)
    );
    run(&["nft", "-f", "-"], Some(&rules))
}

pub fn teardown(config: &Config) {
    quiet(&["ip", "link", "del", &config.interface]);
    quiet(&["nft", "delete", "table", "inet", TABLE]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_allows_only_exports() {
        let c = Config::parse(
            &serde_json::json!({
                "localAddress": "169.254.99.1", "peerAddress": "169.254.99.2",
                "peerEndpoint": "peer.example.com:31821", "listenPort": 51822,
                "listeners": [
                    {"name": "vault", "bind": "tunnel", "protocol": "TCP", "listenPort": 20100,
                     "targetHost": "vault.vault.svc.cluster.local", "targetPort": 8200},
                    {"name": "dns", "bind": "tunnel", "protocol": "UDP", "listenPort": 20053,
                     "targetHost": "kube-dns.kube-system.svc.cluster.local", "targetPort": 53},
                    {"name": "remote", "bind": "pod", "protocol": "TCP", "listenPort": 20300,
                     "targetHost": "169.254.99.2", "targetPort": 20300}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let rules = firewall(&c);
        assert!(rules.contains("tcp dport { 20100 } accept"));
        assert!(rules.contains("udp dport { 20053 } accept"));
        assert!(
            !rules.contains("20300"),
            "imports are not reachable from the peer"
        );
        assert!(rules.contains("udp dport 19999 accept"));
        assert!(rules.contains("policy drop"));
    }
}
