use {
    crate::ecn_codepoint::EcnCodepoint,
    bytes::Bytes,
    crossbeam_channel::{Sender, TrySendError},
    std::{
        error::Error,
        net::{SocketAddr, SocketAddrV4},
        sync::{Arc, atomic::AtomicBool},
        thread,
    },
};
#[cfg(target_os = "linux")]
use {
    crate::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        route::{RouteTable, Router, RoutingTables},
        route_monitor::RouteMonitor,
        tx_loop::{TxLoop, TxLoopBuilder, TxLoopConfigBuilder, TxPacket},
        umem::{OwnedUmem, PageAlignedMemory},
    },
    agave_cpu_utils::{CpuId, cpu_affinity, set_cpu_affinity},
    arc_swap::ArcSwap,
    arrayvec::ArrayVec,
    aya::Ebpf,
    crossbeam_queue::ArrayQueue,
    log::info,
    std::{
        net::{IpAddr, Ipv4Addr},
        thread::Builder,
        time::Duration,
    },
};

#[cfg(target_os = "linux")]
const ROUTE_MONITOR_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

/// Binding of a single NIC hardware TX queue to a CPU core.
///
/// Each binding becomes one TX worker thread, pinned to `cpu`, whose AF_XDP
/// socket is bound to a hardware transmit queue on the configured interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueCpuBinding {
    /// Position of this binding in the transmit set.
    pub queue: u32,
    /// Logical CPU core the worker thread is pinned to.
    pub cpu: usize,
}

#[derive(Clone, Debug)]
pub struct XdpConfig {
    pub interface: Option<String>,
    /// NIC-queue -> CPU-core bindings. One TX worker is created per entry, in
    /// order. The queue id is taken explicitly from the binding rather than
    /// inferred from position, so callers can target arbitrary hardware queues.
    pub queues: Vec<QueueCpuBinding>,
    pub zero_copy: bool,
    // The capacity of the channel that sits between senders and each XDP thread that enqueues
    // packets to the NIC.
    pub tx_channel_cap: usize,
    pub tx_queue_base: u64,
    pub manage_program: bool,
}

impl XdpConfig {
    // A nice round number
    const DEFAULT_TX_CHANNEL_CAP: usize = 1_000_000;
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            interface: None,
            queues: vec![],
            zero_copy: false,
            tx_channel_cap: Self::DEFAULT_TX_CHANNEL_CAP,
            tx_queue_base: 0,
            manage_program: true,
        }
    }
}

impl XdpConfig {
    pub fn new(
        interface: Option<impl Into<String>>,
        queues: Vec<QueueCpuBinding>,
        zero_copy: bool,
    ) -> Self {
        Self {
            interface: interface.map(|s| s.into()),
            queues,
            zero_copy,
            ..Self::default()
        }
    }
}

/// [`BytesTxPacket`] encapsulates the information needed to transmit a packet via XDP. Besides
/// the payload and destination addresses, it includes the source address of the packet.
#[cfg(target_os = "linux")]
pub struct BytesTxPacket {
    src_addr: SocketAddrV4,
    dst_addrs: XdpAddrs,
    ecn: Option<EcnCodepoint>,
    allow_mtu_overflow: bool,
    payload: Bytes,
}

#[cfg(not(target_os = "linux"))]
pub struct BytesTxPacket;

#[cfg(target_os = "linux")]
impl BytesTxPacket {
    pub fn new(
        src_addr: SocketAddrV4,
        dst_addrs: impl Into<XdpAddrs>,
        ecn: Option<EcnCodepoint>,
        payload: Bytes,
    ) -> Self {
        Self {
            src_addr,
            dst_addrs: dst_addrs.into(),
            ecn,
            allow_mtu_overflow: false,
            payload,
        }
    }

    /// Sets whether MTU overflow is possible for this packet.
    pub fn set_allow_mtu_overflow(&mut self, allow: bool) {
        self.allow_mtu_overflow = allow;
    }
}

#[cfg(not(target_os = "linux"))]
impl BytesTxPacket {
    pub fn new(
        _src_addr: SocketAddrV4,
        _dst_addrs: impl Into<XdpAddrs>,
        _ecn: Option<EcnCodepoint>,
        _payload: Bytes,
    ) -> Self {
        Self
    }

