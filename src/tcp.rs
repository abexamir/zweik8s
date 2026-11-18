//! TCP relay. Each session is a pair of sockets; data moves between them with splice(2)
//! through a kernel pipe, so payload bytes never enter user space.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::Interest;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{Semaphore, watch};

use crate::config::{Bind, Config, Listener};
use crate::metrics::ListenerStats;
use crate::warn;

const PIPE_SIZE: usize = 1 << 20;
const COPY_BUFFER: usize = 256 << 10;

static CONGESTION_WARNED: AtomicBool = AtomicBool::new(false);

/// Select the congestion control algorithm (e.g. BBR) for a socket on the tunnel leg.
fn set_congestion(fd: RawFd, name: &str) {
    if name.is_empty() {
        return;
    }
    // SAFETY: TCP_CONGESTION takes a byte string and its length.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONGESTION,
            name.as_ptr() as *const libc::c_void,
            name.len() as libc::socklen_t,
        )
    };
    if rc != 0 && !CONGESTION_WARNED.swap(true, Relaxed) {
        warn!(
            "congestion control not available, using the kernel default",
            algorithm = name,
            error = io::Error::last_os_error()
        );
    }
}

fn tune(stream: &TcpStream, config: &Config) {
    let _ = stream.set_nodelay(true);
    let t = &config.tuning;
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(t.keepalive_idle_seconds))
        .with_interval(Duration::from_secs(t.keepalive_interval_seconds))
        .with_retries(t.keepalive_count);
    let _ = SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

pub fn listen(addr: SocketAddr, listener: &Listener, config: &Config) -> Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    if listener.bind == Bind::Tunnel {
        // Accepted sockets inherit it: the peer-facing leg of an export crosses the tunnel.
        set_congestion(socket.as_raw_fd(), &config.tuning.tcp_congestion);
    }
    socket
        .bind(addr)
        .with_context(|| format!("binding TCP {addr} for {}", listener.name))?;
    Ok(socket.listen(1024)?)
}

/// Accept until shutdown. Sessions already open keep running; the caller drains them.
pub async fn serve(
    listener: TcpListener,
    spec: Listener,
    config: Arc<Config>,
    stats: Arc<ListenerStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let spec = Arc::new(spec);
    let permits = Arc::new(Semaphore::new(config.tuning.max_sessions));
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            accepted = listener.accept() => match accepted {
                Ok((client, _)) => match permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let (spec, config, stats) = (spec.clone(), config.clone(), stats.clone());
                        tokio::spawn(async move {
                            session(client, &spec, &config, &stats).await;
                            drop(permit);
                        });
                    }
                    Err(_) => { stats.rejected.fetch_add(1, Relaxed); }
                },
                Err(e) => {
                    warn!("accept failed", listener = spec.name, error = e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

struct Active<'a>(&'a ListenerStats);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Relaxed);
    }
}

async fn session(client: TcpStream, spec: &Listener, config: &Config, stats: &ListenerStats) {
    stats.accepted.fetch_add(1, Relaxed);
    stats.active.fetch_add(1, Relaxed);
    let _active = Active(stats);
    tune(&client, config);
    let upstream = match connect(spec, config).await {
        Ok(s) => s,
        Err(e) => {
            stats.failed.fetch_add(1, Relaxed);
            warn!(
                "target unreachable",
                listener = spec.name,
                error = format!("{e:#}")
            );
            return;
        }
    };
    tune(&upstream, config);
    let splice = config.tuning.splice;
    let _ = tokio::try_join!(
        pipe(&client, &upstream, &stats.bytes_forward, splice),
        pipe(&upstream, &client, &stats.bytes_reverse, splice),
    );
}

/// Resolve the target for every new session, so Service changes take effect immediately.
async fn connect(spec: &Listener, config: &Config) -> Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(config.tuning.connect_timeout_seconds);
    let target = (spec.target_host.as_str(), spec.target_port);
    let mut addrs: Vec<SocketAddr> =
        tokio::time::timeout_at(deadline.into(), tokio::net::lookup_host(target))
            .await
            .map_err(|_| anyhow!("resolving {} timed out", spec.target_host))?
            .with_context(|| format!("resolving {}", spec.target_host))?
            .collect();
    addrs.sort_by_key(|a| a.is_ipv6()); // IPv4 first; stable, so resolver order is kept within a family
    let mut last = anyhow!("{} has no addresses", spec.target_host);
    for addr in addrs {
        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        if spec.bind == Bind::Pod {
            set_congestion(socket.as_raw_fd(), &config.tuning.tcp_congestion);
        }
        match tokio::time::timeout_at(deadline.into(), socket.connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = anyhow!("connecting to {addr}: {e}"),
            Err(_) => return Err(anyhow!("connecting to {addr} timed out")),
        }
    }
    Err(last)
}

fn shutdown_write(stream: &TcpStream) {
    let _ = SockRef::from(stream).shutdown(std::net::Shutdown::Write);
}

/// One direction of a session. Returns Ok on a clean end of stream (after passing the
/// half-close on), Err on a reset, which ends both directions.
async fn pipe(src: &TcpStream, dst: &TcpStream, counter: &AtomicU64, splice: bool) -> io::Result<()> {
    if splice {
        if let Ok(p) = Pipe::new() {
            match splice_loop(src, dst, counter, &p).await {
                Err(e)
                    if e.raw_os_error() == Some(libc::EINVAL) || e.raw_os_error() == Some(libc::ENOSYS) => {}
                other => return other,
            }
        }
    }
    copy_loop(src, dst, counter).await
}

