//! Tiny HTTP server: /livez, /readyz, /status (JSON) and /metrics (Prometheus).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::metrics::Stats;

pub async fn serve(addr: SocketAddr, stats: Arc<Stats>) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding health server {addr}"))?;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let stats = stats.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(3), handle(stream, &stats)).await;
        });
    }
}

async fn handle(mut stream: TcpStream, stats: &Stats) -> std::io::Result<()> {
    let mut req = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 || req.len() > 8192 {
            return Ok(());
        }
        req.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&req);
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let ready = stats.ready.load(Relaxed);
    let (status, kind, body) = match path.split('?').next().unwrap_or("/") {
        "/livez" => ("200 OK", "text/plain", "ok\n".to_string()),
        "/readyz" if ready => ("200 OK", "text/plain", "ready\n".to_string()),
        "/readyz" => ("503 Service Unavailable", "text/plain", "not ready\n".to_string()),
        "/status" => ("200 OK", "application/json", stats.status_json()),
        "/metrics" => ("200 OK", "text/plain; version=0.0.4", stats.prometheus()),
        _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

/// Exec-probe client: `zweik8s probe /readyz`. Exits non-zero unless the answer is 200.
pub fn probe(addr: SocketAddr, path: &str) -> Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    anyhow::ensure!(
        head.starts_with("HTTP/1.1 200"),
        "{}",
        head.lines().next().unwrap_or("no response")
    );
    Ok(body.to_string())
}
