use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use nix::errno::Errno;
use nix::sys::socket::sockopt;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, VsockAddr};
use nix::sys::socket::{connect, getsockopt, shutdown, socket};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub const VMADDR_CID_HOST: u32 = 2;

pub struct VsockStream {
    fd: AsyncFd<OwnedFd>,
}

impl VsockStream {
    pub async fn connect(port: u32, cid: u32) -> io::Result<VsockStream> {
        let fd = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )?;
        let addr = VsockAddr::new(cid, port);
        if let Err(e) = connect(fd.as_raw_fd(), &addr) {
            if e != Errno::EINPROGRESS {
                return Err(e.into());
            }
        }
        let fd = AsyncFd::new(fd)?;
        loop {
            let mut guard = fd.writable().await?;
            let so_err = getsockopt(fd.get_ref(), sockopt::SocketError).unwrap_or(0);
            let _ = guard.clear_ready();
            if so_err == 0 {
                return Ok(VsockStream { fd });
            }
            if so_err == Errno::EINPROGRESS as i32 {
                continue;
            }
            return Err(io::Error::from_raw_os_error(so_err));
        }
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = std::task::ready!(self.fd.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match nix::unistd::read(&self.fd, unfilled) {
                Ok(n) => {
                    let _ = guard.clear_ready();
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) => {
                    let err = io::Error::from(e);
                    if err.kind() == io::ErrorKind::WouldBlock {
                        guard.clear_ready();
                        continue;
                    }
                    return Poll::Ready(Err(err));
                }
            }
        }
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = std::task::ready!(self.fd.poll_write_ready(cx))?;
            match nix::unistd::write(&self.fd, buf) {
                Ok(n) => {
                    let _ = guard.clear_ready();
                    return Poll::Ready(Ok(n));
                }
                Err(e) => {
                    let err = io::Error::from(e);
                    if err.kind() == io::ErrorKind::WouldBlock {
                        guard.clear_ready();
                        continue;
                    }
                    return Poll::Ready(Err(err));
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        shutdown(self.fd.as_raw_fd(), nix::sys::socket::Shutdown::Both)?;
        Poll::Ready(Ok(()))
    }
}