struct Pipe {
    read: RawFd,
    write: RawFd,
}

impl Pipe {
    fn new() -> io::Result<Pipe> {
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills two descriptors that Pipe then owns.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::fcntl(fds[1], libc::F_SETPIPE_SZ, PIPE_SIZE as libc::c_int) };
        Ok(Pipe {
            read: fds[0],
            write: fds[1],
        })
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

fn splice(from: RawFd, to: RawFd, len: usize) -> io::Result<usize> {
    // SAFETY: both descriptors are open for the duration of the call.
    let n = unsafe {
        libc::splice(
            from,
            std::ptr::null_mut(),
            to,
            std::ptr::null_mut(),
            len,
            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

async fn splice_loop(src: &TcpStream, dst: &TcpStream, counter: &AtomicU64, pipe: &Pipe) -> io::Result<()> {
    let mut moved_any = false;
    loop {
        let filled = loop {
            src.readable().await?;
            match src.try_io(Interest::READABLE, || {
                splice(src.as_raw_fd(), pipe.write, PIPE_SIZE)
            }) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) if !moved_any => return Err(e), // EINVAL here falls back to copying
                Err(e) => return Err(io::Error::new(e.kind(), e.to_string())),
            }
        };
        if filled == 0 {
            shutdown_write(dst);
            return Ok(());
        }
        moved_any = true;
        let mut left = filled;
        while left > 0 {
            dst.writable().await?;
            match dst.try_io(Interest::WRITABLE, || splice(pipe.read, dst.as_raw_fd(), left)) {
                Ok(n) => {
                    left -= n;
                    counter.fetch_add(n as u64, Relaxed);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                // Not EINVAL-shaped any more: data is in flight, so never fall back now.
                Err(e) => return Err(io::Error::new(e.kind(), e.to_string())),
            }
        }
    }
}

async fn copy_loop(src: &TcpStream, dst: &TcpStream, counter: &AtomicU64) -> io::Result<()> {
    let mut buf = vec![0u8; COPY_BUFFER];
    loop {
        src.readable().await?;
        let n = match src.try_read(&mut buf) {
            Ok(0) => {
                shutdown_write(dst);
                return Ok(());
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        let mut off = 0;
        while off < n {
            dst.writable().await?;
            match dst.try_write(&buf[off..n]) {
                Ok(w) => off += w,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        counter.fetch_add(n as u64, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn relay_round_trip(splice: bool) {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut data = Vec::new();
            s.read_to_end(&mut data).await.unwrap();
            s.write_all(&data).await.unwrap();
        });
        let config = Config::parse(
            &serde_json::json!({
            "localAddress": "169.254.99.1", "peerAddress": "169.254.99.2",
            "peerEndpoint": "127.0.0.1:1", "listenPort": 51822, "tuning": {"splice": splice},
            "listeners": [{"name": "echo", "bind": "tunnel", "protocol": "TCP", "listenPort": 20000,
                "targetHost": "127.0.0.1", "targetPort": echo_addr.port()}]})
            .to_string(),
        )
        .unwrap();
        let spec = config.listeners[0].clone();
        let listener = listen("127.0.0.1:0".parse().unwrap(), &spec, &config).unwrap();
        let addr = listener.local_addr().unwrap();
        let stats = ListenerStats::new(&spec);
        let (_tx, rx) = watch::channel(false);
        tokio::spawn(serve(listener, spec, Arc::new(config), stats.clone(), rx));

        let payload: Vec<u8> = (0..8 << 20).map(|i: u32| (i * 7 % 251) as u8).collect();
        let mut c = TcpStream::connect(addr).await.unwrap();
        let (mut r, mut w) = c.split();
        let (_, got) = tokio::join!(
            async {
                w.write_all(&payload).await.unwrap();
                w.shutdown().await.unwrap();
            },
            async {
                let mut got = Vec::new();
                r.read_to_end(&mut got).await.unwrap();
                got
            }
        );
        assert!(got == payload, "payload must come back intact");
        assert_eq!(stats.bytes_forward.load(Relaxed), payload.len() as u64);
        assert_eq!(stats.accepted.load(Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn splice_relay() {
        relay_round_trip(true).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn copy_relay() {
        relay_round_trip(false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_target_counts_failure() {
        let config = Config::parse(
            &serde_json::json!({
            "localAddress": "169.254.99.1", "peerAddress": "169.254.99.2",
            "peerEndpoint": "127.0.0.1:1", "listenPort": 51822,
            "listeners": [{"name": "gone", "bind": "tunnel", "protocol": "TCP", "listenPort": 20000,
                "targetHost": "127.0.0.1", "targetPort": 9}]})
            .to_string(),
        )
        .unwrap();
        let spec = config.listeners[0].clone();
        let listener = listen("127.0.0.1:0".parse().unwrap(), &spec, &config).unwrap();
        let addr = listener.local_addr().unwrap();
        let stats = ListenerStats::new(&spec);
        let (_tx, rx) = watch::channel(false);
        tokio::spawn(serve(listener, spec, Arc::new(config), stats.clone(), rx));
        let mut c = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(c.read(&mut buf).await.unwrap_or(0), 0);
        assert_eq!(stats.failed.load(Relaxed), 1);
    }
}
