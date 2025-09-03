use {
    allnodes_service_protos::{
        client::{self},
        BootstrapInfoRequestPb, BootstrapInfoResponse, BootstrapSnapshotNode, Flags,
    },
    log::debug,
    std::{
        collections::BTreeMap,
        future::Future,
        ops::Not,
        str::FromStr,
        sync::{Arc, LazyLock},
        time::Duration,
    },
    tokio::sync::Mutex,
    tonic::{
        transport::{Channel, Endpoint, Uri},
        Status,
    },
};

const MAX_RPC_CALL_ATTEMPTS: usize = 5;
const WAIT_BETWEEN_RPC_CALL_ATTEMPTS: Duration = Duration::from_millis(200);

type GrpcClient = client::AllnodesServiceClient<Channel>;

pub struct Client {
    shred_version: u16,
    clients: Vec<Arc<Mutex<GrpcClient>>>,
}

const NUM_ALLNODES_SERVERS: usize = 3;
static ALLNODES_ENDPOINT_PORTS: LazyLock<BTreeMap<u16, u16>> = LazyLock::new(|| {
    BTreeMap::from_iter([
        (50093, 10280), // Mainnet-beta
        (9065, 20280),  // Testnet
        (29062, 21280), // Devnet
    ])
});

static OVERRIDE_ENDPOINTS: LazyLock<Option<Vec<Endpoint>>> = LazyLock::new(|| {
    const ENV_VAR: &str = "ALLNODES_ENDPOINTS_OVERRIDE";
    std::env::var(ENV_VAR)
        .inspect_err(|err|
            if !matches!(err, std::env::VarError::NotPresent) {
                debug!("Failed to read `{ENV_VAR}` env var: {err}")
            }
        )
        .ok()
        .and_then(|overridden_endpoints| {
            overridden_endpoints.trim().is_empty().not().then(|| {
                overridden_endpoints
                    .split([',', ';'])
                    .map(|endpoint| {
                        Endpoint::new(endpoint.to_string())
                            .unwrap_or_else(
                                |err| panic!("Invalid endpoint override, check `{ENV_VAR}` env var. Error: {err}")
                            )
                    })
                    .collect()
            })
        })
});

impl Client {
    pub async fn for_shred_version(shred_version: u16) -> Option<Self> {
        if let Some(overridden_endpoints) = OVERRIDE_ENDPOINTS.as_ref() {
            debug!(
                "Using overridden server endpoints: {}",
                overridden_endpoints
                    .iter()
                    .map(|endpoint| endpoint.uri().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Some(Self {
                shred_version,
                clients: overridden_endpoints
                    .iter()
                    .map(|endpoint| Arc::new(Mutex::new(GrpcClient::new(endpoint.connect_lazy()))))
                    .collect(),
            });
        }

        get_allnodes_endpoints(shred_version)
            .or_else(|| {
                debug!("No Allnodes server endpoints found for shred version {shred_version}");
                None
            })
            .inspect(|endpoints| {
                debug!(
                    "Using Allnodes server endpoints for shred version {shred_version}: {}",
                    endpoints
                        .iter()
                        .map(|endpoint| endpoint.uri().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            })
            .map(|endpoints| Self {
                shred_version,
                clients: endpoints
                    .into_iter()
                    .map(|endpoint| Arc::new(Mutex::new(GrpcClient::new(endpoint.connect_lazy()))))
                    .collect(),
            })
    }

    pub async fn get_bootstrap_info(&self) -> Option<BootstrapInfoResponse> {
        let response = self
            .call(|client| async move {
                let request = BootstrapInfoRequestPb {
                    shred_version: self.shred_version.into(),
                };
                client.lock().await.get_bootstrap_info(request).await
            })
            .await
            .inspect_err(|err| debug!("Failed to get bootstrap info from Allnodes service: {err}"))
            .ok()?;

        TryFrom::try_from(response.into_inner())
            .inspect_err(|err| {
                debug!("Failed to convert bootstrap info from Allnodes service: {err}")
            })
            .ok()
    }

    async fn call<Fut, R>(
        &self,
        mut f: impl FnMut(Arc<Mutex<GrpcClient>>) -> Fut,
    ) -> Result<R, Status>
    where
        Fut: Future<Output = Result<R, Status>>,
    {
        let mut current_client_index = 0;
        let mut num_attempts = MAX_RPC_CALL_ATTEMPTS;
        loop {
            let client = Arc::clone(&self.clients[current_client_index]);
            match self.repeat(Arc::clone(&client), &mut f, num_attempts).await {
                Ok(res) => return Ok(res),
                Err(status) => {
                    debug!("Failed to call Allnodes server #{current_client_index}: {status}");
                    // Try the next endpoint
                    current_client_index = current_client_index.wrapping_add(1);
                    if current_client_index >= self.clients.len() {
                        debug!("All endpoints returned errors. Last error: {status}");
                        // All endpoints are down
                        return Err(status);
                    }
                    num_attempts = 1;
                }
            }
        }
    }

    async fn repeat<Fut, R>(
        &self,
        client: Arc<Mutex<GrpcClient>>,
        f: &mut impl FnMut(Arc<Mutex<GrpcClient>>) -> Fut,
        mut attempts: usize,
    ) -> Result<R, Status>
    where
        Fut: Future<Output = Result<R, Status>>,
    {
        Ok(loop {
            match f(Arc::clone(&client)).await {
                Ok(res) => break res,
                Err(err) if attempts == 0 => return Err(err),
                Err(err) => {
                    debug!("Allnodes server call failed: {err}, {attempts} attempts left");
                    attempts = attempts.saturating_sub(1);
                    tokio::time::sleep(WAIT_BETWEEN_RPC_CALL_ATTEMPTS).await
                }
            }
        })
    }
}

fn get_allnodes_endpoints(shred_version: u16) -> Option<Vec<Endpoint>> {
    let port = *ALLNODES_ENDPOINT_PORTS.get(&shred_version)?;
    Some(
        (1..=NUM_ALLNODES_SERVERS)
            .map(|i| {
                Endpoint::from(
                    Uri::from_str(&format!("https://solana-server-fra-{i}.allnodes.me:{port}"))
                        .expect("Failed to parse endpoint's URL"),
                )
            })
            .collect(),
    )
}

pub fn get_bootstrap_info(shred_version: u16) -> (Option<BootstrapSnapshotNode>, Option<Flags>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build tokio runtime");

    let (bootstrap_snapshot_node, voting_patch_flags) = runtime
        .block_on(async move {
            if let Some(client) = Client::for_shred_version(shred_version).await {
                client.get_bootstrap_info().await
            } else {
                None
            }
        })
        .map(|info| (info.node, info.flags))
        .unzip();

    (bootstrap_snapshot_node.flatten(), voting_patch_flags)
}
