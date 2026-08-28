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

pub struct VsockListener {
    fd: OwnedFd,
}

impl VsockListener {
    pub fn bind(port: u32) -> io::Result<Self> {
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let addr = make_addr(port, libc::VMADDR_CID_ANY);
            if libc::bind(
                fd,
                (&addr as *const libc::sockaddr_vm).cast(),
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            ) < 0
            {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            if libc::listen(fd, 16) < 0 {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            Ok(Self {
                fd: OwnedFd::from_raw_fd(fd),
            })
        }
    }

    pub async fn accept(&self) -> io::Result<VsockStream> {
        let listener = AsyncFd::new(self.fd.try_clone()?)?;
        loop {
            let conn = unsafe {
                libc::accept4(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if conn >= 0 {
                return Ok(VsockStream {
                    fd: AsyncFd::new(unsafe { OwnedFd::from_raw_fd(conn) })?,
                });
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::WouldBlock {
                return Err(err);
            }
            listener.readable().await?;
        }
    }
}

pub struct VsockStream {
    fd: AsyncFd<OwnedFd>,
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
