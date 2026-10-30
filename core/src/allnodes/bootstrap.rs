use std::{
    net::IpAddr,
    path::{Path, PathBuf},
};

pub fn init(
    ledger_path: &Path,
    identity_path: Option<&PathBuf>,
    expected_shred_version: Option<u16>,
    advertised_ip: IpAddr,
) {
    *allnodes_client::IP.write() = Some(advertised_ip);

    {
        let mut lock = allnodes_client::STORAGE_PATHS.lock();
        let (ref mut store_paths, _) = *lock;
        if let Some(dir) = identity_path.as_ref().and_then(|path| path.parent()) {
            store_paths.push(dir.to_path_buf());
        }
        store_paths.push(ledger_path.to_path_buf());
    }
    allnodes_client::CONSTANTS.load();

    let expected_shred_version =
        expected_shred_version.expect("expected_shred_version should not be None");

    allnodes_client::resolve_endpoints(expected_shred_version);
}
