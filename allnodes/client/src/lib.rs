pub use constants::{WrappedConst, CONSTANTS};
use {
    allnodes_service_protos::{
        client::{self},
        BenchmarkResults, BootstrapInfoRequestPb, BootstrapInfoResponse, BootstrapSnapshotNode,
        CoreConfig, Flags, HeartbeatRequest, ProcessPohCoreConfigRequest,
        ProcessPohCoreConfigResponse, ResolvePohCpuCoreRequest, ResolvePohCpuCoreResponse,
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
        Response, Status,
    },
};

mod constants;
mod macros;
mod mid;

const CLIENT_VERSION: &str = get_client_version();

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
        (50093, 10280),
        (9604, 20280),
        (29062, 21280),
    ])
});

static OVERRIDE_ENDPOINTS: LazyLock<Option<Vec<Endpoint>>> = LazyLock::new(|| {
    const ENV_VAR: &str = "SOLANA_ALLNODES_ENDPOINTS_OVERRIDE";
    std::env::var(ENV_VAR)
        .inspect_err(|err| {
            if !matches!(err, std::env::VarError::NotPresent) {
                debug!("Failed to read `{ENV_VAR}` env var: {err}")
            }
        })
        .ok()
        .and_then(|overridden_endpoints| {
            overridden_endpoints.trim().is_empty().not().then(|| {
                overridden_endpoints
                    .split([',', ';'])
                    .map(|endpoint| {
                        configure_endpoint(Endpoint::new(endpoint.to_string()).unwrap_or_else(
                            |err| {
                                panic!(
                                    "Invalid endpoint override, check `{ENV_VAR}` env var. Error: \
                                     {err}"
                                )
                            },
                        ))
                    })
                    .collect()
            })
        })
});

impl Client {
    async fn for_shred_version(shred_version: u16) -> Option<Self> {
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

    async fn get_bootstrap_info(&self) -> Option<BootstrapInfoResponse> {
        let response = self
            .call(|client| async move {
                let request = BootstrapInfoRequestPb {
                    shred_version: self.shred_version.into(),
                    version: None,
                    hardware_id: None,
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

    async fn process_poh_core_config(
        &self,
        cpu_info: &str,
    ) -> Option<ProcessPohCoreConfigResponse> {
        self.call(move |client| {
            let request = ProcessPohCoreConfigRequest {
                cpu_info: cpu_info.to_string(),
            };

            async move { client.lock().await.process_poh_core_config(request).await }
        })
        .await
        .inspect_err(|err| debug!("Failed to process PoH core config by Allnodes service: {err}"))
        .ok()
        .map(Response::into_inner)
    }

    async fn resolve_poh_cpu_core(
        &self,
        benchmark: &BenchmarkResults,
        isolated: Option<&String>,
    ) -> Option<ResolvePohCpuCoreResponse> {
        self.call(move |client| {
            let request = ResolvePohCpuCoreRequest {
                benchmark: Some(benchmark.clone()),
                isolated: isolated.cloned(),
            };

            async move { client.lock().await.resolve_poh_cpu_core(request).await }
        })
        .await
        .inspect_err(|err| debug!("Failed to resolve PoH CPU core by Allnodes service: {err}"))
        .ok()
        .map(Response::into_inner)
    }

    async fn send_heartbeat(&self) {
        _ = self
            .call(|client| async move { client.lock().await.heartbeat(HeartbeatRequest {}).await })
            .await
            .inspect_err(|err| debug!("Failed to send heartbeat to Allnodes service: {err}"));
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
                    current_client_index = current_client_index.wrapping_add(1);
                    if current_client_index >= self.clients.len() {
                        debug!("All endpoints returned errors. Last error: {status}");
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
                configure_endpoint(Endpoint::from(
                    Uri::from_str(&format!("https://solana-server-fra-{i}.allnodes.me:{port}"))
                        .expect("Failed to parse endpoint's URL"),
                ))
            })
            .collect(),
    )
}

fn configure_endpoint(endpoint: Endpoint) -> Endpoint {
    endpoint
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(600))
        .user_agent(CLIENT_VERSION)
        .unwrap_or_else(|err| panic!("Failed to set user agent {CLIENT_VERSION}. Error: {err}"))
}

pub fn get_bootstrap_info(shred_version: u16) -> (Option<BootstrapSnapshotNode>, Option<Flags>) {
    let (bootstrap_snapshot_node, voting_patch_flags) = block_on(async move {
        if let Some(client) = Client::for_shred_version(shred_version).await {
            client.get_bootstrap_info().await.map(|info| {
                for c in info.constants {
                    CONSTANTS.update(&c.name, &c.value, c.is_persistent);
                }
                CONSTANTS.save();
                (info.node, info.flags)
            })
        } else {
            None
        }
    })
    .unzip();

    (bootstrap_snapshot_node.flatten(), voting_patch_flags)
}

pub fn poh_process_core_config(
    shred_version: u16,
    cpuinfo: &str,
) -> Option<(u64, Vec<CoreConfig>)> {
    block_on(async move {
        if let Some(client) = Client::for_shred_version(shred_version).await {
            client
                .process_poh_core_config(cpuinfo)
                .await
                .map(|response| (response.cpuid, response.cores))
        } else {
            None
        }
    })
}

pub fn poh_resolve_cpu_core(
    shred_version: u16,
    benchmark: &BenchmarkResults,
    isolated: Option<&String>,
) -> Option<(usize, Option<String>)> {
    block_on(async move {
        if let Some(client) = Client::for_shred_version(shred_version).await {
            client
                .resolve_poh_cpu_core(benchmark, isolated)
                .await
                .map(|response| (response.core_id as usize, response.message))
        } else {
            None
        }
    })
}

pub fn run_heartbeat_sender(shred_version: u16) {
    _ = std::thread::Builder::new()
        .name("anHeartbeatSender".to_owned())
        .spawn(move || new_tokio_runtime().block_on(heartbeat_sender_loop(shred_version)));
}

constants! {
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);
}

async fn heartbeat_sender_loop(shred_version: u16) {
    if let Some(client) = Client::for_shred_version(shred_version).await {
        loop {
            tokio::time::sleep(*HEARTBEAT_INTERVAL).await;
            client.send_heartbeat().await;
        }
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    new_tokio_runtime().block_on(future)
}

fn new_tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build tokio runtime")
}

const fn get_client_version() -> &'static str {
    let version = env!("ALLNODES_CLIENT_VERSION");
    let bytes = version.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        assert!(b >= 32 && b != 127, "Invalid client version");
        i = i.saturating_add(1);
    }
    version
}
