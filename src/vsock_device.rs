use std::collections::HashMap;
use std::io::{self, ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::num::Wrapping;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alioth::hv::IoeventFd;
use alioth::mem::mapped::RamBus;
use alioth::sync::notifier::Notifier;
use alioth::virtio::dev::vsock::{VsockConfig, VsockFeature};
use alioth::virtio::dev::{DevParam, Virtio, WakeEvent};
use alioth::virtio::queue::{DescChain, Queue, QueueReg, Status, VirtQueue};
use alioth::virtio::worker::mio::{ActiveMio, Mio, VirtioMio};
use alioth::virtio::{DeviceId, IrqSender, VirtioFeature};
use mio::event::Event;
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

type Result<T> = std::result::Result<T, alioth::virtio::Error>;

const FEATURE_BUILT_IN: u128 = VirtioFeature::EVENT_IDX.bits()
    | VirtioFeature::RING_PACKED.bits()
    | VirtioFeature::VERSION_1.bits();

const CID_HOST: u32 = 2;
const QUEUE_RX: u16 = 0;
const QUEUE_TX: u16 = 1;
const HDR_SIZE: usize = 44;
const TYPE_STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;
const SHUTDOWN_RECEIVE: u32 = 1;
const SHUTDOWN_SEND: u32 = 2;
const SHUTDOWN_BOTH: u32 = SHUTDOWN_RECEIVE | SHUTDOWN_SEND;
const BUF_ALLOC: u32 = 64 * 1024;

#[derive(Debug, Clone, Copy, Default)]
struct Hdr {
    src_cid: u32,
    dst_cid: u32,
    src_port: u32,
    dst_port: u32,
    len: u32,
    type_: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Hdr {
    fn to_bytes(self) -> [u8; HDR_SIZE] {
        let mut b = [0u8; HDR_SIZE];
        b[0..4].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..12].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.type_.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < HDR_SIZE {
            return None;
        }
        let u32_at = |i: usize| {
            let mut w = [0u8; 4];
            w.copy_from_slice(&b[i..i + 4]);
            u32::from_le_bytes(w)
        };
        let u16_at = |i: usize| {
            let mut w = [0u8; 2];
            w.copy_from_slice(&b[i..i + 2]);
            u16::from_le_bytes(w)
        };
        Some(Hdr {
            src_cid: u32_at(0),
            dst_cid: u32_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            type_: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        })
    }
}

fn set_flags(fd: RawFd) -> io::Result<()> {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd_flags = libc::fcntl(fd, libc::F_GETFD);
        if fd_flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn socketpair() -> io::Result<(UnixStream, UnixStream)> {
    let mut fds = [0 as RawFd; 2];
    unsafe {
        if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    set_flags(fds[0])?;
    set_flags(fds[1])?;
    Ok(unsafe {
        (
            UnixStream::from_raw_fd(fds[0]),
            UnixStream::from_raw_fd(fds[1]),
        )
    })
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    set_flags(fds[0])?;
    set_flags(fds[1])?;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[derive(Debug)]
enum ConnState {
    Requested,
    Established { fwd_cnt: Wrapping<u32> },
    Shutdown { flags: u32 },
}

#[derive(Debug)]
struct Connection {
    state: ConnState,
    reader: UnixStream,
    writer: UnixStream,
}

impl Connection {
    fn of(stream: UnixStream) -> io::Result<Self> {
        Ok(Connection {
            state: ConnState::Requested,
            writer: stream.try_clone()?,
            reader: stream,
        })
    }
}

#[derive(Debug)]
struct Shared {
    pending: Mutex<Vec<(u32, UnixStream)>>,
    wake_write: OwnedFd,
}

#[derive(Clone, Debug)]
pub struct VsockHost {
    shared: Arc<Shared>,
}

impl VsockHost {
    pub fn connect(&self, port: u32) -> io::Result<UnixStream> {
        let (host_end, dev_end) = socketpair()?;
        self.shared.pending.lock().unwrap().push((port, dev_end));
        let byte = [1u8];
        unsafe {
            let _ = libc::write(self.shared.wake_write.as_raw_fd(), byte.as_ptr().cast(), 1);
        }
        Ok(host_end)
    }
}

#[derive(Debug)]
pub struct VsockParam {
    cid: u32,
    shared: Arc<Shared>,
    wake_read: OwnedFd,
}

impl VsockParam {
    pub fn new(cid: u32) -> io::Result<(Self, VsockHost)> {
        let (wake_read, wake_write) = pipe()?;
        let shared = Arc::new(Shared {
            pending: Mutex::new(Vec::new()),
            wake_write,
        });
        Ok((
            VsockParam {
                cid,
                shared: shared.clone(),
                wake_read,
            },
            VsockHost { shared },
        ))
    }
}

impl DevParam for VsockParam {
    type Device = InProcessVsock;

    fn build(self, name: impl Into<Arc<str>>) -> Result<Self::Device> {
        Ok(InProcessVsock {
            name: name.into(),
            config: Arc::new(VsockConfig {
                guest_cid: self.cid,
                guest_cid_hi: 0,
            }),
            guest_cid: self.cid,
            shared: self.shared,
            wake_read: self.wake_read,
            connections: HashMap::new(),
            ports: HashMap::new(),
            host_ports: HashMap::new(),
            next_port: 1024,
        })
    }
}

#[derive(Debug)]
pub struct InProcessVsock {
    name: Arc<str>,
    config: Arc<VsockConfig>,
    guest_cid: u32,
    shared: Arc<Shared>,
    wake_read: OwnedFd,
    connections: HashMap<(u32, u32), Connection>,
    ports: HashMap<Token, (u32, u32)>,
    host_ports: HashMap<u32, u32>,
    next_port: u32,
}

impl InProcessVsock {
    fn allocate_port(&mut self) -> Option<u32> {
        let mut count: u64 = 0;
        while self.host_ports.contains_key(&self.next_port) && count < u32::MAX as u64 {
            self.next_port = self.next_port.wrapping_add(1);
            count += 1;
        }
        if count == u32::MAX as u64 {
            None
        } else {
            Some(self.next_port)
        }
    }

    fn drain_wake_pipe(&self) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe {
                libc::read(
                    self.wake_read.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n < buf.len() as isize {
                break;
            }
        }
    }

    fn process_pending<'m, Q, S>(
        &mut self,
        registry: &Registry,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        loop {
            let pending = self.shared.pending.lock().unwrap().pop();
            let Some((guest_port, dev_end)) = pending else {
                break;
            };
            let Some(host_port) = self.allocate_port() else {
                log::error!("{}: failed to allocate host port", self.name);
                continue;
            };
            let token = Token(dev_end.as_raw_fd() as usize);
            if let Err(e) = registry.register(
                &mut SourceFd(&dev_end.as_raw_fd()),
                token,
                Interest::READABLE,
            ) {
                log::error!("{}: failed to register socket: {e}", self.name);
                continue;
            }
            let hdr = Hdr {
                src_cid: CID_HOST,
                dst_cid: self.guest_cid,
                src_port: host_port,
                dst_port: guest_port,
                type_: TYPE_STREAM,
                op: OP_REQUEST,
                buf_alloc: BUF_ALLOC,
                ..Default::default()
            };
            if let Err(e) = self.respond(&hdr, irq_sender, rx_q) {
                log::error!("{}: failed to send connection request: {e:?}", self.name);
                let _ = registry.deregister(&mut SourceFd(&dev_end.as_raw_fd()));
                continue;
            }
            match Connection::of(dev_end) {
                Ok(conn) => {
                    self.connections.insert((host_port, guest_port), conn);
                    self.ports.insert(token, (host_port, guest_port));
                    *self.host_ports.entry(host_port).or_default() += 1;
                }
                Err(e) => {
                    log::error!("{}: failed to init connection: {e:?}", self.name);
                }
            }
        }
        Ok(())
    }

    fn respond<'m, Q, S>(
        &mut self,
        hdr: &Hdr,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let bytes = hdr.to_bytes();
        let name = &self.name;
        let mut sent = false;
        rx_q.handle_desc(QUEUE_RX, irq_sender, |desc| {
            if sent {
                return Ok(Status::Break);
            }
            write_prefix(&mut desc.writable, &bytes);
            if !write_prefix_satisfied(&mut desc.writable, &bytes) {
                log::error!("{name}: no buffer space for op {}", hdr.op);
                return Ok(Status::Break);
            }
            sent = true;
            Ok(Status::Done {
                len: HDR_SIZE as u32,
            })
        })?;
        if !sent {
            log::error!("{}: no rx buffers for op {}", self.name, hdr.op);
        }
        Ok(())
    }

    fn respond_rst<'m, Q, S>(
        &mut self,
        hdr: &Hdr,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let rst = Hdr {
            src_cid: CID_HOST,
            dst_cid: self.guest_cid,
            src_port: hdr.dst_port,
            dst_port: hdr.src_port,
            type_: hdr.type_,
            op: OP_RST,
            ..Default::default()
        };
        self.respond(&rst, irq_sender, rx_q)
    }

    fn remove_conn(&mut self, host_port: u32, guest_port: u32, registry: &Registry) -> Result<()> {
        let Some(conn) = self.connections.remove(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let token = Token(conn.reader.as_raw_fd() as usize);
        self.ports.remove(&token);
        if let Some(count) = self.host_ports.get_mut(&host_port) {
            if *count == 1 {
                self.host_ports.remove(&host_port);
            } else {
                *count -= 1;
            }
        }
        registry.deregister(&mut SourceFd(&conn.reader.as_raw_fd()))?;
        Ok(())
    }

    fn handle_tx_response<'m, Q, S>(
        &mut self,
        hdr: &Hdr,
        registry: &Registry,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let (host_port, guest_port) = (hdr.dst_port, hdr.src_port);
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        match conn.state {
            ConnState::Requested => {
                conn.state = ConnState::Established {
                    fwd_cnt: Wrapping(0),
                };
                log::debug!(
                    "{}: vm:{guest_port} -> host:{host_port}: established",
                    self.name
                );
            }
            ref other => {
                log::error!(
                    "{}: vm:{guest_port} -> host:{host_port}: found {other:?}, expect Requested",
                    self.name
                );
                return Ok(());
            }
        }
        self.transfer_rx_data(host_port, guest_port, registry, rx_q, irq_sender)
    }

    fn handle_tx_shutdown(&mut self, hdr: &Hdr, registry: &Registry) -> Result<()> {
        let (host_port, guest_port) = (hdr.dst_port, hdr.src_port);
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let mut flags = if let ConnState::Shutdown { flags } = conn.state {
            flags
        } else {
            0
        };
        flags |= hdr.flags & SHUTDOWN_BOTH;
        if flags != SHUTDOWN_BOTH {
            conn.state = ConnState::Shutdown { flags };
        } else {
            self.remove_conn(host_port, guest_port, registry)?;
        }
        Ok(())
    }

    fn handle_tx_desc<'m, Q, S>(
        &mut self,
        desc: &mut DescChain<'_>,
        registry: &Registry,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let Some(raw) = gather::<HDR_SIZE>(&desc.readable) else {
            log::error!("{}: short header from guest", self.name);
            return Ok(());
        };
        let Some(hdr) = Hdr::from_bytes(&raw) else {
            log::error!("{}: malformed header from guest", self.name);
            return Ok(());
        };
        log::debug!(
            "{}: tx: vm:{} -> host:{} op {}",
            self.name,
            hdr.src_port,
            hdr.dst_port,
            hdr.op
        );
        match hdr.op {
            OP_RESPONSE => self.handle_tx_response(&hdr, registry, rx_q, irq_sender)?,
            OP_RST => {
                self.remove_conn(hdr.dst_port, hdr.src_port, registry)?;
            }
            OP_SHUTDOWN => self.handle_tx_shutdown(&hdr, registry)?,
            OP_RW => self.transfer_tx_data(&hdr, &desc.readable)?,
            OP_REQUEST => {
                log::warn!("{}: guest-initiated connections unsupported", self.name);
                self.respond_rst(&hdr, irq_sender, rx_q)?;
            }
            OP_CREDIT_UPDATE | OP_CREDIT_REQUEST => {}
            other => log::error!("{}: unsupported op {other}", self.name),
        }
        Ok(())
    }

    fn transfer_tx_data(&mut self, hdr: &Hdr, bufs: &[IoSlice]) -> Result<()> {
        let (host_port, guest_port) = (hdr.dst_port, hdr.src_port);
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let ConnState::Established { fwd_cnt } = &mut conn.state else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: invalid state",
                self.name
            );
            return Ok(());
        };
        let mut skip = HDR_SIZE;
        let mut remain = hdr.len as usize;
        for buf in bufs {
            if remain == 0 {
                break;
            }
            let mut buf: &[u8] = buf;
            if skip > 0 {
                let n = skip.min(buf.len());
                buf = &buf[n..];
                skip -= n;
                if buf.is_empty() {
                    continue;
                }
            }
            let n = remain.min(buf.len());
            if let Err(e) = conn.writer.write_all(&buf[..n]) {
                log::error!("{}: write host socket: {e}", self.name);
                break;
            }
            remain -= n;
        }
        if remain > 0 {
            log::error!("{}: missing {remain} bytes", self.name);
        }
        *fwd_cnt += Wrapping(hdr.len - remain as u32);
        Ok(())
    }

    fn transfer_rx_data<'m, Q, S>(
        &mut self,
        host_port: u32,
        guest_port: u32,
        registry: &Registry,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let mut send_shutdown = false;
        {
            let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
                log::warn!(
                    "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                    self.name
                );
                return Ok(());
            };
            let fwd_cnt = match &conn.state {
                ConnState::Established { fwd_cnt } => fwd_cnt.0,
                ref other => {
                    log::debug!("{}: unexpected state {other:?}", self.name);
                    return Ok(());
                }
            };
            let rw = Hdr {
                src_cid: CID_HOST,
                dst_cid: self.guest_cid,
                src_port: host_port,
                dst_port: guest_port,
                type_: TYPE_STREAM,
                op: OP_RW,
                fwd_cnt,
                buf_alloc: BUF_ALLOC,
                ..Default::default()
            };
            let name = &self.name;
            let reader = &mut conn.reader;
            rx_q.handle_desc(QUEUE_RX, irq_sender, |desc| {
                if send_shutdown {
                    return Ok(Status::Break);
                }
                let (nread, eof, fits) = fill_rx_bufs(&mut desc.writable, reader, name)?;
                if !fits {
                    log::error!("{name}: no buffer space for RW");
                    return Ok(Status::Break);
                }
                if nread == 0 {
                    if eof {
                        send_shutdown = true;
                        write_shut_hdr(&mut desc.writable, host_port, guest_port);
                        return Ok(Status::Done {
                            len: HDR_SIZE as u32,
                        });
                    }
                    return Ok(Status::Break);
                }
                let hdr = Hdr {
                    len: nread as u32,
                    ..rw
                };
                write_prefix(&mut desc.writable, &hdr.to_bytes());
                Ok(Status::Done {
                    len: (nread + HDR_SIZE) as u32,
                })
            })?;
        }
        log::debug!(
            "{}: vm:{guest_port} -> host:{host_port}: rx done",
            self.name
        );
        if send_shutdown {
            log::debug!(
                "{}: vm:{guest_port} -> host:{host_port}: host eof, shutdown",
                self.name
            );
            let _ = self.remove_conn(host_port, guest_port, registry);
        }
        Ok(())
    }
}

