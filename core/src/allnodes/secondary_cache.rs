use {
    allnodes_client::{MAX_PEERS, PeerSet, lookahead_slots},
    bytes::Bytes,
    solana_clock::Slot,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_tpu_client_next::{
        SendTransactionStats,
        connection_workers_scheduler::{
            BindTarget, StakeIdentity, build_client_config, setup_endpoint,
        },
        workers_cache::{WorkersCache, shutdown_worker},
    },
    std::{
        fmt::Display,
        net::{SocketAddr, UdpSocket},
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread::Builder,
        time::{Duration, Instant},
    },
    tokio::{
        sync::mpsc,
        time::{MissedTickBehavior, interval},
    },
    tokio_util::sync::CancellationToken,
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_RECONNECTS: usize = 0;
const WORKER_CHANNEL_SIZE: usize = 16;
const QUEUE_SIZE: usize = 64;
const CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(MAX_PEERS).unwrap();
const NO_SLOT: Slot = Slot::MAX;

pub trait Topology: Send + 'static {
    fn identity(&self) -> Arc<Keypair>;
    fn leader_at(&self, slot: Slot) -> Option<Pubkey>;
    fn targets(&self, leader: &Pubkey, slot: Slot) -> Vec<SocketAddr>;
}

pub struct SecondaryCache {
    slot: Arc<AtomicU64>,
    queue: Option<mpsc::Sender<Delivery>>,
}

struct Delivery {
    peers: Vec<SocketAddr>,
    wire: Bytes,
}

impl SecondaryCache {
    pub fn spawn(socket: UdpSocket, topology: impl Topology) -> Self {
        let slot = Arc::new(AtomicU64::new(NO_SLOT));
        let (sender, receiver) = mpsc::channel(QUEUE_SIZE);
        let spawned = Builder::new().name("solVoteSender".to_string()).spawn({
            let slot = Arc::clone(&slot);
            move || match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run(topology, socket, slot, receiver)),
                Err(err) => start_failed(err),
            }
        });
        let queue = match spawned {
            Ok(_) => Some(sender),
            Err(err) => {
                start_failed(err);
                None
            }
        };
        Self { slot, queue }
    }

    pub fn disabled() -> Self {
        Self {
            slot: Arc::new(AtomicU64::new(NO_SLOT)),
            queue: None,
        }
    }

    pub fn update_slot(&self, slot: Slot) {
        self.slot.store(slot, Ordering::Relaxed);
    }

    pub fn send(&self, peers: Vec<SocketAddr>, wire: Bytes) {
        let Some(queue) = &self.queue else {
            return;
        };
        if let Err(err) = queue.try_send(Delivery { peers, wire }) {
            trace!("Vote not queued: {err}");
        }
    }
}

async fn run(
    topology: impl Topology,
    socket: UdpSocket,
    slot: Arc<AtomicU64>,
    mut queue: mpsc::Receiver<Delivery>,
) {
    let mut identity = topology.identity();
    let mut endpoint = match setup_endpoint(
        BindTarget::Socket(socket),
        Some(StakeIdentity::new(&identity)),
        None,
    ) {
        Ok(endpoint) => endpoint,
        Err(err) => return start_failed(err),
    };
    let mut workers = WorkersCache::new(CACHE_CAPACITY, CancellationToken::new());
    let stats = Arc::new(SendTransactionStats::default());
    let mut kept = PeerSet::default();
    let mut refresh = interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            delivery = queue.recv() => {
                let Some(Delivery { peers, wire }) = delivery else {
                    break;
                };
                deliver(&mut workers, &peers, wire);
            }
            _ = refresh.tick() => {
                let current = topology.identity();
                if current.pubkey() != identity.pubkey() {
                    endpoint.set_default_client_config(build_client_config(
                        Some(&StakeIdentity::new(&current)),
                        None,
                    ));
                    close(&mut workers, kept.reset());
                    identity = current;
                }

                let now = Instant::now();
                let change = kept.update(&wanted(&topology, slot.load(Ordering::Relaxed)), now);
                close(&mut workers, change.remove);
                for peer in change.ensure {
                    let existed = workers.contains(&peer);
                    let replaced = workers.ensure_worker(
                        peer,
                        &endpoint,
                        WORKER_CHANNEL_SIZE,
                        WORKER_RECONNECTS,
                        HANDSHAKE_TIMEOUT,
                        Arc::clone(&stats),
                    );
                    if !existed || replaced.is_some() {
                        kept.record_attempt(peer, now);
                    }
                    if let Some(worker) = replaced {
                        shutdown_worker(worker);
                    }
                }
            }
        }
    }

    workers.shutdown().await;
    endpoint.close(0u32.into(), b"");
}

fn wanted(topology: &impl Topology, slot: Slot) -> Vec<SocketAddr> {
    let mut peers = Vec::new();
    for ahead in lookahead_slots((slot != NO_SLOT).then_some(slot)) {
        let Some(leader) = topology.leader_at(ahead) else {
            continue;
        };
        for peer in topology.targets(&leader, ahead) {
            if !peers.contains(&peer) {
                peers.push(peer);
            }
        }
    }
    peers
}

fn deliver(workers: &mut WorkersCache, peers: &[SocketAddr], wire: Bytes) {
    let mut skipped = 0;
    for peer in peers {
        if workers
            .try_send_transaction_to_address(peer, wire.clone())
            .is_err()
        {
            skipped += 1;
        }
    }
    if skipped > 0 {
        trace!("{skipped} of {} vote sends skipped", peers.len());
    }
}

fn close(workers: &mut WorkersCache, peers: impl IntoIterator<Item = SocketAddr>) {
    for peer in peers {
        if let Some(worker) = workers.pop(peer) {
            shutdown_worker(worker);
        }
    }
}

fn start_failed(err: impl Display) {
    warn!("Failed to start the vote sender: {err}");
}
