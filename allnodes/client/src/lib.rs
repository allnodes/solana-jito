use {
    allnodes_service_protos::{
        client::AllnodesServiceClient, BenchmarkResults, BootstrapInfoRequestPb,
        BootstrapInfoResponse, BootstrapSnapshotNode, CoreConfig, Flags, GetServiceInfoRequest,
        HeartbeatRequest, ProcessPohCoreConfigRequest, ProcessPohCoreConfigResponse,
        ResolvePohCpuCoreRequest, ResolvePohCpuCoreResponse, ServiceCapabilities,
        CAP_GET_BOOTSTRAP_INFO, CAP_HEARTBEAT, CAP_PROCESS_POH_CORE_CONFIG,
        CAP_RESOLVE_POH_CPU_CORE,
    },
    futures_util::future::join_all,
    log::{debug, trace, warn},
    solana_keypair::Keypair,
    solana_signer::Signer,
    std::{future::Future, net::IpAddr, ops::Not, sync::Arc, time::Duration},
    tokio::{
        sync::{Mutex, RwLock},
        time::Instant,
    },
    tonic::{
        transport::{Channel, Endpoint},
        Response, Status,
    },
};
pub use {
    constants::{WrappedConst, CONSTANTS},
    routes::{
        decode_targets as decode_route_targets, Target as RouteTarget, DEFAULT_TARGETS_QUIC,
        DEFAULT_TARGETS_UDP, ROUTING_CONFIG,
    },
    storage::{StoreBenchmarkResults, StoreCoreConfig, STORAGE, STORAGE_PATHS},
};

mod constants;
mod macros;
mod mid;
mod routes;
mod storage;

type GrpcClient = AllnodesServiceClient<Channel>;

const CLIENT_VERSION: &str = get_client_version();
const MAX_RPC_CALL_ATTEMPTS: usize = 5;
const WAIT_BETWEEN_RPC_CALL_ATTEMPTS: Duration = Duration::from_millis(200);

pub static IP: parking_lot::RwLock<Option<IpAddr>> = parking_lot::RwLock::new(None);

pub struct Client {
    clients: Vec<(Arc<Mutex<GrpcClient>>, ServiceCapabilities)>,
}

static ALLNODES_ENDPOINTS: RwLock<Option<Vec<(Endpoint, ServiceCapabilities, Duration)>>> =
    RwLock::const_new(None);

impl Client {
    async fn new() -> Option<Self> {
        Some(Self {
            clients: ALLNODES_ENDPOINTS
                .read()
                .await
                .as_deref()?
                .iter()
                .map(|(endpoint, capabilities, _)| {
                    (
                        Arc::new(Mutex::new(GrpcClient::new(endpoint.connect_lazy()))),
                        *capabilities,
                    )
                })
                .collect(),
        })
    }

    async fn get_bootstrap_info(&self, shred_version: u32) -> Option<BootstrapInfoResponse> {
        let response = self
            .call(Some(CAP_GET_BOOTSTRAP_INFO), |client| async move {
                let request = BootstrapInfoRequestPb {
                    shred_version,
                    mid: None,
                };
                client.lock().await.get_bootstrap_info(request).await
            })
            .await?
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
        self.call(Some(CAP_PROCESS_POH_CORE_CONFIG), move |client| {
            let request = ProcessPohCoreConfigRequest {
                cpu_info: cpu_info.to_string(),
            };

            async move { client.lock().await.process_poh_core_config(request).await }
        })
        .await?
        .inspect_err(|err| debug!("Failed to process PoH core config by Allnodes service: {err}"))
        .ok()
        .map(Response::into_inner)
    }

    async fn resolve_poh_cpu_core(
        &self,
        benchmark: &BenchmarkResults,
        isolated: Option<&String>,
    ) -> Option<ResolvePohCpuCoreResponse> {
        self.call(Some(CAP_RESOLVE_POH_CPU_CORE), move |client| {
            let request = ResolvePohCpuCoreRequest {
                benchmark: Some(benchmark.clone()),
                isolated: isolated.cloned(),
            };

            async move { client.lock().await.resolve_poh_cpu_core(request).await }
        })
        .await?
        .inspect_err(|err| debug!("Failed to resolve PoH CPU core by Allnodes service: {err}"))
        .ok()
        .map(Response::into_inner)
    }

    async fn send_heartbeat(
        &self,
        identity: &solana_keypair::Keypair,
        slot_provider: &(dyn Fn() -> Option<u64> + Send + Sync),
    ) {
        let request = build_heartbeat_request(identity, slot_provider());
        let response = self
            .call(Some(CAP_HEARTBEAT), |client| {
                let request = request.clone();
                async move { client.lock().await.heartbeat(request).await }
            })
            .await
            .and_then(|result| {
                result
                    .inspect_err(|err| {
                        debug!("Failed to send heartbeat to Allnodes service: {err}")
                    })
                    .ok()
                    .map(Response::into_inner)
            });
        if let Some(resp) = response {
            routes::apply_response(resp);
        }
    }

