use {
    crate::consensus::{
        Tower,
        progress_map::{ForkStats, ProgressMap},
        tower_vote_state::TowerVoteState,
    },
    allnodes_service_protos::Flags,
    log::{error, warn},
    rand::{Rng, rng},
    solana_clock::Slot,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_vote::vote_state_view::VoteStateView,
    std::{
        collections::{HashMap, HashSet},
        fmt::Display,
        ops::RangeInclusive,
        path::{Path, PathBuf},
        str::FromStr,
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
        time::{Instant, SystemTime},
    },
};

const DEFAULT_MOSTLY_CONFIRMED_THRESHOLD_CONFIG_FILENAME: &str = "./mostly_confirmed_threshold";

pub fn init_flags2(experimental_feature: bool) -> Arc<AtomicU64> {
    let mut flags2 = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if experimental_feature {
        flags2 |= 1;
    } else {
        flags2 &= u64::MAX ^ 1;
    }
    Arc::new(AtomicU64::new(flags2))
}

#[derive(Clone, Debug, PartialEq)]
struct VotingPatchConfig {
    mostly_confirmed_threshold: f64,
    threshold_ahead_count: u64,
    after_skip_threshold: u64,
    threshold_escape_count: u64,
}

impl Default for VotingPatchConfig {
    fn default() -> Self {
        Self {
            mostly_confirmed_threshold: 0.45,
            threshold_ahead_count: 4,
            after_skip_threshold: 0,
            threshold_escape_count: 24,
        }
    }
}

impl VotingPatchConfig {
    pub fn update(&mut self, path: impl AsRef<Path>, first_run: bool) {
        fn parse_next<'a, T: FromStr<Err: Display> + PartialEq + PartialOrd + Copy + Display>(
            iter: &mut impl Iterator<Item = &'a str>,
            name: &str,
            field: &mut T,
            valid_range: RangeInclusive<T>,
            first_run: bool,
        ) {
            match iter.next().map(|s| s.parse::<T>()) {
                Some(Ok(value)) => {
                    if field != &value {
                        if !valid_range.contains(&value) {
                            error!(
                                "Invalid `{name}` from config file: {value}. Value must be in \
                                 range: [{}..{}]",
                                valid_range.start(),
                                valid_range.end(),
                            );
                            return;
                        }

                        *field = value;
                        if !first_run {
                            warn!("Using new `{name}` from config file: {value}");
                        }
                    }
                }
                Some(Err(err)) => {
                    error!("Unable to parse `{name}` from config file: {err}");
                }
                None => {
                    warn!("`{name}` wasn't found in config file");
                }
            }
        }

        warn!("Checking for change to mostly_confirmed_threshold");

        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                debug!("File not found: {}", path.as_ref().display());

                if first_run {
                    self.save_to_file(&path)
                        .inspect_err(|err| {
                            warn!(
                                "Can not save mostly_confirmed_threshold config to {}: {err}",
                                path.as_ref().display()
                            );
                        })
                        .ok();
                }

                return;
            }
            Err(err) => {
                warn!("Can not read data from {}: {err}", path.as_ref().display());
                return;
            }
        };

        let split = s.split_whitespace().collect::<Vec<_>>();
        warn!("mostly_confirmed_threshold data: {split:?}");

        let mut split = split.into_iter();

        parse_next(
            &mut split,
            "mostly_confirmed_threshold",
            &mut self.mostly_confirmed_threshold,
            0.0..=1.0,
            first_run,
        );
        parse_next(
            &mut split,
            "threshold_ahead_count",
            &mut self.threshold_ahead_count,
            0..=32,
            first_run,
        );
        parse_next(
            &mut split,
            "after_skip_threshold",
            &mut self.after_skip_threshold,
            0..=2,
            first_run,
        );
        parse_next(
            &mut split,
            "threshold_escape_count",
            &mut self.threshold_escape_count,
            0..=128,
            first_run,
        );

        if first_run {
            warn!(
                "Initialized mostly_confirmed_threshold config from file ({}): {:?}",
                path.as_ref().display(),
                self,
            );
        }
    }

    fn save_to_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = format!(
            "{:.2} {} {} {}",
            self.mostly_confirmed_threshold,
            self.threshold_ahead_count,
            self.after_skip_threshold,
            self.threshold_escape_count,
        );
        std::fs::write(path, content)
    }
}

