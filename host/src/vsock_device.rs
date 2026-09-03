use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, IoSlice, IoSliceMut};
use std::num::Wrapping;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alioth::hv::IoeventFd;
use alioth::mem::mapped::RamBus;
use alioth::sync::notifier::Notifier;
use alioth::virtio::dev::vsock::{VsockConfig, VsockFeature};
use alioth::virtio::dev::{DevSpec, Virtio, WakeEvent};
use alioth::virtio::queue::{DescChain, Queue, QueueReg, Status, VirtQueue};
use alioth::virtio::worker::mio::{ActiveMio, Mio, VirtioMio};
use alioth::virtio::{DeviceId, IrqSender, VirtioFeature};
use flume::Receiver;
use mio::event::Event;
use mio::{Interest, Registry, Token};

use crate::vsock_pipe::{PipeDevEnd, VsockPipe, VsockStream, WakeFn};

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

#[derive(Debug)]
enum ConnState {
    Established { fwd_cnt: Wrapping<u32> },
    Shutdown { flags: u32 },
}

#[derive(Debug)]
struct Connection {
    state: ConnState,
    pipe: PipeDevEnd,
    pending: Vec<u8>,
}

impl Connection {
    fn established(pipe: PipeDevEnd) -> Self {
        Connection {
            state: ConnState::Established {
                fwd_cnt: Wrapping(0),
            },
            pipe,
            pending: Vec::new(),
        }
    }
}

fn flush_pending(conn: &mut Connection) -> io::Result<usize> {
    let mut total = 0usize;
    while !conn.pending.is_empty() {
        match conn.pipe.try_write(&conn.pending) {
            Ok(n) => {
                total += n;
                if n == conn.pending.len() {
                    conn.pending.clear();
                } else {
                    conn.pending.drain(..n);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

#[derive(Debug, Default)]
struct PortState {
    queue: VecDeque<VsockStream>,
    listener: Option<tokio::sync::oneshot::Sender<VsockStream>>,
}

#[derive(Debug, Default)]
struct Shared {
    ports: Mutex<HashMap<u32, PortState>>,
}

#[derive(Clone, Debug)]
pub struct VsockHost {
    shared: Arc<Shared>,
}

impl VsockHost {
    pub async fn accept(&self, port: u32) -> io::Result<VsockStream> {
        let receiver = {
            let mut ports = self.shared.ports.lock().unwrap();
            let state = ports.entry(port).or_default();
            if let Some(stream) = state.queue.pop_front() {
                return Ok(stream);
            }
            let (sender, receiver) = tokio::sync::oneshot::channel();
            state.listener = Some(sender);
            receiver
        };
        receiver
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "vsock device dropped"))
    }
}

#[derive(Debug)]
pub struct VsockParam {
    cid: u32,
    shared: Arc<Shared>,
}

impl VsockParam {
    pub fn new(cid: u32) -> (Self, VsockHost) {
        let shared = Arc::new(Shared::default());
        (
            VsockParam {
                cid,
                shared: shared.clone(),
            },
            VsockHost { shared },
        )
    }
}

impl DevSpec for VsockParam {
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
            connections: HashMap::new(),
            notifier: Arc::new(Mutex::new(Notifier::new()?)),
        })
    }
}

// must not collide with the queue/kicker tokens used by the mio worker
const TOKEN_NOTIFY: usize = 1 << 61;
// matches the 2MiB socket buffers this device used to allocate per connection
const PIPE_CAPACITY: usize = 1 << 21;

#[derive(Debug)]
pub struct InProcessVsock {
    name: Arc<str>,
    config: Arc<VsockConfig>,
    guest_cid: u32,
    shared: Arc<Shared>,
    connections: HashMap<(u32, u32), Connection>,
    notifier: Arc<Mutex<Notifier>>,
}

impl InProcessVsock {
    fn device_wake_fn(&self) -> WakeFn {
        let notifier = self.notifier.clone();
        Arc::new(move || {
            if let Err(e) = notifier.lock().unwrap().notify() {
                log::debug!("vsock wake: {e}");
            }
        })
    }

