use allnodes_service_protos::Flags;

pub fn get_allnodes_validator_flags(shred_version: u16) -> Option<Flags> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build tokio runtime");

    runtime.block_on(async move {
        match allnodes_client::Client::for_shred_version(shred_version) {
            None => None,
            Some(client) => client
                .get_validator_flags(allnodes_service_protos::ValidatorFlagsRequest {
                    shred_version,
                })
                .await
                .inspect_err(|err| {
                    error!("Failed to get validator flags from Allnodes service: {err}")
                })
                .ok(),
        }
    })
}
