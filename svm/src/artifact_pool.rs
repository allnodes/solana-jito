use {
    crate::{
        account_loader::AccountLoader,
        program_loader::{filter_executable_program_accounts, load_program_with_pubkey},
    },
    solana_clock::Slot,
    solana_program_runtime::{
        loaded_programs::{
            ForkGraph, ProgramCacheForTxBatch, ProgramCacheMatchCriteria,
            ProgramRuntimeEnvironment, ProgramToLoad,
        },
        program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType},
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_timings::ExecuteTimings,
    solana_svm_type_overrides::sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    std::collections::{HashMap, hash_map::Entry},
};

const MAX_POOL_ENTRIES: usize = 256;

const MAX_POOL_BYTES: usize = 1024 * 1024 * 1024;

type PoolKey = (Pubkey, Slot, ProgramRuntimeEnvironment);

#[derive(Debug, Default)]
pub struct ArtifactPool {
    entries: RwLock<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    entries: HashMap<PoolKey, Pooled>,
    total_bytes: usize,
    next_seq: u64,
}

#[derive(Debug)]
struct Pooled {
    program: Arc<ProgramCacheEntry>,
    size: usize,
    hits: AtomicU64,
    seq: u64,
}

impl ArtifactPool {
    pub(crate) fn load<CB: TransactionProcessingCallback>(
        &self,
        account_loader: &AccountLoader<CB>,
        environment: &ProgramRuntimeEnvironment,
        program: &ProgramToLoad,
        current_slot: Slot,
        execute_timings: &mut ExecuteTimings,
    ) -> Option<(Arc<ProgramCacheEntry>, Slot)> {
        let load = |execute_timings: &mut ExecuteTimings| {
            load_program_with_pubkey(
                account_loader,
                environment,
                program.program_id,
                current_slot,
                execute_timings,
            )
        };
        let Some(requested) = deployment(account_loader, program.program_id, current_slot) else {
            return load(execute_timings);
        };
        let key = (*program.program_id, requested.1, environment.clone());
        let pooled = self.entries.read().unwrap().get(&key, requested.0);
        if let Some(pooled) = pooled {
            pooled.update_access_slot(current_slot);
            return Some((pooled, program.last_modification_slot));
        }

        let (loaded, last_modification_slot) = load(execute_timings)?;
        if let Some(size) = poolable_size(&loaded, requested)
            && deployment(
                account_loader.inner_callbacks(),
                program.program_id,
                current_slot,
            ) == Some(requested)
        {
            let _evicted = self
                .entries
                .write()
                .unwrap()
                .insert(key, Arc::clone(&loaded), size);
        }
        Some((loaded, last_modification_slot))
    }

    fn prune_by_deployment_slot(&self, deployment_slot: Slot) {
        let _pruned = self.entries.write().unwrap().prune(deployment_slot);
    }
}

impl<FG: ForkGraph> crate::transaction_processor::TransactionBatchProcessor<FG> {
    pub fn prune_artifact_pool_by_deployment_slot(&self, deployment_slot: Slot) {
        self.artifact_pool.prune_by_deployment_slot(deployment_slot);
    }
}

impl Entries {
    fn get(&self, key: &PoolKey, loader: ProgramCacheEntryOwner) -> Option<Arc<ProgramCacheEntry>> {
        let pooled = self.entries.get(key)?;
        if pooled.program.account_owner != loader {
            return None;
        }
        pooled.hits.fetch_add(1, Ordering::Relaxed);
        Some(Arc::clone(&pooled.program))
    }

    fn insert(
        &mut self,
        key: PoolKey,
        program: Arc<ProgramCacheEntry>,
        size: usize,
    ) -> Vec<Arc<ProgramCacheEntry>> {
        if size > MAX_POOL_BYTES {
            return Vec::new();
        }
        let Entry::Vacant(vacant) = self.entries.entry(key) else {
            return Vec::new();
        };
        vacant.insert(Pooled {
            program,
            size,
            hits: AtomicU64::new(0),
            seq: self.next_seq,
        });
        self.next_seq = self.next_seq.wrapping_add(1);
        self.total_bytes = self.total_bytes.saturating_add(size);

        let mut evicted = Vec::new();
        while self.entries.len() > MAX_POOL_ENTRIES || self.total_bytes > MAX_POOL_BYTES {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, pooled)| (pooled.hits.load(Ordering::Relaxed), pooled.seq))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(pooled) = self.entries.remove(&victim) {
                self.total_bytes = self.total_bytes.saturating_sub(pooled.size);
                evicted.push(pooled.program);
            }
            for pooled in self.entries.values() {
                let hits = pooled.hits.load(Ordering::Relaxed);
                pooled.hits.store(hits / 2, Ordering::Relaxed);
            }
        }
        evicted
    }

    fn prune(&mut self, deployment_slot: Slot) -> Vec<Arc<ProgramCacheEntry>> {
        let pruned: Vec<_> = self
            .entries
            .extract_if(|(_, slot, _), _| *slot == deployment_slot)
            .map(|(_, pooled)| pooled)
            .collect();
        pruned
            .into_iter()
            .map(|pooled| {
                self.total_bytes = self.total_bytes.saturating_sub(pooled.size);
                pooled.program
            })
            .collect()
    }
}

fn poolable_size(
    loaded: &ProgramCacheEntry,
    (loader, deployment_slot): (ProgramCacheEntryOwner, Slot),
) -> Option<usize> {
    let ProgramCacheEntryType::Loaded(executable) = &loaded.program else {
        return None;
    };
    (loaded.deployment_slot == deployment_slot && loaded.account_owner == loader)
        .then(|| executable.mem_size())
}

fn deployment<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_id: &Pubkey,
    current_slot: Slot,
) -> Option<(ProgramCacheEntryOwner, Slot)> {
    let program = filter_executable_program_accounts(
        callbacks,
        &ProgramCacheForTxBatch::new(current_slot),
        std::iter::once(program_id),
        true,
    )
    .pop()?;
    match program.match_criteria {
        ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(deployment_slot) => {
            Some((program.loader, deployment_slot))
        }
        ProgramCacheMatchCriteria::NoCriteria => None,
    }
}