    fn handle_tx_request<'m, Q, S>(
        &mut self,
        hdr: &Hdr,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let (host_port, guest_port) = (hdr.dst_port, hdr.src_port);
        let (dev_end, host_end) = VsockPipe::pair(PIPE_CAPACITY, self.device_wake_fn());
        let response = Hdr {
            src_cid: CID_HOST,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            type_: TYPE_STREAM,
            op: OP_RESPONSE,
            buf_alloc: BUF_ALLOC,
            ..Default::default()
        };
        if let Err(e) = self.respond(&response, irq_sender, rx_q) {
            log::error!("{}: failed to send connection response: {e:?}", self.name);
            return self.respond_rst(hdr, irq_sender, rx_q);
        }
        let conn = Connection::established(dev_end);
        log::debug!(
            "{}: vm:{guest_port} -> host:{host_port}: established (guest-initiated)",
            self.name
        );
        self.connections.insert((host_port, guest_port), conn);
        let mut ports = self.shared.ports.lock().unwrap();
        let state = ports.entry(host_port).or_default();
        if let Some(sender) = state.listener.take() {
            let _ = sender.send(host_end);
        } else {
            state.queue.push_back(host_end);
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
            if !write_prefix_satisfied(&desc.writable, &bytes) {
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

    fn send_credit_update<'m, Q, S>(
        &mut self,
        host_port: u32,
        guest_port: u32,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let Some(conn) = self.connections.get(&(host_port, guest_port)) else {
            return Ok(());
        };
        let ConnState::Established { fwd_cnt } = conn.state else {
            return Ok(());
        };
        let update = Hdr {
            src_cid: CID_HOST,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            type_: TYPE_STREAM,
            op: OP_CREDIT_UPDATE,
            fwd_cnt: fwd_cnt.0,
            buf_alloc: BUF_ALLOC,
            ..Default::default()
        };
        self.respond(&update, irq_sender, rx_q)
    }

    fn remove_conn(&mut self, host_port: u32, guest_port: u32) -> Result<()> {
        // dropping the pipe end closes the host-side stream (EOF/EPIPE)
        if self.connections.remove(&(host_port, guest_port)).is_none() {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
        }
        Ok(())
    }

    fn handle_tx_shutdown(&mut self, hdr: &Hdr) -> Result<()> {
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
            self.remove_conn(host_port, guest_port)?;
        }
        Ok(())
    }

