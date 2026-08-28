use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

fn make_addr(port: u32, cid: u32) -> libc::sockaddr_vm {
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = port;
    addr.svm_cid = cid;
    addr
}

pub struct VsockStream {
    fd: AsyncFd<OwnedFd>,
}

impl VsockStream {
    pub async fn connect(port: u32, cid: u32) -> io::Result<VsockStream> {
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            if libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let addr = make_addr(port, cid);
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_vm).cast(),
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINPROGRESS)
                && err.kind() != io::ErrorKind::WouldBlock
            {
                return Err(err);
            }
        }
        let fd = AsyncFd::new(fd)?;
        loop {
            let mut guard = fd.writable().await?;
            let mut so_err: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut so_err as *mut libc::c_int).cast(),
                    &mut len,
                );
            }
            let _ = guard.clear_ready();
            if so_err == 0 {
                return Ok(VsockStream { fd });
            }
            let err = io::Error::from_raw_os_error(so_err);
            if so_err == libc::EINPROGRESS {
                continue;
            }
            return Err(err);
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
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    unfilled.as_mut_ptr().cast(),
                    unfilled.len(),
                )
            };
            if n >= 0 {
                let n = n as usize;
                let _ = guard.clear_ready();
                buf.advance(n);
                return Poll::Ready(Ok(()));
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Poll::Ready(Err(err));
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
            let n = unsafe { libc::write(self.fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
            if n >= 0 {
                let _ = guard.clear_ready();
                return Poll::Ready(Ok(n as usize));
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Poll::Ready(Err(err));
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let r = unsafe { libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR) };
        if r < 0 {
            return Poll::Ready(Err(io::Error::last_os_error()));
        }
        Poll::Ready(Ok(()))
    }
}
