use {
    allnodes_service_protos::{BenchmarkResults, CoreConfig},
    core_affinity::CoreId,
    prost::Message,
    solana_entry::poh::compute_hash_time,
    std::thread,
};

pub fn read_cpu_info() -> Option<String> {
    std::fs::read_to_string("/proc/cpuinfo").ok()
}

pub fn read_isolated() -> Option<String> {
    std::fs::read_to_string("/sys/devices/system/cpu/isolated").ok()
}

pub fn encode_benchmark_results(benchmark: &BenchmarkResults) -> Vec<u8> {
    benchmark.encode_to_vec()
}

pub fn decode_benchmark_results(data: &[u8]) -> Option<BenchmarkResults> {
    BenchmarkResults::decode(data).ok()
}

pub fn test_cores(cores: Vec<CoreConfig>) -> Option<BenchmarkResults> {
    const SAMPLES: u64 = 100_000_000;
    thread::spawn(move || {
        let mut benchmark = BenchmarkResults { cores };
        for core in &mut benchmark.cores {
            let id = core.vcore_ids[0] as usize;
            core_affinity::set_for_current(CoreId { id });
            let value = (SAMPLES as f64 / compute_hash_time(SAMPLES).as_secs_f64()) as u64;
            log::info!("  Virtual core #{:0>3}: {value} hashes/s", id);
            core.score = Some(value);
        }

        benchmark
    })
    .join()
    .ok()
}
