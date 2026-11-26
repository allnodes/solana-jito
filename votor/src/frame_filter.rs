use {
    agave_votor_messages::{vote::Vote, wire::VersionedWireConsensusMessage},
    agave_votor_transport::frame_cache::FrameCache,
    bytes::Bytes,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    std::sync::Arc,
};


pub struct FrameFilter {
    directory: Arc<LeaderScheduleCache>,
    cache: FrameCache,
}

impl FrameFilter {
    pub fn new(directory: Arc<LeaderScheduleCache>, cache: FrameCache) -> Self {
        Self {
            directory,
            cache,
        }
    }

    pub(crate) fn stage(
        &self,
        vote: &Vote,
        message: &VersionedWireConsensusMessage,
        origin: &Pubkey,
        resident: bool,
    ) {
        let slot = vote.slot();
        let profile = self.cache.profile();
        let followed = profile
            .mask()
            .checked_shr(ordinal(vote))
            .unwrap_or_default()
            & 1
            == 1;
        if !profile.current() || !followed {
            return;
        }
        let Some(leader) = self
            .directory
            .slot_leader_at(slot.saturating_add(profile.stride()), None)
            .map(|leader| leader.id)
        else {
            return;
        };
        if leader == *origin {
            return;
        }
        if let Ok(bytes) = wincode::serialize(message) {
            self.cache
                .record(leader, Bytes::from(bytes), slot, *origin, resident);
        }
    }
}

fn ordinal(vote: &Vote) -> u32 {
    match vote {
        Vote::Notarize(_) => 0,
        Vote::Finalize(_) => 1,
        Vote::Skip(_) => 2,
        Vote::NotarizeFallback(_) => 3,
        Vote::SkipFallback(_) => 4,
        Vote::Genesis(_) => 5,
    }
}
