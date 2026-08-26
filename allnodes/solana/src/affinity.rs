#[cfg(target_os = "linux")]
use {
    agave_cpu_utils::{CpuId, cpu_affinity, set_cpu_affinity},
    log::{info, warn},
    solana_clap_utils::input_parsers::parse_cpu_ranges,
};

pub fn read_isolated() -> Option<String> {
    std::fs::read_to_string("/sys/devices/system/cpu/isolated").ok()
}

#[cfg(target_os = "linux")]
pub fn exclude_isolated_cpus() {
    let Some(isolated) = read_isolated() else {
        warn!("could not read the isolated CPU list; leaving the process affinity mask alone");
        return;
    };
    let isolated = isolated.trim();
    if isolated.is_empty() {
        return;
    }
    let isolated = match parse_cpu_ranges(isolated) {
        Ok(isolated) => isolated,
        Err(err) => {
            warn!("could not parse the isolated CPU list: {err}");
            return;
        }
    };
    let allowed = match cpu_affinity(None) {
        Ok(allowed) => allowed,
        Err(err) => {
            warn!("could not read the process affinity mask: {err}");
            return;
        }
    };
    let Some(housekeeping) = housekeeping_cpus(&allowed, &isolated) else {
        warn!(
            "every CPU this process may run on is isolated ({isolated:?}); leaving the affinity \
             mask alone"
        );
        return;
    };
    info!(
        "isolated CPUs {isolated:?}: process affinity mask reduced from {} to {} CPUs",
        allowed.len(),
        housekeeping.len()
    );
    if let Err(err) = set_cpu_affinity(None, housekeeping) {
        warn!("could not exclude the isolated CPUs from the process affinity mask: {err}");
    }
}

#[cfg(target_os = "linux")]
fn housekeeping_cpus(allowed: &[CpuId], isolated: &[usize]) -> Option<Vec<CpuId>> {
    let housekeeping = allowed
        .iter()
        .copied()
        .filter(|cpu| !isolated.contains(cpu))
        .collect::<Vec<_>>();

    (!housekeeping.is_empty()).then_some(housekeeping)
}