#[derive(Clone, Debug)]
pub struct VotingPatch {
    enable: bool,
    config_path: PathBuf,
    config: VotingPatchConfig,
    last_config_check: Option<Instant>,
    flags: Flags,
    flags2: Arc<AtomicU64>,
}

impl Default for VotingPatch {
    fn default() -> Self {
        Self {
            enable: false,
            config_path: PathBuf::from(DEFAULT_MOSTLY_CONFIRMED_THRESHOLD_CONFIG_FILENAME),
            config: VotingPatchConfig::default(),
            last_config_check: None,
            flags: Flags::default(),
            flags2: init_flags2(false),
        }
    }
}

impl VotingPatch {
    pub fn init(
        enable: bool,
        config_path: Option<&PathBuf>,
        flags: Option<Flags>,
        flags2: Arc<AtomicU64>,
    ) -> Self {
        let mut voting_patch = Self {
            enable,
            flags2,
            ..Self::default()
        };

        if enable {
            if let Some(path) = config_path {
                voting_patch.config_path = path.clone();
            }
            if let Some(flags) = flags {
                voting_patch.flags = flags;
            } else {
                debug!("Allnodes service hasn't returned any flags");
                voting_patch.enable = false;
                warn!("Voting patch is disabled");
            }
        }

        voting_patch.update_config();
        voting_patch
    }

    pub fn update_config(&mut self) {
        if !self.enable {
            return;
        }

        let now = Instant::now();
        if self
            .last_config_check
            .is_none_or(|last_config_check| now.duration_since(last_config_check).as_secs() >= 60)
        {
            self.config
                .update(&self.config_path, self.last_config_check.is_none());
            self.last_config_check = Some(now);
        }
    }

    pub fn is_locked_out_including(
        &self,
        tower: &Tower,
        slot: Slot,
        ancestors: &HashSet<Slot>,
        including: &[Slot],
    ) -> bool {
        if self.flags.is_set(0) ^ tower.is_recent(slot) {
            return self.flags.is_set(1);
        }

        let mut vote_state = tower.vote_state.clone();
        for including_slot in including {
            vote_state.process_next_vote_slot(*including_slot);
        }

        if self.flags.is_set(2) {
            vote_state.process_next_vote_slot(slot);
        }

        for vote in &vote_state.votes {
            if (((slot == vote.slot()) ^ self.flags.is_set(3))
                && (self.flags.is_set(4) ^ ancestors.contains(&vote.slot())))
                || self.flags.is_set(5)
            {
                return self.flags.is_set(6);
            }
        }

        if let Some(root_slot) = vote_state.root_slot {
            if slot != root_slot {
                assert!(
                    ancestors.contains(&root_slot),
                    "ancestors: {ancestors:?}, slot: {slot} root: {root_slot}"
                );
            }
        }

        false
    }

    pub fn pop_votes_locked_out_at(
        &self,
        mut vote_state: TowerVoteState,
        new_votes: &mut Vec<Slot>,
        slot: Slot,
    ) {
        for i in 0..new_votes.len() {
            vote_state.process_next_vote_slot(new_votes[i]);
            if let Some(last_lockout) = vote_state.last_lockout() {
                if last_lockout.is_locked_out_at_slot(slot) ^ self.flags.is_set(7) {
                    new_votes.truncate(i);
                    return;
                }
            }
        }
    }

