
use {
    crate::streamer::{ChannelSend, StreamerReceiveStats},
    agave_xdp::{
        device::QueueId,
        packet::parse_udp_frame,
        runtime::XdpRuntime,
        rx::RxSocket,
        socket::Socket,
        umem::{OwnedUmem, PageAlignedMemory},
    },
    bytes::Bytes,
    solana_packet::{Meta, PACKET_DATA_SIZE},
    solana_perf::packet::{BytesPacket, BytesPacketBatch, PACKETS_PER_BATCH, PacketBatch},
    std::{
        error::Error,
        net::SocketAddr,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::Builder,
    },
};

const FRAME_SIZE: usize = 4096;
const RING_SIZE: usize = 2048;
const RX_RING_SIZE: usize = 8192;
const POLL_TIMEOUT_MS: libc::c_int = 5;

pub struct XskRxSocket {
    socket: RxSocket<OwnedUmem<PageAlignedMemory>>,
    device: String,
    queue: u32,
}

impl XskRxSocket {
    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn queue(&self) -> u32 {
        self.queue
    }
}

pub fn build_xsk_rx_sockets(
    runtime: &mut XdpRuntime,
    queues: &[u32],
    zero_copy: bool,
) -> Result<Vec<XskRxSocket>, Box<dyn Error>> {
    let mut sockets = Vec::with_capacity(runtime.device_count().saturating_mul(queues.len()));
    for idx in 0..runtime.device_count() {
        for &queue in queues {
            let (socket, device) = {
                let dev = runtime.device(idx);
                let name = dev.name().to_string();
                let bound = dev.open_queue(QueueId(u64::from(queue)))?;
                let device_ring = if zero_copy {
                    bound
                        .ring_sizes()
                        .ok_or_else(|| {
                            format!("{name}: zero copy requires a known device ring size")
                        })?
                        .rx
                } else {
                    RING_SIZE
                };
                let fill = device_ring.max(RX_RING_SIZE);
                let ring = RX_RING_SIZE;
                let frames = fill
                    .saturating_add(device_ring)
                    .saturating_add(RING_SIZE)
                    .next_power_of_two();
                let mem = PageAlignedMemory::alloc(FRAME_SIZE, frames)
                    .map_err(|_| "failed to allocate UMEM")?;
                let umem = OwnedUmem::new(mem, FRAME_SIZE as u32)?;
                let (socket, rx) =
                    Socket::rx(bound, umem, zero_copy, fill, ring).map_err(|err| {
                        format!(
                            "{name} queue {queue}: AF_XDP receive socket (zero_copy={zero_copy}) \
                             failed: {err}"
                        )
                    })?;
                (RxSocket::new(socket, rx).ok_or("socket missing an RX ring")?, name)
            };
            runtime.register_xsk(idx, queue, socket.fd())?;
            sockets.push(XskRxSocket {
                socket,
                device,
                queue,
            });
        }
    }
    Ok(sockets)
}

struct Group {
    name: &'static str,
    index: usize,
    ports: Vec<u16>,
    sinks: Mutex<Vec<Sink>>,
    pending: Mutex<Vec<XskRxSocket>>,
    drains: AtomicUsize,
    exit: Arc<AtomicBool>,
}

struct Sink {
    port: u16,
    channel: Box<dyn ChannelSend<PacketBatch>>,
    stats: Arc<StreamerReceiveStats>,
    is_staked_service: bool,
}

static GROUPS: OnceLock<Vec<Arc<Group>>> = OnceLock::new();