fn write_prefix(bufs: &mut [IoSliceMut], bytes: &[u8]) {
    let mut remaining = bytes;
    for buf in bufs.iter_mut() {
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        remaining = &remaining[n..];
        if remaining.is_empty() {
            break;
        }
    }
}

fn write_prefix_satisfied(bufs: &[IoSliceMut], bytes: &[u8]) -> bool {
    bufs.iter().map(|b| b.len()).sum::<usize>() >= bytes.len()
}

fn write_shut_hdr(bufs: &mut [IoSliceMut], host_port: u32, guest_port: u32) {
    let shut = Hdr {
        src_port: host_port,
        dst_port: guest_port,
        type_: TYPE_STREAM,
        op: OP_SHUTDOWN,
        flags: SHUTDOWN_BOTH,
        buf_alloc: BUF_ALLOC,
        ..Default::default()
    };
    write_prefix(bufs, &shut.to_bytes());
}

fn gather<const N: usize>(bufs: &[IoSlice]) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    let mut filled = 0;
    for buf in bufs {
        let n = (N - filled).min(buf.len());
        out[filled..filled + n].copy_from_slice(&buf[..n]);
        filled += n;
        if filled == N {
            return Some(out);
        }
    }
    None
}

fn fill_rx_bufs(
    bufs: &mut [IoSliceMut],
    reader: &mut UnixStream,
    name: &Arc<str>,
) -> io::Result<(usize, bool, bool)> {
    let mut skip = HDR_SIZE;
    let mut nread = 0usize;
    let mut eof = false;
    for buf in bufs.iter_mut() {
        let off = if skip > 0 {
            let n = skip.min(buf.len());
            skip -= n;
            n
        } else {
            0
        };
        if off == buf.len() {
            continue;
        }
        if eof {
            break;
        }
        match reader.read(&mut buf[off..]) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(n) => {
                nread += n;
                if off + n < buf.len() {
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                log::error!("{name}: read host socket: {e}");
                break;
            }
        }
    }
    Ok((nread, eof, skip == 0))
}