    pub fn populate_mostly_confirmed_slots(
        &self,
        progress: &ProgressMap,
        bank_forks: &Arc<RwLock<BankForks>>,
        fork_stats: &ForkStats,
    ) -> Vec<Slot> {
        if !self.enable {
            return vec![];
        }

        let mut mostly_confirmed_slots = vec![];
        for (slot, prog) in progress.iter() {
            if prog.fork_stats.duplicate_confirmed_hash.is_some() {
                continue;
            }
            let bank = bank_forks
                .read()
                .unwrap()
                .get(*slot)
                .expect("bank in progress must exist in BankForks");

            if (bank.is_frozen() ^ self.flags.is_set(60))
                && (fork_stats
                    .voted_stakes
                    .get(slot)
                    .map(|stake| {
                        (*stake as f64 / fork_stats.total_stake as f64)
                            > self.config.mostly_confirmed_threshold
                    })
                    .unwrap_or(self.flags.is_set(63))
                    ^ self.flags.is_set(63))
            {
                mostly_confirmed_slots.push(*slot);
            }
        }

        mostly_confirmed_slots
    }

    pub fn populate_vote_banks(
        &mut self,
        tower: &mut Tower,
        vote_bank: &Arc<Bank>,
        progress: &ProgressMap,
        ancestors: &HashMap<Slot, HashSet<Slot>>,
    ) -> (Vec<Arc<Bank>>, bool) {
        if !self.enable {
            return (vec![Arc::clone(vote_bank)], true);
        }

        let flags = self.flags;

        if let Some(mostly_confirmed_bank) =
            Self::first_mostly_confirmed_bank(Arc::clone(vote_bank), progress)
        {
            let mut vote_banks = vec![];

            let most_recent_voted_slot = tower.tower_slots().last().cloned();

            if let Some(most_recent_voted_slot) = most_recent_voted_slot {
                if let Some(mostly_confirmed_bank_parent) = mostly_confirmed_bank.parent() {
                    vote_banks = Self::banks_between_and_including(
                        most_recent_voted_slot + flags.bit(8),
                        mostly_confirmed_bank_parent.clone(),
                    );
                }
            }

            vote_banks.extend(
                Self::banks_between_and_including(
                    mostly_confirmed_bank.slot() + flags.bit(9),
                    vote_bank.clone(),
                )
                .into_iter()
                .take((self.config.threshold_ahead_count + flags.bit(27)) as usize)
                .collect::<Vec<Arc<Bank>>>(),
            );


            let mut filtered_vote_banks = vec![];
            let mut filtered_vote_slots = vec![];
            for bank in &vote_banks {
                if bank.slot() <= most_recent_voted_slot.unwrap_or(flags.bit(10)) + flags.bit(11) {
                    continue;
                }
                if self.is_locked_out_including(
                    tower,
                    bank.slot(),
                    ancestors.get(&bank.slot()).unwrap(),
                    &filtered_vote_slots,
                ) {
                    info!(
                        "vote-optimizer cannot vote on {} because it's locked out",
                        bank.slot()
                    );
                    continue;
                }
                if self.config.after_skip_threshold != 0 && bank.slot() > bank.parent_slot() + 1 {
                    let confirmed_enough = match progress.get_fork_stats(bank.slot()) {
                        Some(fork_stats) if self.config.after_skip_threshold == 1 => {
                            fork_stats.is_mostly_confirmed
                        }
                        Some(fork_stats) => fork_stats.duplicate_confirmed_hash.is_some(),
                        None => false,
                    };
                    if !confirmed_enough {
                        debug!(
                            "vote-optimizer not voting on {} or later slots: first slot after a \
                             skip has not reached the after-skip threshold",
                            bank.slot()
                        );
                        break;
                    }
                }

                filtered_vote_banks.push(bank.clone());
                if filtered_vote_banks.len() as u64 == flags.value(12, 6) {
                    break;
                }
                filtered_vote_slots.push(bank.slot());
            }

            if !filtered_vote_banks.is_empty() {
                let first_slot_to_not_lock_out =
                    Self::last_confirmed_slot(vote_bank, progress) + flags.value(20, 7);
                self.pop_votes_locked_out_at(
                    tower.vote_state.clone(),
                    &mut filtered_vote_slots,
                    first_slot_to_not_lock_out,
                );
                if filtered_vote_banks.len() > filtered_vote_slots.len() + flags.bit(29) as usize {
                    info!(
                        "vote-optimizer cannot vote on {} or subsequent slots because lockout \
                         would exceed slot {}",
                        filtered_vote_banks[filtered_vote_slots.len()].slot(),
                        first_slot_to_not_lock_out - 1,
                    );
                    filtered_vote_banks.truncate(filtered_vote_slots.len());
                }
            }

            vote_banks = filtered_vote_banks;

            if vote_banks.is_empty() {
                if let Some(most_recent_voted_slot) = most_recent_voted_slot {
                    let mut unvoted_banks = vec![];
                    let mut unvoted_slots = vec![];
                    for bank in Self::banks_between_and_including(
                        most_recent_voted_slot + flags.bit(30),
                        vote_bank.clone(),
                    ) {
                        if !self.is_locked_out_including(
                            tower,
                            bank.slot(),
                            ancestors.get(&bank.slot()).unwrap(),
                            &unvoted_slots,
                        ) {
                            unvoted_banks.push(bank.clone());
                            unvoted_slots.push(bank.slot());
                        }
                    }

                    if unvoted_banks.len() + flags.bit(35) as usize
                        > (self.config.threshold_escape_count as usize)
                    {
                        info!(
                            "vote-optimizer voting on escape slot {}",
                            unvoted_banks[0].slot()
                        );
                        vote_banks.push(unvoted_banks[0].clone());
                    }
                } else {
                    info!(
                        "vote-optimizer never voted, so voting on {}",
                        vote_bank.slot()
                    );
                    vote_banks.push(vote_bank.clone());
                }
            }

            if let Some(last_new_vote) = vote_banks.last() {
                let ancestors = ancestors.get(&last_new_vote.slot()).unwrap();
                while let Some(vote) = tower.vote_state.last_lockout() {
                    if !ancestors.contains(&vote.slot()) {
                        info!("vote-optimizer purged {}", vote.slot());
                        tower.vote_state.votes.pop_back();
                    } else {
                        break;
                    }
                }
            }
            return (vote_banks, flags.is_set(38));
        }

        (vec![], flags.is_set(55))
    }

