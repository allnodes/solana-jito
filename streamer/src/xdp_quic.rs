
use {
    agave_xdp::{
        device::QueueId,
        netlink::MacAddress,
        runtime::XdpRuntime,
        rx_tx::XskChannel,
        socket::Socket,
        umem::{OwnedUmem, PageAlignedMemory},
    },
    quinn::{
        AsyncUdpSocket, UdpPoller,
        udp::{RecvMeta, Transmit},
    },
    std::{
        collections::HashMap,
        error::Error,
        fmt::{self, Debug},
        io::{self, IoSliceMut},
        net::{IpAddr, SocketAddr, SocketAddrV4},
        pin::Pin,
        sync::{Arc, Mutex, MutexGuard, OnceLock},
        task::{Context, Poll, ready},
    },
    tokio::io::unix::AsyncFd,
};

const FRAME_SIZE: usize = 4096;
const RING_SIZE: usize = 2048;
const PEER_MAC_CAP: usize = 16384;

pub struct XskQuicSocket {
    channel: XskChannel<OwnedUmem<PageAlignedMemory>>,
    local: SocketAddrV4,
    device: usize,
    queue: u32,
}

impl Debug for XskQuicSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XskQuicSocket")
            .field("local", &self.local)
            .field("device", &self.device)
            .field("queue", &self.queue)
            .finish()
    }
}

impl XskQuicSocket {
    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::V4(self.local)
    }

    pub fn into_async(self) -> io::Result<Arc<XskUdpSocket>> {
        let fd = self.channel.fd();
        Ok(Arc::new(XskUdpSocket {
            afd: AsyncFd::new(fd)?,
            local: self.local,
            inner: Mutex::new(XskState {
                channel: self.channel,
                peer_macs: HashMap::new(),
                last_mac: None,
            }),
        }))
    }
}

pub fn build_xsk_quic_sockets(
    runtime: &mut XdpRuntime,
    interface: &str,
    queues: &[u32],
    port: u16,
    zero_copy: bool,
) -> Result<Vec<XskQuicSocket>, Box<dyn Error>> {
    let src_ip = agave_xdp::interface_ipv4(interface)?;
    let local = SocketAddrV4::new(src_ip, port);

    let mut sockets = Vec::with_capacity(runtime.device_count().saturating_mul(queues.len()));
    for idx in 0..runtime.device_count() {
        for &queue in queues {
            let channel = {
                let dev = runtime.device(idx);
                let name = dev.name();
                let src_mac = dev.mac_addr()?;
                let bound = dev.open_queue(QueueId(u64::from(queue)))?;
                let (fill_size, rx_size) = if zero_copy {
                    let rx = bound
                        .ring_sizes()
                        .ok_or_else(|| {
                            format!("{name}: zero copy requires a known device ring size")
                        })?
                        .rx;
                    (rx, rx)
                } else {
                    (RING_SIZE, RING_SIZE)
                };
                let frame_count = fill_size.saturating_add(RING_SIZE).next_power_of_two();
                let mem = PageAlignedMemory::alloc(FRAME_SIZE, frame_count)
                    .map_err(|_| "failed to allocate UMEM")?;
                let umem = OwnedUmem::new(mem, FRAME_SIZE as u32)?;
                let (socket, rx, tx) =
                    Socket::new(bound, umem, zero_copy, fill_size, rx_size, RING_SIZE, RING_SIZE)
                        .map_err(|e| {
                            format!(
                                "{name} queue {queue}: AF_XDP socket setup \
                                 (zero_copy={zero_copy}) failed: {e}"
                            )
                        })?;
                XskChannel::new(socket, rx, tx, src_mac).ok_or("socket missing RX/TX rings")?
            };
            runtime.register_xsk(idx, queue, channel.fd())?;
            sockets.push(XskQuicSocket { channel, local, device: idx, queue });
        }
    }
    Ok(sockets)
}

struct XskState {
    channel: XskChannel<OwnedUmem<PageAlignedMemory>>,
    peer_macs: HashMap<SocketAddr, MacAddress>,
    last_mac: Option<MacAddress>,
}

pub struct XskUdpSocket {
    afd: AsyncFd<std::os::fd::RawFd>,
    local: SocketAddrV4,
    inner: Mutex<XskState>,
}

impl Debug for XskUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XskUdpSocket").field("local", &self.local).finish()
    }
}

