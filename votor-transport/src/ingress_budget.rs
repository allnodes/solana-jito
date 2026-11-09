use {
    quinn::congestion::{Controller, ControllerFactory},
    std::{any::Any, sync::Arc, time::Instant},
};

pub(crate) fn extent(profile: crate::epoch_word::EpochWord) -> u64 {
    if !profile.current() {
        return 0;
    }
    u64::MAX
        .checked_shr(profile.shift() << 6)
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Quota(Arc<std::sync::atomic::AtomicU64>);

impl Quota {
    pub(crate) fn refresh(&self, profile: crate::epoch_word::EpochWord) {
        self.0
            .store(extent(profile), std::sync::atomic::Ordering::Relaxed);
    }

    fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub(crate) struct GuardBudget {
    inner: Box<dyn Controller>,
    extent: Quota,
    newest: Option<Instant>,
}

impl GuardBudget {
    pub(crate) fn handle(connection: &quinn::Connection) -> Option<Quota> {
        connection
            .congestion_state()
            .into_any()
            .downcast::<Self>()
            .ok()
            .map(|state| state.extent)
    }

    pub(crate) fn newest(connection: &quinn::Connection) -> Option<Instant> {
        connection
            .congestion_state()
            .into_any()
            .downcast::<Self>()
            .ok()
            .and_then(|state| state.newest)
    }
}

impl Controller for GuardBudget {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &quinn_proto::RttEstimator,
    ) {
        self.newest = self.newest.max(Some(sent));
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        self.inner.window().max(self.extent.get())
    }

    fn metrics(&self) -> quinn::congestion::ControllerMetrics {
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            extent: self.extent.clone(),
            newest: self.newest,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window().max(self.extent.get())
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Default)]
pub(crate) struct GuardBudgetFactory;

impl ControllerFactory for GuardBudgetFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(GuardBudget {
            inner: Arc::new(quinn::congestion::CubicConfig::default()).build(now, current_mtu),
            extent: Quota::default(),
            newest: None,
        })
    }
}