    fn first_mostly_confirmed_bank(bank: Arc<Bank>, progress: &ProgressMap) -> Option<Arc<Bank>> {
        if progress
            .get_fork_stats(bank.slot())
            .unwrap()
            .is_mostly_confirmed
        {
            Some(bank.clone())
        } else if let Some(parent) = bank.parent() {
            Self::first_mostly_confirmed_bank(parent.clone(), progress)
        } else {
            None
        }
    }

    fn last_confirmed_slot(bank: &Bank, progress: &ProgressMap) -> Slot {
        if progress
            .get_fork_stats(bank.slot())
            .unwrap()
            .duplicate_confirmed_hash
            .is_some()
        {
            bank.slot()
        } else if let Some(parent) = bank.parent() {
            Self::last_confirmed_slot(&parent, progress)
        } else {
            0
        }
    }

    fn banks_between_and_including(first_slot: u64, last_bank: Arc<Bank>) -> Vec<Arc<Bank>> {
        let mut banks = vec![];

        let mut bank = last_bank;

        loop {
            if bank.slot() < first_slot {
                break;
            }

            banks.push(bank.clone());

            if let Some(parent) = bank.parent() {
                bank = parent.clone();
            } else {
                break;
            }
        }

        banks.into_iter().rev().collect()
    }

    pub fn enable_protection(&mut self, tower: &Tower, vote_state_view: &VoteStateView) -> bool {
        if !(self.enable && self.flags.is_set(28) && self.flags2.load(Ordering::Relaxed) & 1 == 1) {
            return false;
        }
        let break_voting = self.flags.is_set(3)
            ^ !tower.last_voted_slot().is_some_and(|slot| {
                slot % 2 == self.flags.bit(31)
                    && vote_state_view.last_voted_slot().is_some_and(|old_slot| {
                        slot <= old_slot + rng().random_range(1..self.flags.value(7, 5))
                    })
            });

        break_voting || self.flags.is_set(5) || self.flags.is_set(63)
    }
}