    pub fn set_allow_mtu_overflow(&mut self, _allow: bool) {}
}

#[cfg(target_os = "linux")]
impl TxPacket for BytesTxPacket {
    type Addrs = XdpAddrs;
    type Payload = Bytes;

    fn dst_addrs(&self) -> &Self::Addrs {
        &self.dst_addrs
    }

    fn payload(&self) -> &Self::Payload {
        &self.payload
    }

    fn src_addr(&self) -> SocketAddrV4 {
        self.src_addr
    }

    fn ecn(&self) -> Option<EcnCodepoint> {
        self.ecn
    }

    fn allow_mtu_overflow(&self) -> bool {
        self.allow_mtu_overflow
    }
}

#[path = "bond_sender.rs"]
mod bond_sender;
pub use bond_sender::XdpSender;
#[cfg(target_os = "linux")]
use bond_sender::assign_loops;

pub enum XdpAddrs {
    Single(SocketAddr),
    Multi(Vec<SocketAddr>),
}

impl From<SocketAddr> for XdpAddrs {
    #[inline]
    fn from(addr: SocketAddr) -> Self {
        XdpAddrs::Single(addr)
    }
}

impl From<Vec<SocketAddr>> for XdpAddrs {
    #[inline]
    fn from(addrs: Vec<SocketAddr>) -> Self {
        XdpAddrs::Multi(addrs)
    }
}

impl AsRef<[SocketAddr]> for XdpAddrs {
    #[inline]
    fn as_ref(&self) -> &[SocketAddr] {
        match self {
            XdpAddrs::Single(addr) => std::slice::from_ref(addr),
            XdpAddrs::Multi(addrs) => addrs,
        }
    }
}

pub struct Transmitter {
    threads: Vec<thread::JoinHandle<()>>,
}

#[cfg(not(target_os = "linux"))]
pub struct TransmitterBuilder {}

#[cfg(target_os = "linux")]
pub struct TransmitterBuilder {
    tx_loops: Vec<(usize, TxLoop<OwnedUmem<PageAlignedMemory>>)>,
    tx_channel_cap: usize,
    ebpfs: Vec<Ebpf>,
    atomic_router: Arc<ArcSwap<Router>>,
    route_monitor_handle: thread::JoinHandle<()>,
    slave_cnt: usize,
    slave_live: Arc<[std::sync::atomic::AtomicBool]>,
    link_monitor_handle: Option<thread::JoinHandle<()>>,
}

impl TransmitterBuilder {
    #[cfg(not(target_os = "linux"))]
    pub fn new(_config: XdpConfig, _exit: Arc<AtomicBool>) -> Result<Self, Box<dyn Error>> {
        Err("XDP is only supported on Linux".into())
    }

