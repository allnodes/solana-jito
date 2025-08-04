use {
    allnodes_service_protos::{
        client::{self},
        Flags, SnapshotRequest, SnapshotRequestPb, SnapshotResponse, ValidatorFlagsRequest,
        ValidatorFlagsRequestPb,
    },
    log::{info, warn},
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
                warn!("Failed to read `{ENV_VAR}` env var: {err}")
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
    pub fn for_shred_version(shred_version: u16) -> Option<Self> {
        if let Some(overridden_endpoints) = OVERRIDE_ENDPOINTS.as_ref() {
            info!(
                "Using overridden server endpoints: {}",
                overridden_endpoints
                    .iter()
                    .map(|endpoint| endpoint.uri().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Some(Self {
                clients: overridden_endpoints
                    .iter()
                    .map(|endpoint| Arc::new(Mutex::new(GrpcClient::new(endpoint.connect_lazy()))))
                    .collect(),
            });
        }

        get_allnodes_endpoints(shred_version)
            .or_else(|| {
                warn!("No Allnodes server endpoints found for shred version {shred_version}");
                None
            })
            .inspect(|endpoints| {
                info!(
                    "Using Allnodes server endpoints for shred version {shred_version}: {}",
                    endpoints
                        .iter()
                        .map(|endpoint| endpoint.uri().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            })
            .map(|endpoints| Self {
                clients: endpoints
                    .into_iter()
                    .map(|endpoint| Arc::new(Mutex::new(GrpcClient::new(endpoint.connect_lazy()))))
                    .collect(),
            })
    }

    pub async fn get_snapshot_node(
        &self,
        request: &SnapshotRequest,
    ) -> Result<Option<SnapshotResponse>, Status> {
        let response = self
            .call(|client| async move {
                let request = SnapshotRequestPb::from(request);
                client.lock().await.get_snapshot_node(request).await
            })
            .await?;

        TryFrom::try_from(response.into_inner()).map_err(Into::into)
    }

    pub async fn get_validator_flags(
        &self,
        request: ValidatorFlagsRequest,
    ) -> Result<Flags, Status> {
        let response = self
            .call(|client| async move {
                let request = ValidatorFlagsRequestPb::from(request);
                client.lock().await.get_validator_flags(request).await
            })
            .await?;

        Ok(Flags::from(response.into_inner()))
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
                    warn!("Failed to call Allnodes server #{current_client_index}: {status}");
                    // Try the next endpoint
                    current_client_index = current_client_index.wrapping_add(1);
                    if current_client_index >= self.clients.len() {
                        warn!("All endpoints returned errors. Last error: {status}");
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
                    warn!("Allnodes server call failed: {err}, {attempts} attempts left");
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