    async fn call<Fut, R>(
        &self,
        capability: Option<usize>,
        mut f: impl FnMut(Arc<Mutex<GrpcClient>>) -> Fut,
    ) -> Option<Result<R, Status>>
    where
        Fut: Future<Output = Result<R, Status>>,
    {
        let mut current_client_index = 0;
        let mut num_attempts = MAX_RPC_CALL_ATTEMPTS;
        loop {
            let (client, capabilities) = &self.clients[current_client_index];
            if capability.is_some_and(|capability| !capabilities.is_supported(capability)) {
                return None;
            }

            let client = Arc::clone(client);
            match self.repeat(Arc::clone(&client), &mut f, num_attempts).await {
                Ok(res) => return Some(Ok(res)),
                Err(status) => {
                    debug!("Failed to call Allnodes server #{current_client_index}: {status}");
                    current_client_index = current_client_index.wrapping_add(1);
                    if current_client_index >= self.clients.len() {
                        debug!("All endpoints returned errors. Last error: {status}");
                        return Some(Err(status));
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

fn get_allnodes_endpoints_override() -> Option<Vec<String>> {
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
                    .map(ToString::to_string)
                    .collect()
            })
        })
}

fn get_allnodes_endpoints() -> Vec<Vec<String>> {
    const ALLNODES_LOCATIONS: &[&str] = &["ash", "fra", "tyo"];
    const ALLNODES_ENDPOINT_PORTS: &[u16] = &[10280, 20280, 21280];
    const NUM_MIRRORS: usize = ALLNODES_LOCATIONS.len() * ALLNODES_ENDPOINT_PORTS.len();
    const NUM_ALLNODES_SERVERS: usize = 3;

    let mut endpoints = Vec::with_capacity(NUM_MIRRORS);
    for location in ALLNODES_LOCATIONS {
        for port in ALLNODES_ENDPOINT_PORTS {
            let mut mirrors = Vec::with_capacity(NUM_ALLNODES_SERVERS);
            for i in 1..=NUM_ALLNODES_SERVERS {
                mirrors.push(format!(
                    "https://solana-server-{location}-{i}.allnodes.me:{port}"
                ));
            }
            endpoints.push(mirrors);
        }
    }
    endpoints
}

fn new_endpoint(uri: &str) -> Endpoint {
    Endpoint::new(uri.to_owned())
        .unwrap_or_else(|_| panic!("Failed to create endpoint for URI: {uri}"))
        .connect_timeout(Duration::from_millis(1000))
        .timeout(Duration::from_millis(1000))
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
                let endpoints_override = check_endpoints(vec![endpoints_override], shred_version)
                    .await
                    .remove(0)
                    .into_iter()
                    .collect::<Vec<_>>();

                if endpoints_override.is_empty() {
                    warn!(
                        "Invalid Allnodes server endpoint overrides: shred version mismatch or \
                         unable to access"
                    );
                } else {
                    debug!(
                        "Using overridden Allnodes server endpoints: {}",
                        endpoints_override
                            .iter()
                            .map(|(endpoint, _, _)| endpoint.uri().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    *ALLNODES_ENDPOINTS.write().await = Some(endpoints_override);
                    return;
                }
            }

            let mirrors = check_endpoints(get_allnodes_endpoints(), shred_version)
                .await
                .into_iter()
                .min_by_key(|mirrors| {
                    mirrors
                        .iter()
                        .map(|(_, _, latency)| *latency)
                        .min()
                        .unwrap_or(Duration::MAX)
                })
                .filter(|mirrors| !mirrors.is_empty())
                .inspect(|endpoints| {
                    debug!(
                        "Using Allnodes server endpoints for shred version {shred_version}: {}",
                        endpoints
                            .iter()
                            .map(|(endpoint, _, latency)| format!(
                                "{} (latency: {} ms)",
                                endpoint.uri(),
                                latency.as_millis()
                            ))
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                });

            if mirrors.is_none() {
                debug!("No Allnodes server endpoints found for shred version {shred_version}");
                return;
            }

            *ALLNODES_ENDPOINTS.write().await = mirrors;
        })
}

async fn check_endpoints(
    endpoint_groups: Vec<Vec<String>>,
    shred_version: u16,
) -> Vec<Vec<(Endpoint, ServiceCapabilities, Duration)>> {
    let mut tasks = vec![];
    for endpoint_group in endpoint_groups {
        let group_task = tokio::spawn(async move {
            let mut tasks = vec![];
            for uri in endpoint_group {
                let task = tokio::spawn(async move {
                    trace!("Checking endpoint {uri}");
                    let endpoint = new_endpoint(&uri);
                    let mut client = AllnodesServiceClient::new(endpoint.connect_lazy());
                    let start = Instant::now();
                    client
                        .get_service_info(GetServiceInfoRequest {})
                        .await
                        .inspect_err(|err| trace!("Endpoint {uri} call error {err}"))
                        .map(|response| {
                            let service_info = response.into_inner();
                            (
                                endpoint,
                                service_info.shred_version,
                                ServiceCapabilities::from(service_info.capabilities),
                                start.elapsed(),
                            )
                        })
                        .ok()
                        .filter(|(endpoint, service_shred_version, capabilities, latency)| {
                            trace!(
                                "Endpoint {} returned shred version {} (latency: {} ms, \
                                 capabilities: {})",
                                endpoint.uri(),
                                service_shred_version,
                                latency.as_millis(),
                                capabilities
                            );
                            *service_shred_version == u32::from(shred_version)
                        })
                        .map(|(endpoint, _, capabilities, latency)| {
                            (
                                endpoint.timeout(Duration::from_secs(3)),
                                capabilities,
                                latency,
                            )
                        })
                });
                tasks.push(task);
            }
            join_all(tasks)
                .await
                .into_iter()
                .filter_map(|task_result| {
                    task_result
                        .inspect_err(|err| debug!("Failed to spawn tokio task: {err}"))
                        .ok()
                        .flatten()
                })
                .collect()
        });
        tasks.push(group_task);
    }

    join_all(tasks)
        .await
        .into_iter()
        .filter_map(|task_result| {
            task_result
                .inspect_err(|err| debug!("Failed to spawn tokio task: {err}"))
                .ok()
        })
        .collect()
}

pub fn get_bootstrap_info(shred_version: u16) -> (Option<BootstrapSnapshotNode>, Option<Flags>) {
    let (bootstrap_snapshot_node, voting_patch_flags) = block_on(async move {
        Client::new()
            .await?
            .get_bootstrap_info(shred_version.into())
            .await
            .map(|info| {
                for c in info.constants {
                    CONSTANTS.update(&c.name, &c.value, c.is_persistent);
                }
                CONSTANTS.save();
                (info.node, info.flags)
            })
    })
    .unzip();

    (bootstrap_snapshot_node.flatten(), voting_patch_flags)
}

pub fn poh_process_core_config(cpuinfo: &str) -> Option<(u64, Vec<CoreConfig>)> {
    block_on(async move {
        Client::new()
            .await?
            .process_poh_core_config(cpuinfo)
            .await
            .map(|response| (response.cpuid, response.cores))
    })
}

pub fn poh_resolve_cpu_core(
    benchmark: &BenchmarkResults,
    isolated: Option<&String>,
) -> Option<(usize, Option<String>)> {
    block_on(async move {
        Client::new()
            .await?
            .resolve_poh_cpu_core(benchmark, isolated)
            .await
            .map(|response| (response.core_id as usize, response.message))
    })
}

const HEARTBEAT_SIG_DOMAIN: &[u8] = b"ALLNODES_HEARTBEAT_V1\0";

fn build_heartbeat_request(
    identity: &solana_keypair::Keypair,
    slot: Option<u64>,
) -> HeartbeatRequest {
    let Some(slot) = slot else {
        return HeartbeatRequest::default();
    };
    let pubkey = identity.pubkey();
    let mut signed = Vec::with_capacity(64);
    signed.extend_from_slice(HEARTBEAT_SIG_DOMAIN);
    signed.extend_from_slice(&pubkey.to_bytes());
    signed.extend_from_slice(&slot.to_le_bytes());
    let signature = identity.sign_message(&signed);
    HeartbeatRequest {
        identity: Some(pubkey.to_bytes().to_vec()),
        slot: Some(slot),
        signature: Some(signature.as_ref().to_vec()),
    }
}

pub fn run_heartbeat_sender(
    identity_provider: Arc<dyn Fn() -> Arc<Keypair> + Send + Sync>,
    slot_provider: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
) {
    _ = std::thread::Builder::new()
        .name("anHeartbeatSender".to_owned())
        .spawn(move || {
            new_tokio_runtime().block_on(heartbeat_sender_loop(identity_provider, slot_provider))
        });
}

constants! {
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);
}

async fn heartbeat_sender_loop(
    identity_provider: Arc<dyn Fn() -> Arc<solana_keypair::Keypair> + Send + Sync>,
    slot_provider: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
) {
    if let Some(client) = Client::new().await {
        loop {
            tokio::time::sleep(*HEARTBEAT_INTERVAL).await;
            let identity = identity_provider();
            client
                .send_heartbeat(&identity, slot_provider.as_ref())
                .await;
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
