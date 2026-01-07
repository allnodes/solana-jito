use {
    allnodes_client::poh_resolve_cpu_core,
    allnodes_service_protos::{BenchmarkResults, CoreConfig},
    core_affinity::CoreId,
    log::info,
    prost::Message,
    solana_entry::poh::compute_hash_time,
    std::thread,
};

pub fn process_core_config() -> Option<(u64, Vec<CoreConfig>)> {
    let cpu_info = read_cpu_info()?;

    allnodes_client::poh_process_core_config(&cpu_info)
}

pub fn resolve_cpu_core(
    cpuid: u64,
    cores: Vec<CoreConfig>,
    store_paths: &[std::path::PathBuf],
) -> Option<(usize, Option<String>)> {
    let filename = format!("poh-{cpuid:016x}.bin");
    let saved = store_paths
        .iter()
        .map(|path| std::fs::read(path.join(&filename)).ok())
        .find(Option::is_some)
        .flatten()
        .and_then(|bytes| decode_benchmark_results(&bytes))
        .filter(|bench| !bench.cores.is_empty() && bench.cores.len() == cores.len());
    let benchmark = match saved {
        Some(saved) => saved,
        None => {
            info!("Running PoH benchmark on {} cores...", cores.len());
            let results = test_cores(cores.clone())?;
            let encoded = encode_benchmark_results(&results);
            for path in store_paths {
                if std::fs::write(path.join(&filename), &encoded).is_ok() {
                    break;
                }
            }
            if let Some((best_vcore_id, best_score)) = results
                .cores
                .iter()
                .max_by_key(|core| core.score.unwrap_or_default())
                .map(|core| (&core.vcore_ids[0], core.score.unwrap_or_default()))
            {
                info!(
                    "Benchmarking completed. Found fastest core #{best_vcore_id} with \
                     {best_score} hashes/s."
                );
            }
            results
        }
    };

    let isolated = read_isolated();

    poh_resolve_cpu_core(&benchmark, isolated.as_ref())
}

fn read_cpu_info() -> Option<String> {
    std::fs::read_to_string("/proc/cpuinfo").ok()
}

fn read_isolated() -> Option<String> {
    std::fs::read_to_string("/sys/devices/system/cpu/isolated").ok()
}

fn encode_benchmark_results(benchmark: &BenchmarkResults) -> Vec<u8> {
    benchmark.encode_to_vec()
}

fn decode_benchmark_results(data: &[u8]) -> Option<BenchmarkResults> {
    BenchmarkResults::decode(data).ok()
}

fn test_cores(cores: Vec<CoreConfig>) -> Option<BenchmarkResults> {
    const SAMPLES: u64 = 100_000_000;
    thread::spawn(move || {
        let mut benchmark = BenchmarkResults { cores };
        for core in &mut benchmark.cores {
            let id = core.vcore_ids[0] as usize;
            core_affinity::set_for_current(CoreId { id });
            let value = (SAMPLES as f64 / compute_hash_time(SAMPLES).as_secs_f64()) as u64;
            info!("  Virtual core #{:0>3}: {value} hashes/s", id);
            core.score = Some(value);
        }

        benchmark
    })
    .join()
    .ok()
}
