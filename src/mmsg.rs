//! Batched UDP I/O with recvmmsg(2)/sendmmsg(2): one syscall moves up to BATCH datagrams.

use std::io;
use std::mem::{MaybeUninit, zeroed};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::RawFd;

/// An IPv4 destination in the kernel's layout.
#[derive(Clone, Copy)]
pub struct Dest(libc::sockaddr_in);

impl Dest {
    pub fn new(addr: SocketAddrV4) -> Dest {
        // SAFETY: sockaddr_in is plain old data.
        let mut sin: libc::sockaddr_in = unsafe { zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = addr.port().to_be();
        sin.sin_addr = libc::in_addr {
            s_addr: u32::from(*addr.ip()).to_be(),
        };
        Dest(sin)
    }
}

pub const BATCH: usize = 64;
/// Larger than any WireGuard packet for an MTU up to 1420; bigger datagrams are dropped.
pub const DATAGRAM: usize = 2048;

pub struct Batch {
    buf: Box<[u8]>,
    pub lens: [usize; BATCH],
    addrs: Box<[libc::sockaddr_storage]>,
    pub count: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Batch {
            buf: vec![0u8; BATCH * DATAGRAM].into_boxed_slice(),
            lens: [0; BATCH],
            // SAFETY: sockaddr_storage is plain old data; all-zero is a valid value.
            addrs: (0..BATCH).map(|_| unsafe { zeroed() }).collect(),
            count: 0,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Batch {
    pub fn packet(&self, i: usize) -> &[u8] {
        &self.buf[i * DATAGRAM..i * DATAGRAM + self.lens[i]]
    }

    pub fn source(&self, i: usize) -> Option<SocketAddr> {
        let s = &self.addrs[i];
        if s.ss_family as i32 != libc::AF_INET {
            return None;
        }
        // SAFETY: the family says the storage holds a sockaddr_in.
        let sin = unsafe { &*(s as *const libc::sockaddr_storage as *const libc::sockaddr_in) };
        Some(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
            u16::from_be(sin.sin_port),
        )))
    }

    pub fn bytes(&self) -> u64 {
        self.lens[..self.count].iter().map(|&l| l as u64).sum()
    }

    /// Receive up to BATCH datagrams without blocking. Truncated datagrams are discarded.
    pub fn recv(&mut self, fd: RawFd) -> io::Result<usize> {
        let mut iovs: [MaybeUninit<libc::iovec>; BATCH] = [const { MaybeUninit::uninit() }; BATCH];
        let mut hdrs: [libc::mmsghdr; BATCH] = unsafe { zeroed() };
        let base = self.buf.as_mut_ptr();
        for i in 0..BATCH {
            iovs[i].write(libc::iovec {
                iov_base: unsafe { base.add(i * DATAGRAM) } as *mut libc::c_void,
                iov_len: DATAGRAM,
            });
            hdrs[i].msg_hdr.msg_iov = iovs[i].as_mut_ptr();
            hdrs[i].msg_hdr.msg_iovlen = 1;
            hdrs[i].msg_hdr.msg_name = &mut self.addrs[i] as *mut _ as *mut libc::c_void;
            hdrs[i].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        }
        // SAFETY: every header points at memory owned by self/iovs that outlives the call.
        let n = unsafe {
            libc::recvmmsg(
                fd,
                hdrs.as_mut_ptr(),
                BATCH as u32,
                libc::MSG_DONTWAIT as _,
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut kept = 0;
        for i in 0..n as usize {
            if hdrs[i].msg_hdr.msg_flags & libc::MSG_TRUNC != 0 {
                continue;
            }
            if kept != i {
                self.buf.copy_within(
                    i * DATAGRAM..i * DATAGRAM + hdrs[i].msg_len as usize,
                    kept * DATAGRAM,
                );
                self.addrs[kept] = self.addrs[i];
            }
            self.lens[kept] = hdrs[i].msg_len as usize;
            kept += 1;
        }
        self.count = kept;
        Ok(kept)
    }

    /// Send packets `from..count` to `dest` without blocking; returns how many were sent.
    pub fn send(&self, fd: RawFd, dest: &Dest, from: usize) -> io::Result<usize> {
        let todo = self.count - from;
        if todo == 0 {
            return Ok(0);
        }
        let mut iovs: [MaybeUninit<libc::iovec>; BATCH] = [const { MaybeUninit::uninit() }; BATCH];
        let mut hdrs: [libc::mmsghdr; BATCH] = unsafe { zeroed() };
        for (j, i) in (from..self.count).enumerate() {
            iovs[j].write(libc::iovec {
                iov_base: self.buf[i * DATAGRAM..].as_ptr() as *mut libc::c_void,
                iov_len: self.lens[i],
            });
            hdrs[j].msg_hdr.msg_iov = iovs[j].as_mut_ptr();
            hdrs[j].msg_hdr.msg_iovlen = 1;
            hdrs[j].msg_hdr.msg_name = &dest.0 as *const libc::sockaddr_in as *mut libc::c_void;
            hdrs[j].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        }
        // SAFETY: as in recv; the kernel only reads these buffers.
        let n = unsafe { libc::sendmmsg(fd, hdrs.as_mut_ptr(), todo as u32, libc::MSG_DONTWAIT as _) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;

    #[test]
    fn round_trip_batch() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0..10u8 {
            a.send_to(&[i; 100], b.local_addr().unwrap()).unwrap();
        }
        a.send_to(&vec![7u8; DATAGRAM + 10], b.local_addr().unwrap())
            .unwrap(); // truncated: dropped
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut batch = Batch::default();
        assert_eq!(batch.recv(b.as_raw_fd()).unwrap(), 10);
        assert_eq!(batch.packet(3), &[3u8; 100]);
        assert_eq!(batch.source(0), Some(a.local_addr().unwrap()));
        assert_eq!(batch.bytes(), 1000);
        let SocketAddr::V4(a4) = a.local_addr().unwrap() else {
            unreachable!()
        };
        let dest = Dest::new(a4);
        assert_eq!(batch.send(b.as_raw_fd(), &dest, 4).unwrap(), 6);
        let mut buf = [0u8; 200];
        let (n, _) = a.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[4u8; 100]);
    }
}
