use {
    crate::{
        PeerList, SpareReceiver,
        client::{HandshakeOutcome, PeerState, connect, reject_reverse_datagrams},
        close_codes,
        stats::ClientStats,
    },
    bytes::Bytes,
    quinn::{Connection, Endpoint, SendDatagramError},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{collections::HashMap, net::SocketAddr, sync::Arc},
    tokio::task::{JoinError, JoinSet},
};

pub(crate) type SpareTable = HashMap<Pubkey, PeerState, PubkeyHasherBuilder>;

#[derive(Default)]
pub(crate) struct Spares {
    receiver: Option<SpareReceiver>,
    state: SpareTable,
    handshakes: JoinSet<HandshakeOutcome>,
    readers: JoinSet<()>,
}

pub(crate) fn live(state: Option<&PeerState>) -> Option<&Connection> {
    match state {
        Some(PeerState::Established { connection, .. }) if connection.close_reason().is_none() => {
            Some(connection)
        }
        _ => None,
    }
}

fn close(state: PeerState, code: close_codes::Spec) {
    if let PeerState::Established { connection, .. } = state {
        code.close(&connection);
    }
}

impl Spares {
    pub(crate) fn new(receiver: SpareReceiver) -> Self {
        Self {
            receiver: Some(receiver),
            ..Self::default()
        }
    }

    pub(crate) fn table(&self) -> &SpareTable {
        &self.state
    }

    fn wanted(&self) -> Arc<HashMap<Pubkey, SocketAddr>> {
        self.receiver
            .as_ref()
            .map(|receiver| receiver.borrow().clone())
            .unwrap_or_default()
    }

    pub(crate) fn exchange(&mut self, peer: Pubkey, desired: SocketAddr, primary: &mut SpareTable) {
        let spare_address = self.wanted().get(&peer).copied();
        if let Some(PeerState::Established { target_address, .. }) = primary.get(&peer)
            && *target_address != desired
            && spare_address == Some(*target_address)
            && live(primary.get(&peer)).is_some()
            && !matches!(self.state.get(&peer), Some(PeerState::Connecting))
        {
            if let Some(moved) = primary.remove(&peer)
                && let Some(previous) = self.state.insert(peer, moved)
            {
                close(previous, close_codes::PEER_MOVED);
            }
            return;
        }
        let primary_settled = match primary.get(&peer) {
            Some(PeerState::Connecting) => true,
            Some(PeerState::Established { target_address, .. }) => {
                *target_address == desired && live(primary.get(&peer)).is_some()
            }
            None => false,
        };
        let spare_fits = matches!(
            self.state.get(&peer),
            Some(PeerState::Established { target_address, .. }) if *target_address == desired
        ) && live(self.state.get(&peer)).is_some();
        if primary_settled || !spare_fits {
            return;
        }
        if let Some(promoted) = self.state.remove(&peer)
            && let Some(previous) = primary.insert(peer, promoted)
        {
            close(previous, close_codes::PEER_MOVED);
        }
    }

    pub(crate) fn reconcile(
        &mut self,
        endpoint: &Endpoint,
        stats: &Arc<ClientStats>,
        peers: &PeerList,
        local: Pubkey,
        primary: &SpareTable,
    ) {
        while self.readers.try_join_next().is_some() {}
        let wanted = self.wanted();
        let push = peers.push_enabled;
        let gone = self
            .state
            .iter()
            .filter(|(peer, state)| match state {
                PeerState::Connecting => false,
                PeerState::Established { target_address, .. } => {
                    let standby = peers.peers.get(peer) == Some(&Some(*target_address))
                        && live(primary.get(peer)).is_none();
                    !push
                        || !peers.peers.contains_key(peer)
                        || (wanted.get(peer) != Some(target_address) && !standby)
                        || live(Some(state)).is_none()
                }
            })
            .map(|(peer, _)| *peer)
            .collect::<Vec<_>>();
        for peer in gone {
            if let Some(state) = self.state.remove(&peer) {
                close(state, close_codes::NOT_ADMITTED);
            }
        }
        if !push {
            return;
        }
        for (peer, addr) in wanted.iter() {
            if *peer == local || !peers.peers.contains_key(peer) || self.state.contains_key(peer) {
                continue;
            }
            self.handshakes.spawn(connect(
                endpoint.clone(),
                *peer,
                *addr,
                stats.clone(),
            ));
            self.state.insert(*peer, PeerState::Connecting);
        }
    }

    pub(crate) async fn next_handshake(&mut self) -> Option<Result<HandshakeOutcome, JoinError>> {
        self.handshakes.join_next().await
    }

    pub(crate) fn handshake_done(
        &mut self,
        outcome: HandshakeOutcome,
        profile: crate::epoch_word::EpochWord,
    ) -> Option<Pubkey> {
        match outcome {
            Ok((peer, connection)) => {
                if !matches!(self.state.get(&peer), Some(PeerState::Connecting)) {
                    close_codes::NOT_ADMITTED.close(&connection);
                    return None;
                }
                let quota = crate::ingress_budget::GuardBudget::handle(&connection);
                if let Some(quota) = &quota {
                    quota.refresh(profile);
                }
                self.readers
                    .spawn(reject_reverse_datagrams(peer, connection.clone()));
                self.state.insert(
                    peer,
                    PeerState::Established {
                        target_address: connection.remote_address(),
                        connection,
                        quota,
                    },
                );
                Some(peer)
            }
            Err(peer) => {
                if matches!(self.state.get(&peer), Some(PeerState::Connecting)) {
                    self.state.remove(&peer);
                }
                None
            }
        }
    }

    pub(crate) fn broadcast(
        &mut self,
        message: &Bytes,
        primary: &SpareTable,
        profile: crate::epoch_word::EpochWord,
    ) -> u64 {
        let mut sent = 0u64;
        let mut dead = Vec::new();
        for (peer, state) in self.state.iter() {
            if live(primary.get(peer)).is_some() {
                continue;
            }
            let PeerState::Established {
                connection, quota, ..
            } = state
            else {
                continue;
            };
            if let Some(quota) = quota {
                quota.refresh(profile);
            }
            match connection.send_datagram(message.clone()) {
                Ok(()) => sent = sent.saturating_add(1),
                Err(SendDatagramError::ConnectionLost(_)) => dead.push(*peer),
                Err(_) => {}
            }
        }
        for peer in dead {
            self.state.remove(&peer);
        }
        sent
    }

    pub(crate) fn refresh(&self, profile: crate::epoch_word::EpochWord) {
        for state in self.state.values() {
            if let PeerState::Established {
                quota: Some(quota), ..
            } = state
            {
                quota.refresh(profile);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.handshakes = JoinSet::new();
        self.readers = JoinSet::new();
        for (_, state) in self.state.drain() {
            close(state, close_codes::IDENTITY_CHANGED);
        }
    }
}
