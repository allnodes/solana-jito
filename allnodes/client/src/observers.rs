use {
    crate::routes::RouteMap,
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, OnceLock},
    },
};

pub type Observers = HashMap<Pubkey, Option<SocketAddr>>;
pub type SlotOwner = Arc<dyn Fn(u64) -> Option<Pubkey> + Send + Sync>;

pub type ObserverAdmission = Arc<dyn Fn(u64) -> bool + Send + Sync>;

static OBSERVER_SET: OnceLock<ObserverSet> = OnceLock::new();

struct ObserverSet {
    configured: Observers,
    target: Arc<ArcSwap<Observers>>,
    owner_of: SlotOwner,
    admission: ObserverAdmission,
    latest: std::sync::Mutex<()>,
}

impl ObserverSet {
    fn new(
        target: Arc<ArcSwap<Observers>>,
        owner_of: SlotOwner,
        admission: ObserverAdmission,
    ) -> Self {
        Self {
            configured: Observers::clone(&target.load()),
            target,
            owner_of,
            admission,
            latest: std::sync::Mutex::default(),
        }
    }

    fn refresh_latest(&self) {
        let _one_at_a_time = self.latest.lock();
        self.refresh(&crate::routes::ROUTING_CONFIG.load());
    }

    fn refresh(&self, routes: &RouteMap) {
        let mut observers = self.configured.clone();
        for (slots, addr, _) in routes
            .observer_spans()
            .filter(|(_, _, flags)| (self.admission)(*flags))
        {
            for slot in slots {
                if let Some(owner) = (self.owner_of)(slot) {
                    observers.insert(owner, Some(addr));
                }
            }
        }
        if **self.target.load() == observers {
            return;
        }
        self.target.store(Arc::new(observers));
    }
}

pub fn install_observers(
    target: Arc<ArcSwap<Observers>>,
    owner_of: SlotOwner,
    admission: ObserverAdmission,
) {
    if OBSERVER_SET
        .set(ObserverSet::new(target, owner_of, admission))
        .is_ok()
    {
        refresh();
    }
}

pub(crate) fn refresh() {
    if let Some(set) = OBSERVER_SET.get() {
        set.refresh_latest();
    }
}
