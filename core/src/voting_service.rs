#![allow(unused)]

use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        next_leader::upcoming_leader_tpu_vote_sockets,
    },
    crossbeam_channel::Receiver,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::{FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET, Slot},
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    std::{
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
    },
    thiserror::Error,
};

pub enum VoteOp {
    PushVote {
        tx: Transaction,
        tower_slots: Vec<Slot>,
        saved_tower: SavedTowerVersions,
        node_keypair: Arc<solana_keypair::Keypair>,
        authorized_voter_keypair: Arc<solana_keypair::Keypair>,
    },
    RefreshVote {
        tx: Transaction,
        last_voted_slot: Slot,
        node_keypair: Arc<solana_keypair::Keypair>,
        authorized_voter_keypair: Arc<solana_keypair::Keypair>,
    },
}

impl VoteOp {
    fn tx(&self) -> &Transaction {
        match self {
            VoteOp::PushVote { tx, .. } => tx,
            VoteOp::RefreshVote { tx, .. } => tx,
        }
    }

    fn voted_slot(&self) -> Option<Slot> {
        match self {
            VoteOp::PushVote { tower_slots, .. } => tower_slots.last().copied(),
            VoteOp::RefreshVote {
                last_voted_slot, ..
            } => Some(*last_voted_slot),
        }
    }

    fn keypairs(&self) -> (&Arc<solana_keypair::Keypair>, &Arc<solana_keypair::Keypair>) {
        match self {
            VoteOp::PushVote {
                node_keypair,
                authorized_voter_keypair,
                ..
            }
            | VoteOp::RefreshVote {
                node_keypair,
                authorized_voter_keypair,
                ..
            } => (node_keypair, authorized_voter_keypair),
        }
    }
}