    fn handle_tx_desc<'m, Q, S>(
        &mut self,
        desc: &mut DescChain<'_>,
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
            OP_REQUEST => {
                self.handle_tx_request(&hdr, irq_sender, rx_q)?;
                self.transfer_rx_data(hdr.dst_port, hdr.src_port, rx_q, irq_sender)?;
            }
            OP_RW => {
                self.transfer_tx_data(&hdr, &desc.readable)?;
                self.send_credit_update(hdr.dst_port, hdr.src_port, irq_sender, rx_q)?;
            }
            OP_RST => {
                self.remove_conn(hdr.dst_port, hdr.src_port)?;
            }
            OP_SHUTDOWN => self.handle_tx_shutdown(&hdr)?,
            OP_CREDIT_UPDATE => {}
            OP_CREDIT_REQUEST => {
                self.send_credit_update(hdr.dst_port, hdr.src_port, irq_sender, rx_q)?;
            }
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
        let ConnState::Established { .. } = conn.state else {
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
            conn.pending.extend_from_slice(&buf[..n]);
            remain -= n;
        }
        if remain > 0 {
            log::error!("{}: missing {remain} bytes", self.name);
        }
        let flushed = match flush_pending(conn) {
            Ok(n) => n,
            Err(e) => {
                log::error!("{}: write host stream: {e}", self.name);
                0
            }
        };
        // if the pipe filled up, the host read side will wake us when it drains
        if let ConnState::Established { fwd_cnt } = &mut conn.state {
            *fwd_cnt += Wrapping(flushed as u32);
        }
        Ok(())
    }

    fn transfer_rx_data<'m, Q, S>(
        &mut self,
        host_port: u32,
        guest_port: u32,
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
            let shut = Hdr {
                src_cid: CID_HOST,
                dst_cid: self.guest_cid,
                src_port: host_port,
                dst_port: guest_port,
                type_: TYPE_STREAM,
                op: OP_SHUTDOWN,
                flags: SHUTDOWN_BOTH,
                buf_alloc: BUF_ALLOC,
                ..Default::default()
            }
            .to_bytes();
            let name = &self.name;
            let pipe = &mut conn.pipe;
            rx_q.handle_desc(QUEUE_RX, irq_sender, |desc| {
                if send_shutdown {
                    return Ok(Status::Break);
                }
                let (nread, eof, fits) = fill_rx_bufs(&mut desc.writable, pipe, name)?;
                if !fits {
                    log::error!("{name}: no buffer space for RW");
                    return Ok(Status::Break);
                }
                if nread == 0 {
                    if eof {
                        send_shutdown = true;
                        write_prefix(&mut desc.writable, &shut);
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
            let _ = self.remove_conn(host_port, guest_port);
        }
        Ok(())
    }

    /// Re-examines every connection after the host side changed state
    /// (notifier wake) or the guest posted new RX buffers: flush pending
    /// guest data into the pipe and move pipe data into guest buffers.
    fn service_connections<'m, Q, S>(
        &mut self,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let keys: Vec<(u32, u32)> = self.connections.keys().copied().collect();
        for (host_port, guest_port) in keys {
            let mut credit = false;
            if let Some(conn) = self.connections.get_mut(&(host_port, guest_port))
                && !conn.pending.is_empty()
            {
                match flush_pending(conn) {
                    Ok(n) => credit = n > 0 && conn.pending.is_empty(),
                    Err(e) => log::error!("{}: write host stream: {e}", self.name),
                }
            }
            if credit {
                self.send_credit_update(host_port, guest_port, irq_sender, rx_q)?;
            }
            let ready = self
                .connections
                .get(&(host_port, guest_port))
                .is_some_and(|conn| conn.pipe.is_readable());
            if ready {
                self.transfer_rx_data(host_port, guest_port, rx_q, irq_sender)?;
            }
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
    pipe: &mut PipeDevEnd,
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
        match pipe.read_slice(&mut buf[off..]) {
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
                log::error!("{name}: read host stream: {e}");
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
        let registry = active_mio.poll.registry();
        let mut notifier = self.notifier.lock().unwrap();
        if let Err(e) = registry.register(&mut *notifier, Token(TOKEN_NOTIFY), Interest::READABLE) {
            log::error!("{}: failed to register notifier: {e}", self.name);
        }
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
        if event.token() != Token(TOKEN_NOTIFY) {
            log::error!("{}: unexpected token {:?}", self.name, event.token());
            return Ok(());
        }
        let Some(Some(rx_q)) = active_mio.queues.get_mut(QUEUE_RX as usize) else {
            log::error!("{}: rx queue not ready", self.name);
            return Ok(());
        };
        let irq_sender = active_mio.irq_sender;
        self.service_connections(rx_q, irq_sender)
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
                let irq_sender = active_mio.irq_sender;
                let name: Arc<str> = self.name.clone();
                tx_q.handle_desc(QUEUE_TX, irq_sender, |desc| {
                    if let Err(e) = self.handle_tx_desc(desc, irq_sender, rx_q) {
                        log::error!("{name}: handle tx: {e:?}");
                        return Ok(Status::Break);
                    }
                    Ok(Status::Done { len: 0 })
                })?;
                Ok(())
            }
            QUEUE_RX => {
                log::debug!("{}: queue {index} buffer available", self.name);
                // the guest posted RX buffers: data may have been waiting
                let Some(Some(rx_q)) = active_mio.queues.get_mut(QUEUE_RX as usize) else {
                    return Ok(());
                };
                let irq_sender = active_mio.irq_sender;
                self.service_connections(rx_q, irq_sender)
            }
            _ => Ok(()),
        }
    }

    fn reset(&mut self, registry: &Registry) {
        self.connections.clear();
        if let Err(e) = registry.deregister(&mut *self.notifier.lock().unwrap()) {
            log::debug!("{}: failed to deregister notifier: {e}", self.name);
        }
    }
}
