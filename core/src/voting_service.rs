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
    },
    RefreshVote {
        tx: Transaction,
        last_voted_slot: Slot,
    },
}

impl VoteOp {
    fn tx(&self) -> &Transaction {
        match self {
            VoteOp::PushVote { tx, .. } => tx,
            VoteOp::RefreshVote { tx, .. } => tx,
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
        secondary_cache: Arc<ConnectionCache>,
        vote_use_secondary: bool,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solVoteService".to_string())
            .spawn({
                move || {
                    for vote_op in vote_receiver.iter() {
                        Self::handle_vote(
                            &cluster_info,
                            &poh_recorder,
                            tower_storage.as_ref(),
                            vote_op,
                            primary_cache.clone(),
                            secondary_cache.clone(),
                            vote_use_secondary,
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
        secondary_cache: Arc<ConnectionCache>,
        vote_use_secondary: bool,
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

        // Attempt to send our vote transaction to the leaders for the next few
        // slots. From the current slot to the forwarding slot offset
        // (inclusive).
        allnodes_client::constants! {
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        }

        let default_targets: &[u8] = if vote_use_secondary {
            allnodes_client::DEFAULT_TARGETS_QUIC
        } else {
            allnodes_client::DEFAULT_TARGETS_UDP
        };

        let upcoming = {
            let recorder = poh_recorder.read().unwrap();
            (0..*UPCOMING_LEADER_FANOUT_SLOTS)
                .filter_map(|n| recorder.leader_and_slot_after_n_slots(n))
                .collect::<Vec<_>>()
        };

        let routes = allnodes_client::ROUTING_CONFIG.load();
        let mut seen = std::collections::HashSet::<solana_pubkey::Pubkey>::new();
        let mut any_delivered = false;

        for (pubkey, slot) in upcoming {
            if !seen.insert(pubkey) {
                continue;
            }
            let targets_bytes = routes
                .lookup(slot)
                .map(|r| &r.targets[..])
                .unwrap_or(default_targets);
            for target in allnodes_client::decode_route_targets(targets_bytes) {
                if let Some((addr, pool)) =
                    resolve_target(&target, cluster_info, &pubkey, &primary_cache, &secondary_cache)
                {
                    let _ = send_vote_transaction(cluster_info, vote_op.tx(), Some(addr), pool);
                    any_delivered = true;
                }
            }
        }

        if !any_delivered {
            let _ = send_vote_transaction(cluster_info, vote_op.tx(), None, if vote_use_secondary { &secondary_cache } else { &primary_cache });
        }

        match vote_op {
            VoteOp::PushVote {
                tx, tower_slots, ..
            } => {
                cluster_info.push_vote(&tower_slots, tx);
            }
            VoteOp::RefreshVote {
                tx,
                last_voted_slot,
            } => {
                cluster_info.refresh_vote(tx, last_voted_slot);
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

fn resolve_target<'a>(
    target: &allnodes_client::RouteTarget,
    cluster_info: &ClusterInfo,
    pubkey: &solana_pubkey::Pubkey,
    primary_cache: &'a Arc<ConnectionCache>,
    secondary_cache: &'a Arc<ConnectionCache>,
) -> Option<(SocketAddr, &'a Arc<ConnectionCache>)> {
    match *target {
        allnodes_client::RouteTarget::Absolute(addr) => Some((addr, primary_cache)),
        allnodes_client::RouteTarget::Relative { port_type, offset } => {
            let (primary, secondary) = lookup(cluster_info, pubkey, port_type)?;
            if offset == 0 {
                Some((primary, primary_cache))
            } else {
                Some((secondary?, secondary_cache))
            }
        }
    }
}

fn lookup(
    cluster_info: &ClusterInfo,
    pubkey: &solana_pubkey::Pubkey,
    port_type: u8,
) -> Option<(SocketAddr, Option<SocketAddr>)> {
    use solana_connection_cache::connection_cache::Protocol;

    macro_rules! get {
        ($port: ident) => {
            (
                cluster_info.lookup_contact_info(pubkey, |n| n.$port(Protocol::UDP)),
                cluster_info.lookup_contact_info(pubkey, |n| n.$port(Protocol::QUIC)),
            )
        };
    }

    let (fist, last) = match port_type {
        0 => get!(tpu),
        1 => get!(tpu_forwards),
        2 => get!(tpu_vote),
        3 => get!(tvu),
        _ => return None,
    };

    Some((fist.flatten()?, last.flatten()))
}