pub fn hold_groups(
    groups: Vec<(&'static str, Vec<u16>, Vec<XskRxSocket>)>,
    exit: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
    let installed = groups
        .into_iter()
        .enumerate()
        .map(|(index, (name, ports, sockets))| {
            log::debug!(
                "xdp: receive group {name} covers udp/{ports:?} on {} socket(s)",
                sockets.len()
            );
            Arc::new(Group {
                name,
                index,
                ports,
                sinks: Mutex::new(Vec::new()),
                pending: Mutex::new(sockets),
                drains: AtomicUsize::new(0),
                exit: exit.clone(),
            })
        })
        .collect();
    if GROUPS.set(installed).is_err() {
        return Err("XDP receive groups were installed twice in one process".into());
    }
    Ok(())
}

pub fn attach(
    port: u16,
    channel: impl ChannelSend<PacketBatch>,
    stats: Arc<StreamerReceiveStats>,
    is_staked_service: bool,
) -> bool {
    let Some(groups) = GROUPS.get() else {
        return false;
    };
    let Some(group) = groups.iter().find(|group| group.ports.contains(&port)) else {
        return false;
    };
    {
        let mut sinks = lock(&group.sinks);
        if sinks.iter().any(|sink| sink.port == port) {
            return false;
        }
        sinks.push(Sink {
            port,
            channel: Box::new(channel),
            stats,
            is_staked_service,
        });
    }
    let refuse = |reason: String| {
        lock(&group.sinks).retain(|sink| sink.port != port);
        log::warn!(
            "xdp: udp/{port} stays on the kernel receive path, {reason}. Throughput on that port \
             is what it would be without XDP."
        );
        false
    };
    if !start_drains(group) {
        return refuse(format!("no drain thread started for group {}", group.name));
    }
    match agave_xdp::runtime::add_installed_rx_port(port) {
        Ok(()) => {
            log::info!("xdp: receive accelerated on udp/{port} ({})", group.name);
            true
        }
        Err(err) => refuse(err.to_string()),
    }
}

pub fn attach_receiver(
    socket: &std::net::UdpSocket,
    channel: &(impl ChannelSend<PacketBatch> + Clone),
    stats: &Arc<StreamerReceiveStats>,
    is_staked_service: bool,
) {
    if let Ok(addr) = socket.local_addr() {
        attach(addr.port(), channel.clone(), stats.clone(), is_staked_service);
    }
}

fn start_drains(group: &Arc<Group>) -> bool {
    {
        let mut pending = lock(&group.pending);
        let sockets = std::mem::take(&mut *pending);
        let expected = sockets.len();
        if expected > 0 {
            let mut started = 0usize;
            for (socket_index, socket) in sockets.into_iter().enumerate() {
                let name = format!("solXdpRx{}{socket_index:02}", group.index);
                match spawn_drain(group.clone(), socket, name) {
                    Ok(()) => started = started.saturating_add(1),
                    Err(err) => log::debug!(
                        "xdp: could not start a drain for receive group {}: {err}",
                        group.name
                    ),
                }
            }
            group.drains.store(
                if started == expected { started } else { 0 },
                Ordering::Relaxed,
            );
        }
    }
    group.drains.load(Ordering::Relaxed) > 0
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn spawn_drain(
    group: Arc<Group>,
    mut xsk: XskRxSocket,
    name: String,
) -> Result<(), Box<dyn Error>> {
    Builder::new().name(name).spawn(move || {
        let exit = group.exit.clone();
        let fd = xsk.socket.fd();
        let mut batches: Vec<BytesPacketBatch> = Vec::new();
        let mut unroutable: u64 = 0;
        while !exit.load(Ordering::Relaxed) {
            let sinks = lock(&group.sinks);
            batches.resize_with(sinks.len(), BytesPacketBatch::new);
            xsk.socket.poll(|frame| {
                let Some(udp) = parse_udp_frame(frame) else {
                    return;
                };
                if udp.payload.len() > PACKET_DATA_SIZE {
                    return;
                }
                let Some(index) = sinks.iter().position(|sink| sink.port == udp.dst.port()) else {
                    if unroutable == 0 {
                        log::debug!(
                            "xdp: receive group {} was handed udp/{} with no attached path — the \
                             steering rules and the queue plan disagree",
                            group.name,
                            udp.dst.port()
                        );
                    }
                    unroutable = unroutable.saturating_add(1);
                    return;
                };
                let mut meta = Meta::default();
                meta.size = udp.payload.len();
                meta.set_socket_addr(&SocketAddr::V4(udp.src));
                meta.set_from_staked_node(sinks[index].is_staked_service);
                batches[index].push(BytesPacket::new(Bytes::copy_from_slice(udp.payload), meta));
            });
            let mut idle = true;
            for (index, batch) in batches.iter_mut().enumerate() {
                if batch.is_empty() {
                    continue;
                }
                idle = false;
                let sink = &sinks[index];
                let len = batch.len();
                let batch = std::mem::replace(batch, BytesPacketBatch::new());
                sink.stats.packets_count.fetch_add(len, Ordering::Relaxed);
                sink.stats.packet_batches_count.fetch_add(1, Ordering::Relaxed);
                sink.stats
                    .max_channel_len
                    .fetch_max(sink.channel.len(), Ordering::Relaxed);
                if len >= PACKETS_PER_BATCH {
                    sink.stats
                        .full_packet_batches_count
                        .fetch_add(1, Ordering::Relaxed);
                }
                if sink.channel.try_send(PacketBatch::from(batch)).is_err() {
                    sink.stats.num_packets_dropped.fetch_add(len, Ordering::Relaxed);
                }
            }
            drop(sinks);
            if idle {
                wait_readable(fd);
            }
        }
        log::debug!(
            "xdp: receive group {} drain on {} queue {} finished, {unroutable} unroutable packet(s)",
            group.name,
            xsk.device,
            xsk.queue,
        );
    })?;
    Ok(())
}

fn wait_readable(fd: std::os::fd::RawFd) {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe {
        libc::poll(&mut poll_fd, 1, POLL_TIMEOUT_MS);
    }
}
