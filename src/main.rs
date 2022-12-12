//! zweik8s: connect selected Services of two Kubernetes clusters over one WireGuard tunnel.

mod config;
mod health;
mod logging;
mod metrics;
mod mmsg;
mod tcp;
mod transport;
mod udp;
mod wg;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use config::{Bind, Config, Protocol};
use metrics::{ListenerStats, Stats};

const USAGE: &str = "\
zweik8s: connect Services of two Kubernetes clusters over one WireGuard tunnel

Usage:
  zweik8s [run] [--config PATH]     run the gateway (default config /config/runtime.json)
  zweik8s check PATH                validate a runtime config
  zweik8s probe PATH [--addr ADDR]  query the local health server, e.g. /readyz
  zweik8s status [--addr ADDR]      print the gateway status as JSON
  zweik8s version
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let addr: SocketAddr = flag("--addr")
        .and_then(|a| a.parse().ok())
        .unwrap_or(([127, 0, 0, 1], 9090).into());
    let result = match args.first().map(String::as_str) {
        None | Some("run") | Some("--config") => {
            let path = flag("--config")
                .or_else(|| std::env::var("ZWEIK8S_CONFIG").ok())
                .unwrap_or_else(|| "/config/runtime.json".into());
            run(PathBuf::from(path))
        }
        Some("check") => match args.get(1) {
            Some(p) => Config::load(p.as_ref()).map(|c| println!("ok: {} listeners", c.listeners.len())),
            None => Err(anyhow::anyhow!("check needs a path")),
        },
        Some("probe") => health::probe(addr, args.get(1).map_or("/readyz", String::as_str)).map(|_| ()),
        Some("status") => health::probe(addr, "/status").map(|b| print!("{b}")),
        Some("version") | Some("--version") => {
            println!("zweik8s {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help") | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown command {other}\n\n{USAGE}")),
    };
    if let Err(e) = result {
        error!("fatal", error = format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run(path: PathBuf) -> Result<()> {
    let config = Arc::new(Config::load(&path)?);
    let workers = match config.tuning.workers {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .max(2),
        n => n,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("zweik8s")
        .enable_all()
        .build()?;
    info!(
        "starting",
        version = env!("CARGO_PKG_VERSION"),
        workers = workers,
        local = config.local_address,
        peer = config.peer_endpoint,
        listeners = config.listeners.len()
    );
    let result = runtime.block_on(gateway(config.clone()));
    wg::teardown(&config);
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn gateway(config: Arc<Config>) -> Result<()> {
    let stats = Stats::new(&config);
    let transport = transport::Transport::bind(config.clone(), stats.clone()).await?;
    let public_key = {
        let (config, port) = (config.clone(), transport.bridge_port());
        tokio::task::spawn_blocking(move || wg::setup(&config, port)).await??
    };
    let mut tasks = transport.start();
    let watchdog = tokio::spawn(transport.clone().watchdog(public_key));

    let pod_ip: IpAddr = match std::env::var("POD_IP") {
        Ok(ip) => ip.parse().context("POD_IP")?,
        Err(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    for spec in config.listeners.iter().cloned() {
        let ip = match spec.bind {
            Bind::Pod => pod_ip,
            Bind::Tunnel => IpAddr::V4(config.local_address),
        };
        let addr = SocketAddr::new(ip, spec.listen_port);
        let s = ListenerStats::new(&spec);
        stats.add_listener(s.clone());
        let (config, rx) = (config.clone(), stop_rx.clone());
        info!(
            "listening",
            listener = spec.name,
            protocol = spec.protocol.as_str(),
            address = addr,
            target = format!("{}:{}", spec.target_host, spec.target_port)
        );
        tasks.push(match spec.protocol {
            Protocol::Tcp => {
                let l = tcp::listen(addr, &spec, &config)?;
                tokio::spawn(tcp::serve(l, spec, config, s, rx))
            }
            Protocol::Udp => {
                let sock = udp::bind(addr, &spec).await?;
                tokio::spawn(udp::serve(sock, spec, config, s, rx))
            }
        });
    }
    let health = tokio::spawn(health::serve(config.health.bind, stats.clone()));
    stats.ready.store(true, Relaxed);
    info!("ready");

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let outcome = tokio::select! {
        _ = term.recv() => Ok(()),
        _ = int.recv() => Ok(()),
        r = watchdog => match r { Ok(Err(e)) => Err(e), _ => Err(anyhow::anyhow!("watchdog stopped")) },
        r = health => match r { Ok(Err(e)) => Err(e), _ => Err(anyhow::anyhow!("health server stopped")) },
    };
    stats.ready.store(false, Relaxed);
    let _ = stop_tx.send(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.tuning.drain_seconds);
    while stats.active_sessions() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    info!("stopping", open_sessions = stats.active_sessions());
    for t in tasks {
        t.abort();
    }
    if let Err(e) = &outcome {
        bail!("{e:#}");
    }
    Ok(())
}