impl Virtio for InProcessVsock {
    type Config = VsockConfig;
    type Feature = VsockFeature;

    fn id(&self) -> DeviceId {
        DeviceId::SOCKET
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn num_queues(&self) -> u16 {
        3
    }

    fn config(&self) -> Arc<VsockConfig> {
        self.config.clone()
    }

    fn feature(&self) -> u128 {
        VsockFeature::STREAM.bits() | FEATURE_BUILT_IN
    }

    fn spawn_worker<S: IrqSender, E: IoeventFd>(
        self,
        event_rx: Receiver<WakeEvent<S, E>>,
        memory: Arc<RamBus>,
        queue_regs: Arc<[QueueReg]>,
    ) -> Result<(JoinHandle<()>, Arc<Notifier>)> {
        Mio::spawn_worker(self, event_rx, memory, queue_regs)
    }
}

impl VirtioMio for InProcessVsock {
    fn activate<'m, Q, S, E>(
        &mut self,
        _feature: u128,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        active_mio.poll.registry().register(
            &mut SourceFd(&self.wake_read.as_raw_fd()),
            Token(self.wake_read.as_raw_fd() as usize),
            Interest::READABLE,
        )?;
        Ok(())
    }

    fn handle_event<'m, Q, S, E>(
        &mut self,
        event: &Event,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        let token = event.token();
        let registry = active_mio.poll.registry();
        let irq_sender = active_mio.irq_sender;
        let Some(Some(rx_q)) = active_mio.queues.get_mut(QUEUE_RX as usize) else {
            log::error!("{}: rx queue not ready", self.name);
            return Ok(());
        };
        if token.0 == self.wake_read.as_raw_fd() as usize {
            self.drain_wake_pipe();
            self.process_pending(registry, rx_q, irq_sender)
        } else if let Some(&(host_port, guest_port)) = self.ports.get(&token) {
            self.transfer_rx_data(host_port, guest_port, registry, rx_q, irq_sender)
        } else {
            log::error!("{}: invalid token: {token:#?}", self.name);
            Ok(())
        }
    }

    fn handle_queue<'m, Q, S, E>(
        &mut self,
        index: u16,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        match index {
            QUEUE_TX => {
                let [Some(rx_q), Some(tx_q), ..] = active_mio.queues else {
                    log::error!("{}: queues not ready", self.name);
                    return Ok(());
                };
                let registry = active_mio.poll.registry();
                let irq_sender = active_mio.irq_sender;
                let name: Arc<str> = self.name.clone();
                tx_q.handle_desc(QUEUE_TX, irq_sender, |desc| {
                    if let Err(e) = self.handle_tx_desc(desc, registry, irq_sender, rx_q) {
                        log::error!("{name}: handle tx: {e:?}");
                        return Ok(Status::Break);
                    }
                    Ok(Status::Done { len: 0 })
                })?;
                Ok(())
            }
            QUEUE_RX | 2 => {
                log::debug!("{}: queue {index} buffer available", self.name);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn reset(&mut self, registry: &Registry) {
        for (_, conn) in self.connections.drain() {
            if let Err(err) = registry.deregister(&mut SourceFd(&conn.reader.as_raw_fd())) {
                log::error!("{}: failed to deregister socket: {err}", self.name);
            }
        }
        if let Err(err) = registry.deregister(&mut SourceFd(&self.wake_read.as_raw_fd())) {
            log::error!("{}: failed to deregister wake pipe: {err}", self.name);
        }
        self.ports.clear();
        self.host_ports.clear();
        self.next_port = 1024;
    }
}
