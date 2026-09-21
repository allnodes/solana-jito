
mod forwarder;
mod wire;

use {
    crossbeam_channel::{bounded, Sender, TrySendError},
    solana_transaction::versioned::VersionedTransaction,
    std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, OnceLock,
        },
        thread::{Builder, JoinHandle},
    },
    wire::Sample,
};

allnodes_client::constants! {
    const QOS_WORKERS_ENV: String = String::new();
    const QOS_WORKERS_DEFAULT: usize = 1;
    const QOS_WORKERS_MAX: usize = 64;
    const QOS_CHANNEL_CAP: usize = 100_000;
}

static SENDER: OnceLock<Sender<Sample>> = OnceLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);

#[must_use = "dropping the handle stops the sampler; keep it alive for the process lifetime"]
pub struct SamplerHandle {
    shutdown: Option<Sender<()>>,
    threads: Vec<JoinHandle<()>>,
}

impl SamplerHandle {
    pub fn join(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        drop(self.shutdown.take());
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                log::debug!("qos: worker panicked during shutdown");
            }
        }
    }
}

impl Drop for SamplerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn init(collectors: Vec<SocketAddr>) -> Option<SamplerHandle> {
    spawn(collectors, workers_from_env())
}

fn spawn(sinks: Vec<SocketAddr>, workers: usize) -> Option<SamplerHandle> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return None;
    }
    if sinks.is_empty() {
        log::debug!("qos: no collector configured; idle");
        return None;
    }
    let sinks = Arc::new(sinks);

    let (sender, receiver) = bounded((*QOS_CHANNEL_CAP).clamp(1, 1_000_000));
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

    let mut threads = Vec::with_capacity(workers);
    for id in 0..workers {
        let receiver = receiver.clone();
        let shutdown_rx = shutdown_rx.clone();
        let sinks = Arc::clone(&sinks);
        match Builder::new()
            .name(format!("solQos{id:02}"))
            .spawn(move || forwarder::run(id, sinks, receiver, shutdown_rx))
        {
            Ok(thread) => threads.push(thread),
            Err(err) => log::debug!("qos: failed to spawn worker {id}: {err}"),
        }
    }
    if threads.is_empty() {
        log::debug!("qos: no workers started");
        return None;
    }

    SENDER
        .set(sender)
        .unwrap_or_else(|_| unreachable!("SENDER set once under INITIALIZED"));
    log::debug!("qos: started {} sampler worker(s)", threads.len());

    Some(SamplerHandle {
        shutdown: Some(shutdown_tx),
        threads,
    })
}

#[inline]
pub fn sampling() -> bool {
    SENDER.get().is_some()
}

#[inline]
pub fn record(slot: u64, seq: usize, body: VersionedTransaction, ok: bool) {
    let Some(sender) = SENDER.get() else {
        return;
    };
    let sample = Sample {
        slot,
        seq,
        body,
        ok,
    };
    match sender.try_send(sample) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

#[inline]
pub fn note_dropped() {
    DROPPED.fetch_add(1, Ordering::Relaxed);
}

pub fn dropped_count() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

fn workers_from_env() -> usize {
    let name = &*QOS_WORKERS_ENV;
    if name.is_empty() {
        return *QOS_WORKERS_DEFAULT;
    }
    let Ok(raw) = std::env::var(name.as_str()) else {
        return *QOS_WORKERS_DEFAULT;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return *QOS_WORKERS_DEFAULT;
    }
    match raw.parse::<usize>() {
        Ok(n) => n.clamp(1, *QOS_WORKERS_MAX),
        Err(_) => *QOS_WORKERS_DEFAULT,
    }
}