#[derive(Debug, Error)]
enum SendVoteError {
    #[error(transparent)]
    WincodeWriteError(#[from] wincode::WriteError),
    #[error("Invalid TPU address")]
    InvalidTpuAddress,
    #[error(transparent)]
    TransportError(#[from] TransportError),
}

fn send_vote_transaction(
    cluster_info: &ClusterInfo,
    transaction: &Transaction,
    tpu: Option<SocketAddr>,
    connection_cache: &Arc<ConnectionCache>,
) -> Result<(), SendVoteError> {
    let tpu = tpu
        .or_else(|| {
            cluster_info
                .my_contact_info()
                .tpu(connection_cache.protocol())
        })
        .ok_or(SendVoteError::InvalidTpuAddress)?;
    let buf = Arc::new(wincode::serialize(transaction)?);
    let client = connection_cache.get_connection(&tpu);

    client.send_data_async(buf).map_err(|err| {
        error!("Ran into an error when sending vote: {err:?} to {tpu:?}");
        SendVoteError::from(err)
    })
}

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

impl VotingService {
    pub fn new(
        vote_receiver: Receiver<VoteOp>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        tower_storage: Arc<dyn TowerStorage>,
        primary_cache: Arc<ConnectionCache>,
        secondary_cache: crate::allnodes::SecondaryCache,
        vote_use_secondary: bool,
        bank_forks: Arc<RwLock<solana_runtime::bank_forks::BankForks>>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solVoteService".to_string())
            .spawn({
                let bank_forks_for_handler = bank_forks.clone();
                move || {
                    for vote_op in vote_receiver.iter() {
                        Self::handle_vote(
                            &cluster_info,
                            &poh_recorder,
                            tower_storage.as_ref(),
                            vote_op,
                            primary_cache.clone(),
                            &secondary_cache,
                            vote_use_secondary,
                            &bank_forks_for_handler,
                        );
                    }
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    pub fn handle_vote(
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        tower_storage: &dyn TowerStorage,
        vote_op: VoteOp,
        primary_cache: Arc<ConnectionCache>,
        secondary_cache: &crate::allnodes::SecondaryCache,
        vote_use_secondary: bool,
        bank_forks: &RwLock<solana_runtime::bank_forks::BankForks>,
    ) {
        if let VoteOp::PushVote { saved_tower, .. } = &vote_op {
            let mut measure = Measure::start("tower storage save");
            if let Err(err) = tower_storage.store(saved_tower) {
                error!("Unable to save tower to storage: {err:?}");
                std::process::exit(1);
            }
            measure.stop();
            trace!("{measure}");
        }

        if let Some(slot) = vote_op.voted_slot() {
            secondary_cache.update_slot(slot);
        }

        // Attempt to send our vote transaction to the leaders for the next few
        // slots. From the current slot to the forwarding slot offset
        // (inclusive).
        allnodes_client::constants! {
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        }

        let upcoming = {
            let recorder = poh_recorder.read().unwrap();
            (0..*UPCOMING_LEADER_FANOUT_SLOTS)
                .filter_map(|n| recorder.leader_and_slot_after_n_slots(n))
                .collect::<Vec<_>>()
        };

        let routes = allnodes_client::ROUTING_CONFIG.load();

        let resigned: Option<Transaction> = if upcoming.iter().any(|(_, slot)| {
            routes
                .lookup(*slot)
                .is_some_and(|r| r.flags & (1 << 5) != 0 && r.flags & (1 << 27) == 0)
        }) {
            let bank = { bank_forks.read().unwrap().working_bank() };
            let hashes = bank.recent_blockhashes_n(2);
            hashes
                .get(1)
                .copied()
                .or_else(|| hashes.first().copied())
                .map(|hash| {
                    let (node_kp, auth_kp) = vote_op.keypairs();
                    let mut tx = vote_op.tx().clone();
                    tx.partial_sign(&[node_kp.as_ref()], hash);
                    tx.partial_sign(&[auth_kp.as_ref()], hash);
                    tx
                })
        } else {
            None
        };
        let tx_to_send: &Transaction = resigned.as_ref().unwrap_or(vote_op.tx());

        let mut seen = std::collections::HashSet::<solana_pubkey::Pubkey>::new();
        let mut any_delivered = false;
        let mut secondary = Vec::new();

        for (pubkey, slot) in upcoming {
            if !seen.insert(pubkey) {
                continue;
            }
            for destination in
                vote_destinations(cluster_info, &routes, vote_use_secondary, &pubkey, slot)
            {
                match destination {
                    Destination::Primary(addr) => {
                        let _ = send_vote_transaction(
                            cluster_info,
                            tx_to_send,
                            Some(addr),
                            &primary_cache,
                        );
                    }
                    Destination::Secondary(addr) => secondary.push(addr),
                }
                any_delivered = true;
            }
        }

        if !secondary.is_empty()
            && let Ok(wire) = wincode::serialize(tx_to_send)
        {
            secondary_cache.send(secondary, bytes::Bytes::from(wire));
        }

        if !any_delivered {
            let _ = send_vote_transaction(cluster_info, tx_to_send, None, &primary_cache);
        }

        match vote_op {
            VoteOp::PushVote {
                tx, tower_slots, ..
            } => {
                cluster_info.push_vote(&tower_slots, resigned.unwrap_or(tx));
            }
            VoteOp::RefreshVote {
                tx,
                last_voted_slot,
                ..
            } => {
                cluster_info.refresh_vote(resigned.unwrap_or(tx), last_voted_slot);
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

enum Destination {
    Primary(SocketAddr),
    Secondary(SocketAddr),
}

fn vote_destinations<'a>(
    cluster_info: &'a ClusterInfo,
    routes: &'a allnodes_client::RouteMap,
    vote_use_secondary: bool,
    leader: &'a solana_pubkey::Pubkey,
    slot: Slot,
) -> impl Iterator<Item = Destination> + 'a {
    let targets = routes
        .lookup(slot)
        .map(|route| &route.targets[..])
        .unwrap_or(if vote_use_secondary {
            allnodes_client::DEFAULT_TARGETS_QUIC
        } else {
            allnodes_client::DEFAULT_TARGETS_UDP
        });
    allnodes_client::decode_route_targets(targets)
        .filter_map(move |target| resolve_target(&target, cluster_info, leader))
}

fn resolve_target(
    target: &allnodes_client::RouteTarget,
    cluster_info: &ClusterInfo,
    pubkey: &solana_pubkey::Pubkey,
) -> Option<Destination> {
    use solana_connection_cache::connection_cache::Protocol;

    match *target {
        allnodes_client::RouteTarget::Absolute(addr) => Some(Destination::Primary(addr)),
        allnodes_client::RouteTarget::Relative { port_type, offset } => {
            if offset == 0 {
                lookup(cluster_info, pubkey, port_type, Protocol::UDP).map(Destination::Primary)
            } else {
                lookup(cluster_info, pubkey, port_type, Protocol::QUIC).map(Destination::Secondary)
            }
        }
    }
}

struct RouteTopology {
    cluster_info: Arc<ClusterInfo>,
    leader_schedule_cache: Arc<solana_ledger::leader_schedule_cache::LeaderScheduleCache>,
    vote_use_secondary: bool,
}

impl crate::allnodes::Topology for RouteTopology {
    fn identity(&self) -> Arc<solana_keypair::Keypair> {
        self.cluster_info.keypair()
    }

    fn leader_at(&self, slot: Slot) -> Option<solana_pubkey::Pubkey> {
        self.leader_schedule_cache
            .slot_leader_at(slot, None)
            .map(|leader| leader.id)
    }

    fn targets(&self, leader: &solana_pubkey::Pubkey, slot: Slot) -> Vec<SocketAddr> {
        let routes = allnodes_client::ROUTING_CONFIG.load();
        vote_destinations(
            &self.cluster_info,
            &routes,
            self.vote_use_secondary,
            leader,
            slot,
        )
        .filter_map(|destination| match destination {
            Destination::Secondary(addr) => Some(addr),
            Destination::Primary(_) => None,
        })
        .collect()
    }
}

pub(crate) fn spawn_secondary_cache(
    socket: std::net::UdpSocket,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule_cache: Arc<solana_ledger::leader_schedule_cache::LeaderScheduleCache>,
    vote_use_secondary: bool,
) -> crate::allnodes::SecondaryCache {
    crate::allnodes::SecondaryCache::spawn(
        socket,
        RouteTopology {
            cluster_info,
            leader_schedule_cache,
            vote_use_secondary,
        },
    )
}

fn lookup(
    cluster_info: &ClusterInfo,
    pubkey: &solana_pubkey::Pubkey,
    port_type: u8,
    protocol: solana_connection_cache::connection_cache::Protocol,
) -> Option<SocketAddr> {
    match port_type {
        0 => cluster_info.lookup_contact_info(pubkey, |node| node.tpu(protocol)),
        1 => cluster_info.lookup_contact_info(pubkey, |node| node.tpu_forwards(protocol)),
        2 => cluster_info.lookup_contact_info(pubkey, |node| node.tpu_vote(protocol)),
        3 => cluster_info.lookup_contact_info(pubkey, |node| node.tvu(protocol)),
        _ => None,
    }
    .flatten()
}
