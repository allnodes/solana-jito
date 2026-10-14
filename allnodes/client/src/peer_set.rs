use {
    solana_clock::Slot,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    std::{
        collections::{HashMap, HashSet},
        net::SocketAddr,
        time::{Duration, Instant},
    },
};

pub const LOOKAHEAD: u64 = 7;

pub const MAX_PEERS: usize = 128;

pub const RETRY_INTERVAL: Duration = Duration::from_secs(2);

const STRIDE: u64 = NUM_CONSECUTIVE_LEADER_SLOTS.get() as u64;

pub fn lookahead_slots(slot: Option<Slot>) -> impl Iterator<Item = Slot> {
    slot.into_iter().flat_map(|slot| {
        (0..LOOKAHEAD).map(move |rotation| {
            slot.saturating_add(1)
                .saturating_add(rotation.saturating_mul(STRIDE))
        })
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PeerSetChange {
    pub remove: Vec<SocketAddr>,
    pub ensure: Vec<SocketAddr>,
}

#[derive(Default)]
pub struct PeerSet {
    kept: HashSet<SocketAddr>,
    last_attempt: HashMap<SocketAddr, Instant>,
}

impl PeerSet {
    pub fn update(&mut self, wanted: &[SocketAddr], now: Instant) -> PeerSetChange {
        let remove: Vec<SocketAddr> = self
            .kept
            .iter()
            .filter(|peer| !wanted.contains(peer))
            .copied()
            .collect();
        for peer in &remove {
            self.kept.remove(peer);
        }

        self.last_attempt
            .retain(|_, at| now.saturating_duration_since(*at) < RETRY_INTERVAL);
        let mut ensure = Vec::new();
        for peer in wanted {
            if self.last_attempt.contains_key(peer) {
                continue;
            }
            if !self.kept.contains(peer) {
                if self.kept.len() >= MAX_PEERS {
                    continue;
                }
                self.kept.insert(*peer);
            }
            ensure.push(*peer);
        }

        PeerSetChange { remove, ensure }
    }

    pub fn record_attempt(&mut self, peer: SocketAddr, now: Instant) {
        self.last_attempt.insert(peer, now);
    }

    pub fn reset(&mut self) -> Vec<SocketAddr> {
        self.last_attempt.clear();
        self.kept.drain().collect()
    }
}

