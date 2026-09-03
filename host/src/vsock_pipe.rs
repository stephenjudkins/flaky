//! In-memory bounded byte pipe connecting the vsock device worker (sync,
//! running under alioth's mio event loop) and the host-side async consumer
//! (tokio). Replaces the socketpair: backpressure comes from the bounded
//! buffers, and device-side readiness comes from the `WakeFn` hook (an
//! alioth `Notifier` ping) fired on every host-side transition the device
//! cares about: data written by the host, space freed by the host, or host
//! close/EOF.
//!
//! Two independent directions (like a socketpair): `to_host` carries
//! guest->host bytes (device writes, stream reads), `to_dev` carries
//! host->guest bytes (stream writes, device reads).

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(crate) type WakeFn = Arc<dyn Fn() + Send + Sync>;

struct Half {
    buf: VecDeque<u8>,
    capacity: usize,
    writer_closed: bool,
    reader_closed: bool,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

impl Half {
    fn new(capacity: usize) -> Self {
        Half {
            buf: VecDeque::new(),
            capacity,
            writer_closed: false,
            reader_closed: false,
            read_waker: None,
            write_waker: None,
        }
    }
}

struct PipeShared {
    to_host: Mutex<Half>,
    to_dev: Mutex<Half>,
    wake_device: WakeFn,
}

pub(crate) struct VsockPipe;

impl VsockPipe {
    pub(crate) fn pair(capacity: usize, wake_device: WakeFn) -> (PipeDevEnd, VsockStream) {
        let shared = Arc::new(PipeShared {
            to_host: Mutex::new(Half::new(capacity)),
            to_dev: Mutex::new(Half::new(capacity)),
            wake_device,
        });
        (
            PipeDevEnd {
                shared: shared.clone(),
            },
            VsockStream { shared },
        )
    }
}

pub(crate) struct PipeDevEnd {
    shared: Arc<PipeShared>,
}

impl fmt::Debug for PipeDevEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeDevEnd").finish()
    }
}

impl Drop for PipeDevEnd {
    fn drop(&mut self) {
        let (read_waker, write_waker) = {
            let mut to_host = self.shared.to_host.lock().unwrap();
            to_host.writer_closed = true;
            let w = to_host.read_waker.take();
            drop(to_host);
            let mut to_dev = self.shared.to_dev.lock().unwrap();
            to_dev.reader_closed = true;
            (w, to_dev.write_waker.take())
        };
        if let Some(w) = read_waker {
            w.wake();
        }
        if let Some(w) = write_waker {
            w.wake();
        }
    }
}

impl PipeDevEnd {
    /// Reads host->guest data into one buffer. `Ok(0)` means EOF (host
    /// closed and drained); an empty pipe returns `WouldBlock`.
    pub(crate) fn read_slice(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (out, write_waker) = {
            let mut to_dev = self.shared.to_dev.lock().unwrap();
            if to_dev.buf.is_empty() {
                return if to_dev.writer_closed {
                    Ok(0)
                } else {
                    Err(io::Error::new(ErrorKind::WouldBlock, "vsock pipe empty"))
                };
            }
            let n = buf.len().min(to_dev.buf.len());
            let (front, back) = to_dev.buf.as_slices();
            let fn_ = n.min(front.len());
            buf[..fn_].copy_from_slice(&front[..fn_]);
            buf[fn_..n].copy_from_slice(&back[..n - fn_]);
            if n == to_dev.buf.len() {
                to_dev.buf.clear();
            } else {
                to_dev.buf.drain(..n);
            }
            (n, to_dev.write_waker.take())
        };
        if let Some(w) = write_waker {
            w.wake();
        }
        Ok(out)
    }

    pub(crate) fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (n, read_waker) = {
            let mut to_host = self.shared.to_host.lock().unwrap();
            if to_host.reader_closed {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "vsock stream closed"));
            }
            let space = to_host.capacity - to_host.buf.len();
            if space == 0 {
                return Err(io::Error::new(ErrorKind::WouldBlock, "vsock pipe full"));
            }
            let n = space.min(buf.len());
            to_host.buf.extend(&buf[..n]);
            (n, to_host.read_waker.take())
        };
        if let Some(w) = read_waker {
            w.wake();
        }
        Ok(n)
    }

    /// Whether the device should attempt a transfer: buffered host->guest
    /// data, or host EOF that still needs to be delivered as a SHUTDOWN to
    /// the guest.
    pub(crate) fn is_readable(&self) -> bool {
        let to_dev = self.shared.to_dev.lock().unwrap();
        !to_dev.buf.is_empty() || to_dev.writer_closed
    }
}

pub struct VsockStream {
    shared: Arc<PipeShared>,
}

impl fmt::Debug for VsockStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VsockStream").finish()
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        {
            let mut to_host = self.shared.to_host.lock().unwrap();
            to_host.reader_closed = true;
            let mut to_dev = self.shared.to_dev.lock().unwrap();
            to_dev.writer_closed = true;
        }
        (self.shared.wake_device)();
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        rbuf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut to_host = self.shared.to_host.lock().unwrap();
        if !to_host.buf.is_empty() {
            let n = rbuf.remaining().min(to_host.buf.len());
            let (front, back) = to_host.buf.as_slices();
            let fn_ = n.min(front.len());
            rbuf.put_slice(&front[..fn_]);
            rbuf.put_slice(&back[..n - fn_]);
            if n == to_host.buf.len() {
                to_host.buf.clear();
            } else {
                to_host.buf.drain(..n);
            }
            drop(to_host);
            // space freed: the device may have guest data pending
            (self.shared.wake_device)();
            Poll::Ready(Ok(()))
        } else if to_host.writer_closed {
            Poll::Ready(Ok(()))
        } else {
            // waker registered under the same lock the writer takes,
            // so a write between the emptiness check and now cannot be lost
            to_host.read_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut to_dev = self.shared.to_dev.lock().unwrap();
        if to_dev.reader_closed || to_dev.writer_closed {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "vsock peer closed",
            )));
        }
        let space = to_dev.capacity - to_dev.buf.len();
        if space == 0 {
            to_dev.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = space.min(buf.len());
        to_dev.buf.extend(&buf[..n]);
        drop(to_dev);
        (self.shared.wake_device)();
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        {
            let mut to_dev = self.shared.to_dev.lock().unwrap();
            to_dev.writer_closed = true;
        }
        (self.shared.wake_device)();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests;
