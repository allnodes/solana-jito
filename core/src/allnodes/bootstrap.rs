use {
    crate::validator::ValidatorConfig,
    solana_turbine::xdp::XdpConfig,
    std::{net::IpAddr, path::Path},
};

pub fn init(
    ledger_path: &Path,
    validator_config: &mut ValidatorConfig,
    advertised_ip: IpAddr,
    enable_xdp: bool,
    xdp_interface: Option<&str>,
    xdp_zero_copy: bool,
) {
    *allnodes_client::IP.write() = Some(advertised_ip);

    {
        let mut lock = allnodes_client::STORAGE_PATHS.lock();
        let (ref mut store_paths, _) = *lock;
        if let Some(dir) = validator_config
            .identity_path
            .as_ref()
            .and_then(|path| path.parent())
        {
            store_paths.push(dir.to_path_buf());
        }
        store_paths.push(ledger_path.to_path_buf());
    }
    allnodes_client::CONSTANTS.load();

    let expected_shred_version = validator_config
        .expected_shred_version
        .expect("expected_shred_version should not be None");

    allnodes_client::resolve_endpoints(expected_shred_version);

    let mut xdp_recommended_vcore_id = 0;

    if let Some((cpuid, cores)) = allnodes_solana::poh::process_core_config() {
        if validator_config.poh_pinned_cpu_core.is_none() {
            if let Some((poh_vcore_id, message)) =
                allnodes_solana::poh::resolve_cpu_core(cpuid, cores.clone())
            {
                validator_config.poh_pinned_cpu_core = Some(poh_vcore_id);
                validator_config.poh_message = message;
            }
        }

        if let Some(poh_vcore_id) = validator_config.poh_pinned_cpu_core {
            for i in 0..cores.len() / 2 {
                if cores[i].vcore_ids.contains(&(poh_vcore_id as u32)) {
                    xdp_recommended_vcore_id =
                        cores[cores.len().saturating_sub(1)].vcore_ids[0] as usize;
                    break;
                }
            }
        }
    }

    if validator_config.retransmit_xdp.is_none()
        && (enable_xdp || xdp_interface.is_some() || xdp_zero_copy)
    {
        validator_config.retransmit_xdp = Some(XdpConfig::new(
            xdp_interface,
            vec![xdp_recommended_vcore_id],
            xdp_zero_copy,
        ));
    }
}