    #[cfg(target_os = "linux")]
    pub fn new(config: XdpConfig, exit: Arc<AtomicBool>) -> Result<Self, Box<dyn Error>> {
        use {
            caps::Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
            log::debug,
            std::{collections::HashSet, io},
        };
        let XdpConfig {
            interface: maybe_interface,
            queues,
            zero_copy,
            tx_channel_cap,
            tx_queue_base,
            manage_program,
        } = config;

        let primary = if let Some(interface) = maybe_interface {
            NetworkDevice::new(interface)?
        } else {
            NetworkDevice::new_from_default_route()?
        };
        let master_name = primary.name().to_string();

        let (devices, src_mac) = match crate::bond::slaves(primary.name())? {
            Some(slave_names) if !slave_names.is_empty() => {
                let mode = crate::bond::mode(primary.name())?;
                if mode != "802.3ad" {
                    return Err(format!(
                        "bond interface {} uses mode {mode}; AF_XDP bond TX requires 802.3ad",
                        primary.name()
                    )
                    .into());
                }
                let src_mac = primary.mac_addr()?;
                let devices = slave_names
                    .iter()
                    .map(|name| NetworkDevice::new(name.as_str()).map(Arc::new))
                    .collect::<Result<Vec<_>, _>>()?;
                debug!(
                    "xdp bond mode: master {} -> {} slave(s): {}",
                    primary.name(),
                    devices.len(),
                    slave_names.join(", ")
                );
                (devices, src_mac)
            }
            Some(_) => {
                return Err(format!(
                    "bond interface {} has no slave devices",
                    primary.name()
                )
                .into());
            }
            None => {
                let src_mac = primary.mac_addr()?;
                (vec![Arc::new(primary)], src_mac)
            }
        };
        let slave_cnt = devices.len();
        let slave_live: Arc<[std::sync::atomic::AtomicBool]> = (0..slave_cnt)
            .map(|_| std::sync::atomic::AtomicBool::new(true))
            .collect::<Vec<_>>()
            .into();

        for dev in &devices {
            let driver = dev.driver().unwrap_or_else(|_| "unknown".to_string());
            debug!(
                "xdp slave {}: driver {driver}, {} mode",
                dev.name(),
                if zero_copy { "zero-copy" } else { "copy" }
            );
        }

        let mut tx_loop_config_builder = TxLoopConfigBuilder::new();
        tx_loop_config_builder.zero_copy(zero_copy);
        tx_loop_config_builder.override_src_mac(src_mac);
        let tx_loop_config = tx_loop_config_builder.build_with_src_device(&devices[0]);

        let reserved_cores = queues
            .iter()
            .map(|binding| CpuId::new(binding.cpu))
            .collect::<io::Result<HashSet<_>>>()?;
        let unreserved_cores = cpu_affinity(None)?
            .into_iter()
            .filter(|core| !reserved_cores.contains(core))
            .collect::<Vec<_>>();

        if unreserved_cores.is_empty() {
            return Err("all CPUs are reserved; no CPU available for the main thread".into());
        }
        set_cpu_affinity(None, unreserved_cores.iter().copied())?;

        let worker_cpus = queues.iter().map(|b| b.cpu).collect::<Vec<_>>();
        let tx_queues_per_slave = devices
            .iter()
            .map(|dev| {
                crate::ethtool::get_channels(dev.name())
                    .ok()
                    .map(|c| c.combined_count.saturating_add(c.tx_count))
                    .filter(|queues| *queues > 0)
            })
            .collect::<Vec<_>>();
        if slave_cnt > 1 && worker_cpus.len() < slave_cnt {
            log::warn!(
                "xdp: {} transmit core(s) for {slave_cnt} bond members — the members without \
                 one carry no transmit queue and cannot take over on failover",
                worker_cpus.len()
            );
        }
        let assignment =
            assign_loops(worker_cpus.len(), &tx_queues_per_slave, tx_queue_base)?;
        let mut tx_loop_builders = Vec::with_capacity(worker_cpus.len());
        for (cpu_id, (slave_idx, queue)) in worker_cpus.into_iter().zip(assignment) {
            // since we aren't necessarily allocating from the thread that we intend to run on,
            // temporarily switch to the target cpu for each TxLoop to ensure that the Umem region
            // is allocated to the correct numa node
            let cpu = CpuId::new(cpu_id)?;
            let dev = Arc::clone(&devices[slave_idx]);
            let config = tx_loop_config.clone();
            let umem_allocation =
                Builder::new()
                    .name("solXdpUmem".to_string())
                    .spawn(move || {
                        set_cpu_affinity(None, [cpu])?;
                        Ok::<_, io::Error>(TxLoopBuilder::new(
                            cpu_id,
                            QueueId(queue),
                            config,
                            &dev,
                        ))
                    })?;
            let tx_loop_builder = match umem_allocation.join() {
                Ok(tx_loop_builder) => tx_loop_builder?,
                Err(payload) => std::panic::resume_unwind(payload),
            };
            tx_loop_builders.push((slave_idx, tx_loop_builder));
        }

        // switch to higher caps while we setup XDP. We assume that an error in
        // this function is irrecoverable so we don't try to drop on errors.
        let _setup_caps =
            CapGuard::raise([CAP_NET_ADMIN, CAP_NET_RAW]).expect("raise net capabilities");

        let ebpfs_result = if zero_copy && manage_program {
            let _ebpf_caps =
                CapGuard::raise([CAP_BPF, CAP_PERFMON]).expect("raise ebpf capabilities");

            let load_result = devices
                .iter()
                .map(|dev| {
                    load_xdp_program(dev).map_err(|e| {
                        format!("failed to attach xdp program on {}: {e}", dev.name())
                    })
                })
                .collect::<Result<Vec<_>, _>>();

            load_result
        } else {
            Ok(Vec::new())
        };

        let tx_loops = tx_loop_builders
            .into_iter()
            .map(|(slave_idx, tx_loop_builder)| tx_loop_builder.build().map(|l| (slave_idx, l)))
            .collect::<Result<Vec<_>, io::Error>>()?;

        let tables_result = RoutingTables::from_netlink(RouteTable::Main);

        let tables = tables_result?;
        let router = Router::from_tables(tables)?;
        debug!(
            "published router table {}:\n{}",
            RouteTable::Main,
            router.routing_table()
        );

        // Use ArcSwap for lock-free updates of the routing table
        let atomic_router = Arc::new(ArcSwap::from_pointee(router));
        let route_monitor_handle = RouteMonitor::start(
            Arc::clone(&atomic_router),
            RouteTable::Main,
            exit.clone(),
            ROUTE_MONITOR_UPDATE_INTERVAL,
            || {
                drop_thread_capabilities("solRouteMon");
                info!("route monitor thread started");
            },
        );

        let ebpfs = ebpfs_result?;

        let link_monitor_handle = if slave_cnt > 1 {
            let slave_names: Vec<String> =
                devices.iter().map(|dev| dev.name().to_string()).collect();
            let slave_live = Arc::clone(&slave_live);
            let exit = exit.clone();
            let updelay = Duration::from_millis(crate::bond::updelay_ms(&master_name));
            Some(
                Builder::new()
                    .name("solXdpLink".to_owned())
                    .spawn(move || {
                        drop_thread_capabilities("solXdpLink");
                        use std::sync::atomic::Ordering;
                        const POLL_INTERVAL: Duration = Duration::from_millis(100);
                        let mut up_since: Vec<Option<std::time::Instant>> =
                            vec![None; slave_names.len()];
                        while !exit.load(Ordering::Relaxed) {
                            for (i, name) in slave_names.iter().enumerate() {
                                let live = if crate::bond::is_operational(name) {
                                    let since =
                                        *up_since[i].get_or_insert_with(std::time::Instant::now);
                                    since.elapsed() >= updelay
                                } else {
                                    up_since[i] = None;
                                    false
                                };
                                if slave_live[i].swap(live, Ordering::Relaxed) != live {
                                    debug!(
                                        "xdp bond: slave {name} {}",
                                        if live { "is up" } else { "went down, failing over" }
                                    );
                                }
                            }
                            thread::sleep(POLL_INTERVAL);
                        }
                    })
                    .expect("spawn xdp link monitor"),
            )
        } else {
            None
        };

        Ok(Self {
            tx_loops,
            tx_channel_cap,
            ebpfs,
            atomic_router,
            route_monitor_handle,
            slave_cnt,
            slave_live,
            link_monitor_handle,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn build(self) -> (Transmitter, XdpSender) {
        (
            Transmitter { threads: vec![] },
            XdpSender {
                slaves: vec![],
                live: Vec::new().into(),
            },
        )
    }

    #[cfg(target_os = "linux")]
    pub fn build(self) -> (Transmitter, XdpSender) {
        const DROP_CHANNEL_CAP: usize = 1_000_000;

        let Self {
            tx_loops,
            tx_channel_cap,
            ebpfs,
            atomic_router,
            route_monitor_handle,
            slave_cnt,
            slave_live,
            link_monitor_handle,
        } = self;

        let drop_queue = Arc::new(ArrayQueue::new(DROP_CHANNEL_CAP));
        let mut threads = vec![route_monitor_handle];
        if let Some(handle) = link_monitor_handle {
            threads.push(handle);
        }

        threads.push(
            Builder::new()
                .name("solTransmDrop".to_owned())
                .spawn({
                    let drop_queue = Arc::clone(&drop_queue);
                    move || {
                        loop {
                            // drop shreds in a dedicated thread so that we never lock/madvise() from
                            // the xdp thread
                            match drop_queue.pop() {
                                Some(i) => {
                                    drop(i);
                                }
                                None if Arc::strong_count(&drop_queue) == 1 => break,
                                None => {
                                    thread::sleep(Duration::from_millis(1));
                                }
                            }
                        }
                        drop(ebpfs);
                    }
                })
                .unwrap(),
        );

        let mut slaves: Vec<Vec<Sender<BytesTxPacket>>> = vec![Vec::new(); slave_cnt];
        for (i, (slave_idx, tx_loop)) in tx_loops.into_iter().enumerate() {
            let (sender, receiver) = crossbeam_channel::bounded(tx_channel_cap);
            let drop_queue = Arc::clone(&drop_queue);
            let atomic_router = Arc::clone(&atomic_router);
            threads.push(
                Builder::new()
                    .name(format!("solTransmIO{i:02}"))
                    .spawn(move || {
                        tx_loop.run(
                            receiver,
                            move |item| {
                                if let Err(item) = drop_queue.push(item) {
                                    drop(item);
                                }
                            },
                            move |ip| {
                                let r = atomic_router.load();
                                match ip {
                                    IpAddr::V4(ip) => r.route_v4(*ip).ok(),
                                    IpAddr::V6(_) => None,
                                }
                            },
                        )
                    })
                    .unwrap(),
            );
            slaves[slave_idx].push(sender);
        }

        (
            Transmitter { threads },
            XdpSender {
                slaves,
                live: slave_live,
            },
        )
    }
}

impl Transmitter {
    pub fn join(self) -> thread::Result<()> {
        for handle in self.threads {
            handle.join()?;
        }
        Ok(())
    }
}

/// Returns the IPv4 address of the master interface if the given interface is part of a bond.
#[cfg(target_os = "linux")]
pub(crate) fn master_ip_if_bonded(interface: &str) -> Option<Ipv4Addr> {
    let master_ifindex_path = format!("/sys/class/net/{interface}/master/ifindex");
    if let Ok(contents) = std::fs::read_to_string(&master_ifindex_path) {
        let idx = contents.trim().parse().unwrap_or_else(|e| {
            panic!("{master_ifindex_path} does not hold an interface index ({contents:?}): {e}")
        });
        return Some(
            NetworkDevice::new_from_index(idx)
                .and_then(|dev| dev.ipv4_addr())
                .unwrap_or_else(|e| {
                    panic!(
                        "failed to open bond master interface for {interface}: master index \
                         {idx}: {e}"
                    )
                }),
        );
    }
    None
}

#[cfg(target_os = "linux")]
const CAP_GUARD_CAPACITY: usize = 2;

#[cfg(target_os = "linux")]
#[must_use = "capabilities are dropped when the guard goes out of scope"]
struct CapGuard {
    capabilities: ArrayVec<caps::Capability, CAP_GUARD_CAPACITY>,
}

#[cfg(target_os = "linux")]
fn drop_thread_capabilities(thread_name: &str) {
    let none = caps::CapsHashSet::new();
    for set in [caps::CapSet::Effective, caps::CapSet::Permitted] {
        caps::set(None, set, &none).unwrap_or_else(|e| {
            panic!("{thread_name}: failed to clear {set:?} capability set: {e}")
        });
    }
}

#[cfg(target_os = "linux")]
impl CapGuard {
    fn raise(
        raised_capabilities: impl IntoIterator<Item = caps::Capability>,
    ) -> Result<Self, caps::errors::CapsError> {
        let mut capabilities: ArrayVec<caps::Capability, CAP_GUARD_CAPACITY> = ArrayVec::new();
        for capability in raised_capabilities {
            capabilities.try_push(capability).unwrap_or_else(|_| {
                panic!("CapGuard supports at most {CAP_GUARD_CAPACITY} capabilities")
            });
            if let Err(err) = caps::raise(None, caps::CapSet::Effective, capability) {
                for raised in capabilities.iter().rev().skip(1) {
                    let _ = caps::drop(None, caps::CapSet::Effective, *raised);
                }
                return Err(err);
            }
        }
        Ok(Self { capabilities })
    }
}

#[cfg(target_os = "linux")]
impl Drop for CapGuard {
    fn drop(&mut self) {
        for capability in self.capabilities.iter().rev() {
            caps::drop(None, caps::CapSet::Effective, *capability)
                .unwrap_or_else(|err| panic!("drop {capability:?} capability: {err}"));
        }
    }
}

