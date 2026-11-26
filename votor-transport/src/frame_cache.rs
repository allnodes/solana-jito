use {
    crate::client::PeerState,
    bytes::Bytes,
    quinn::{Connection, ConnectionStats, SendDatagramError},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{collections::HashMap, time::Duration},
    tokio::{sync::mpsc, time::Instant},
};

const MAX_ENTRIES: usize = 64;
const MIN_INTERVAL: Duration = Duration::from_millis(10);
const MAX_INTERVAL: Duration = Duration::from_millis(200);
const PEER_PHASE: Duration = Duration::from_millis(25);
const MIN_PHASE_MARGIN: Duration = Duration::from_millis(5);
const ENTRY_LEASE: Duration = Duration::from_secs(3);
const MAX_COPIES: u32 = 16;

#[derive(Clone)]
pub struct FrameCache {
    epoch_word: crate::epoch_word::EpochSource,
    sender: mpsc::Sender<FrameRecord>,
}

pub struct FrameRecord {
    pub key: Pubkey,
    pub value: Bytes,
    pub label: u64,
    pub origin: Pubkey,
    pub resident: bool,
}

impl FrameCache {
    pub fn profile(&self) -> crate::epoch_word::EpochWord {
        crate::epoch_word::EpochWord::new((self.epoch_word)())
    }

    pub fn record(&self, key: Pubkey, value: Bytes, label: u64, origin: Pubkey, resident: bool) {
        if !self.profile().current() {
            return;
        }
        let _ = self
            .sender
            .try_send(FrameRecord {
                key,
                value,
                label,
                origin,
                resident,
            })
            .inspect_err(|_| {
            });
    }
}

pub(crate) struct CacheState {
    epoch_word: crate::epoch_word::EpochSource,
    receiver: mpsc::Receiver<FrameRecord>,
    entries: Vec<CacheEntry>,
}

struct CacheEntry {
    key: Pubkey,
    label: u64,
    value: Bytes,
    visits: u32,
    scan_step: Duration,
    due: Instant,
    scan_at: Instant,
    valid_until: Instant,
    marks: Vec<InspectionMark>,
}

impl CacheEntry {
    fn matching_mark(&self, connection: &Connection, slack: u64, now: Instant) -> Option<usize> {
        let newest = crate::ingress_budget::GuardBudget::newest(connection);
        let (id, stats) = (connection.stable_id(), connection.stats());
        self.marks
            .iter()
            .position(|mark| mark.matches(id, &stats, newest, slack, now))
    }

    fn next_look(&self, now: Instant) -> Instant {
        self.marks
            .iter()
            .filter(|mark| mark.check_at > now)
            .map(|mark| mark.check_at)
            .chain([self.scan_at])
            .fold(self.valid_until, Instant::min)
    }
}

#[derive(Clone, Copy)]
struct InspectionMark {
    scope: usize,
    epoch: u64,
    sent: Instant,
    check_at: Instant,
}

impl InspectionMark {
    fn capture(connection: &Connection, now: Instant) -> Self {
        let stats = connection.stats();
        Self {
            scope: connection.stable_id(),
            epoch: stats.path.lost_packets,
            sent: now,
            check_at: after(now, inspection_span(connection)),
        }
    }

    fn matches(
        &self,
        scope: usize,
        stats: &ConnectionStats,
        newest: Option<std::time::Instant>,
        slack: u64,
        now: Instant,
    ) -> bool {
        scope == self.scope
            && now >= self.check_at
            && newest.is_some_and(|acked| acked >= self.sent.into_std())
            && stats.path.lost_packets.saturating_sub(self.epoch) <= slack
    }
}

enum CacheView<'a> {
    Live(&'a Connection),
    Connecting,
    Down,
}

impl<'a> CacheView<'a> {
    fn read(
        peer_state: &'a HashMap<Pubkey, PeerState, PubkeyHasherBuilder>,
        spares: &'a crate::spare::SpareTable,
        key: &Pubkey,
    ) -> Self {
        if crate::spare::live(peer_state.get(key)).is_none()
            && let Some(connection) = crate::spare::live(spares.get(key))
        {
            return Self::Live(connection);
        }
        match peer_state.get(key) {
            Some(PeerState::Established { connection, .. })
                if connection.close_reason().is_none() =>
            {
                Self::Live(connection)
            }
            Some(PeerState::Connecting) => Self::Connecting,
            Some(PeerState::Established { .. }) | None => Self::Down,
        }
    }
}

const K_GRANULARITY: Duration = Duration::from_millis(1);

fn scan_floor(connection: &Connection) -> Duration {
    let rtt = connection.rtt();
    rtt.saturating_add(rtt.saturating_mul(2).max(K_GRANULARITY))
        .saturating_add(PEER_PHASE)
}