impl XskUdpSocket {
    fn lock(&self) -> MutexGuard<'_, XskState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn drain(&self, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> usize {
        let max = bufs.len().min(meta.len());
        if max == 0 {
            return 0;
        }
        let state = &mut *self.lock();
        let XskState {
            channel,
            peer_macs,
            last_mac,
            ..
        } = state;
        let mut n = 0;
        channel.poll_recv(max, |src_mac, udp| {
            let buf = &mut bufs[n];
            let len = udp.payload.len();
            if len > buf.len() {
                log::trace!(
                    "xsk drain: dropping {len}-byte datagram from {}, receive buffer holds {}",
                    udp.src,
                    buf.len()
                );
                return;
            }
            buf[..len].copy_from_slice(udp.payload);
            let peer = SocketAddr::V4(udp.src);
            meta[n] = RecvMeta {
                addr: peer,
                len,
                stride: len,
                ecn: None,
                dst_ip: Some(IpAddr::V4(*udp.dst.ip())),
            };
            if peer_macs.len() >= PEER_MAC_CAP && !peer_macs.contains_key(&peer) {
                let evict = peer_macs.keys().next().copied();
                if let Some(evict) = evict {
                    peer_macs.remove(&evict);
                }
            }
            peer_macs.insert(peer, *src_mac);
            *last_mac = Some(*src_mac);
            n += 1;
        });
        n
    }
}

impl AsyncUdpSocket for XskUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(XskWritePoller { socket: self })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let SocketAddr::V4(dst) = transmit.destination else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xsk socket is ipv4-only",
            ));
        };
        let src_ip = match transmit.src_ip {
            Some(IpAddr::V4(ip)) => ip,
            _ => *self.local.ip(),
        };
        let src = SocketAddrV4::new(src_ip, self.local.port());
        let mut state = self.lock();
        let mac = match state.peer_macs.get(&transmit.destination) {
            Some(mac) => *mac,
            None => match state.last_mac {
                Some(mac) => mac,
                None => {
                    log::trace!("xsk try_send: no known link-layer next hop for {dst}");
                    return Ok(());
                }
            },
        };
        if !state.channel.send(&mac, src, dst, transmit.contents) {
            state.channel.reclaim_tx();
            if !state.channel.send(&mac, src, dst, transmit.contents) {
                state.channel.flush_tx();
                return Err(io::ErrorKind::WouldBlock.into());
            }
        }
        state.channel.flush_tx();
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let n = self.drain(bufs, meta);
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
            let mut guard = ready!(self.afd.poll_read_ready(cx))?;
            let n = self.drain(bufs, meta);
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
            guard.clear_ready();
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(self.local))
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

struct XskWritePoller {
    socket: Arc<XskUdpSocket>,
}

impl Debug for XskWritePoller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XskWritePoller").finish_non_exhaustive()
    }
}

impl UdpPoller for XskWritePoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let mut state = self.socket.lock();
        state.channel.reclaim_tx();
        if state.channel.tx_ready() {
            Poll::Ready(Ok(()))
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

type QuicSocketsByPort = Vec<(u16, Vec<XskQuicSocket>)>;
static QUIC_SOCKETS: OnceLock<Mutex<QuicSocketsByPort>> = OnceLock::new();

pub fn hold_sockets(sockets: QuicSocketsByPort) -> Result<(), Box<dyn Error>> {
    for (port, built) in &sockets {
        log::debug!("xdp: {} quic receive socket(s) held for udp/{port}", built.len());
    }
    if QUIC_SOCKETS.set(Mutex::new(sockets)).is_err() {
        return Err("XDP QUIC receive sockets were installed twice in one process".into());
    }
    Ok(())
}

pub fn attach(port: u16) -> Vec<XskQuicSocket> {
    let Some(installed) = QUIC_SOCKETS.get() else {
        return Vec::new();
    };
    let taken = {
        let mut installed = installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match installed.iter().position(|(held, _)| *held == port) {
            Some(index) => installed.remove(index).1,
            None => return Vec::new(),
        }
    };
    if let Err(err) = agave_xdp::runtime::add_installed_rx_port(port) {
        log::warn!(
            "xdp: udp/{port} stays on the kernel receive path, {err}. Throughput on that port is \
             what it would be without XDP."
        );
        return Vec::new();
    }
    log::info!(
        "xdp: receive accelerated on udp/{port} ({})",
        taken
            .iter()
            .map(|socket| format!("device {} queue {}", socket.device, socket.queue))
            .collect::<Vec<_>>()
            .join(", ")
    );
    taken
}
