pub use constants::{WrappedConst, CONSTANTS};
use {
    allnodes_service_protos::{
        client::AllnodesServiceClient, BenchmarkResults, BootstrapInfoRequestPb,
        BootstrapInfoResponse, BootstrapSnapshotNode, CoreConfig, Flags, GetShredVersionRequest,
        HeartbeatRequest, ProcessPohCoreConfigRequest, ProcessPohCoreConfigResponse,
        ResolvePohCpuCoreRequest, ResolvePohCpuCoreResponse,
    },
    futures_util::future::join_all,
    log::{debug, trace},
    std::{future::Future, ops::Not, sync::Arc, time::Duration},
    tokio::{
        sync::{Mutex, RwLock},
        time::Instant,
    },
    tonic::{
        transport::{Channel, Endpoint},
        Response, Status,
    },
};

mod constants;
mod macros;
mod mid;

const CLIENT_VERSION: &str = get_client_version();

const MAX_RPC_CALL_ATTEMPTS: usize = 5;
const WAIT_BETWEEN_RPC_CALL_ATTEMPTS: Duration = Duration::from_millis(200);

type GrpcClient = AllnodesServiceClient<Channel>;

pub struct Client {
    clients: Vec<Arc<Mutex<GrpcClient>>>,
}

static ALLNODES_ENDPOINTS: RwLock<Option<Vec<Endpoint>>> = RwLock::const_new(None);

impl Client {
    async fn new() -> Option<Self> {
        Some(Self {
            clients: ALLNODES_ENDPOINTS
                .read()
                .await
                .as_deref()?
                .iter()
                .map(|endpoint| Arc::new(Mutex::new(GrpcClient::new(endpoint.connect_lazy()))))
                .collect(),
        })
    }

    async fn get_bootstrap_info(&self, shred_version: u32) -> Option<BootstrapInfoResponse> {
        let response = self
            .call(|client| async move {
                let request = BootstrapInfoRequestPb {
                    shred_version,
                    mid: None,
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

fn get_allnodes_endpoints_override() -> Option<Vec<Endpoint>> {
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
}

fn get_allnodes_endpoints() -> Vec<Vec<Endpoint>> {
    const ALLNODES_LOCATIONS: &[&str] = &["ash", "fra", "tyo"];
    const ALLNODES_ENDPOINT_PORTS: &[u16] = &[10280, 20280, 21280];
    const NUM_MIRRORS: usize = ALLNODES_LOCATIONS.len() * ALLNODES_ENDPOINT_PORTS.len();
    const NUM_ALLNODES_SERVERS: usize = 3;

    let mut endpoints = Vec::with_capacity(NUM_MIRRORS);
    for location in ALLNODES_LOCATIONS {
        for port in ALLNODES_ENDPOINT_PORTS {
            let mut mirrors = Vec::with_capacity(NUM_ALLNODES_SERVERS);
            for i in 1..=NUM_ALLNODES_SERVERS {
                mirrors.push(configure_endpoint(
                    Endpoint::new(format!(
                        "https://solana-server-{location}-{i}.allnodes.me:{port}"
                    ))
                    .expect("Failed to parse endpoint's URL"),
                ));
            }
            endpoints.push(mirrors);
        }
    }
    endpoints
}

fn configure_endpoint(endpoint: Endpoint) -> Endpoint {
    endpoint
        .connect_timeout(Duration::from_millis(1000))
        .timeout(Duration::from_secs(3))
        .user_agent(CLIENT_VERSION)
        .unwrap_or_else(|err| panic!("Failed to set user agent {CLIENT_VERSION}. Error: {err}"))
}

pub fn resolve_endpoints(shred_version: u16) {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(async move {
            if let Some(endpoints_override) = get_allnodes_endpoints_override() {
                debug!(
                    "Using overridden server endpoints: {}",
                    endpoints_override
                        .iter()
                        .map(|endpoint| endpoint.uri().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                *ALLNODES_ENDPOINTS.write().await = Some(endpoints_override);
                return;
            }

            let mut all_endpoints = get_allnodes_endpoints();
            let mut tasks = vec![];
            for (endpoint_index, mirrors) in all_endpoints.iter().cloned().enumerate() {
                for endpoint in mirrors {
                    let endpoint = endpoint.timeout(Duration::from_millis(1000));
                    let task = tokio::spawn(async move {
                        trace!("Checking endpoint {}", endpoint.uri());
                        let mut client = AllnodesServiceClient::new(endpoint.connect_lazy());
                        let start = Instant::now();
                        client
                            .get_shred_version(GetShredVersionRequest {})
                            .await
                            .inspect_err(|err| {
                                trace!("Endpoint {} call error {err}", endpoint.uri())
                            })
                            .map(|response| {
                                (
                                    endpoint_index,
                                    response.get_ref().shred_version,
                                    start.elapsed(),
                                )
                            })
                            .ok()
                            .filter(|(_, response_shred_version, latency)| {
                                trace!(
                                    "Endpoint {} returned shred version {response_shred_version} \
                                     (latency: {} ms)",
                                    endpoint.uri(),
                                    latency.as_millis()
                                );
                                *response_shred_version == shred_version.into()
                            })
                    });
                    tasks.push(task);
                }
            }

            let Some((best_endpoint_index, _, best_latency)) = join_all(tasks)
                .await
                .into_iter()
                .filter_map(|task_result| {
                    task_result
                        .inspect_err(|err| debug!("Failed to spawn tokio task: {err}"))
                        .ok()
                        .flatten()
                })
                .min_by_key(|(_, _, latency)| *latency)
            else {
                debug!("No server endpoints found for shred version {shred_version}");
                return;
            };

            *ALLNODES_ENDPOINTS.write().await =
                Some(all_endpoints.swap_remove(best_endpoint_index)).inspect(|endpoints| {
                    debug!(
                        "Using Allnodes server endpoints for shred version {shred_version}: {} \
                         (latency: {} ms)",
                        endpoints
                            .iter()
                            .map(|endpoint| endpoint.uri().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        best_latency.as_millis()
                    );
                });
        })
}

pub fn get_bootstrap_info(shred_version: u16) -> (Option<BootstrapSnapshotNode>, Option<Flags>) {
    let (bootstrap_snapshot_node, voting_patch_flags) = block_on(async move {
        if let Some(client) = Client::new().await {
            client
                .get_bootstrap_info(shred_version.into())
                .await
                .map(|info| {
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

pub fn poh_process_core_config(cpuinfo: &str) -> Option<(u64, Vec<CoreConfig>)> {
    block_on(async move {
        if let Some(client) = Client::new().await {
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
    benchmark: &BenchmarkResults,
    isolated: Option<&String>,
) -> Option<(usize, Option<String>)> {
    block_on(async move {
        if let Some(client) = Client::new().await {
            client
                .resolve_poh_cpu_core(benchmark, isolated)
                .await
                .map(|response| (response.core_id as usize, response.message))
        } else {
            None
        }
    })
}

pub fn run_heartbeat_sender() {
    _ = std::thread::Builder::new()
        .name("anHeartbeatSender".to_owned())
        .spawn(move || new_tokio_runtime().block_on(heartbeat_sender_loop()));
}

constants! {
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);
}

async fn heartbeat_sender_loop() {
    if let Some(client) = Client::new().await {
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