fn inspection_span(connection: &Connection) -> Duration {
    let rtt = connection.rtt();
    rtt.saturating_add(PEER_PHASE)
        .saturating_add(rtt.checked_div(4).unwrap_or_default().max(MIN_PHASE_MARGIN))
}

fn next_scan(previous: Duration, connection: &Connection) -> Duration {
    previous
        .saturating_mul(2)
        .max(scan_floor(connection))
        .clamp(MIN_INTERVAL, MAX_INTERVAL)
}

fn after(now: Instant, wait: Duration) -> Instant {
    now.checked_add(wait).unwrap_or(now)
}

impl CacheState {
    pub(crate) fn new() -> (Self, FrameCache) {
        Self::with_epoch(crate::epoch_word::closed())
    }

    pub(crate) fn with_epoch(epoch_word: crate::epoch_word::EpochSource) -> (Self, FrameCache) {
        let (sender, receiver) = mpsc::channel(MAX_ENTRIES);
        let book = Self {
            epoch_word: epoch_word.clone(),
            receiver,
            entries: Vec::new(),
        };
        (book, FrameCache { sender, epoch_word })
    }

    pub(crate) fn profile(&self) -> crate::epoch_word::EpochWord {
        crate::epoch_word::EpochWord::new((self.epoch_word)())
    }

    pub(crate) async fn recv(&mut self) -> Option<FrameRecord> {
        self.receiver.recv().await
    }

    pub(crate) fn insert(
        &mut self,
        request: FrameRecord,
        peer_state: &HashMap<Pubkey, PeerState, PubkeyHasherBuilder>,
        spares: &crate::spare::SpareTable,
        now: Instant,
    ) {
        let FrameRecord {
            key,
            value,
            label,
            resident,
            ..
        } = request;
        if !self.profile().current() {
            return;
        }
        self.entries
            .retain(|entry| entry.key != key || entry.label != label);
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.remove(0);
        }
        let (visits, scan_step, marks) = match CacheView::read(peer_state, spares, &key) {
            CacheView::Live(connection) if resident => (
                1,
                next_scan(Duration::ZERO, connection),
                vec![InspectionMark::capture(connection, now)],
            ),
            CacheView::Live(_) | CacheView::Connecting | CacheView::Down => {
                (0, Duration::ZERO, Vec::new())
            }
        };
        let scan_at = after(now, scan_step);
        let mut record = CacheEntry {
            key,
            label,
            value,
            visits,
            scan_step,
            due: scan_at,
            scan_at,
            valid_until: after(now, ENTRY_LEASE),
            marks,
        };
        record.due = record.next_look(now);
        self.entries.push(record);
    }

    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.entries.iter().map(|record| record.due).min()
    }

    pub(crate) fn attach(&mut self, key: &Pubkey, now: Instant) {
        for record in self.entries.iter_mut().filter(|record| record.key == *key) {
            record.due = now;
            record.scan_at = now;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        while self.receiver.try_recv().is_ok() {}
    }

    pub(crate) fn sweep(
        &mut self,
        peer_state: &HashMap<Pubkey, PeerState, PubkeyHasherBuilder>,
        spares: &crate::spare::SpareTable,
        now: Instant,
    ) {
        let profile = self.profile();
        self.entries.retain_mut(|record| {
            if let Some(PeerState::Established {
                quota: Some(quota), ..
            }) = peer_state.get(&record.key)
            {
                quota.refresh(profile);
            }
            if !profile.current() {
                return false;
            }
            if record.due > now {
                return true;
            }
            let link = CacheView::read(peer_state, spares, &record.key);
            let selected = match link {
                CacheView::Live(connection) => record
                    .matching_mark(connection, profile.slack(), now)
                    .map(|index| (index, connection)),
                CacheView::Connecting | CacheView::Down => None,
            };
            if selected.is_some() {
                return false;
            }
            if now >= record.valid_until {
                return false;
            }
            if record.scan_at <= now {
                if record.visits >= MAX_COPIES {
                    record.scan_at = after(now, record.scan_step);
                } else {
                    match link {
                        CacheView::Live(connection) => {
                            match connection.send_datagram(record.value.clone()) {
                                Ok(()) => {
                                    record.visits = record.visits.saturating_add(1);
                                    record.marks.push(InspectionMark::capture(connection, now));
                                    record.scan_step = next_scan(record.scan_step, connection);
                                    record.scan_at = after(now, record.scan_step);
                                }
                                Err(SendDatagramError::ConnectionLost(_)) => {
                                    record.scan_at = after(now, MIN_INTERVAL);
                                }
                                #[allow(unused_variables)]
                                Err(err) => {
                                    return false;
                                }
                            }
                        }
                        CacheView::Connecting => record.scan_at = after(now, MIN_INTERVAL),
                        CacheView::Down => record.scan_at = after(now, MIN_INTERVAL),
                    }
                }
            }
            record.due = record.next_look(now);
            true
        });
    }
}
