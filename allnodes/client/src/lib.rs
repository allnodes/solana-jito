use {
    allnodes_service_protos::{
        BootstrapInfoRequestPb, BootstrapInfoResponse, BootstrapSnapshotNode,
        CAP_GET_BOOTSTRAP_INFO, CAP_HEARTBEAT, Flags, GetServiceInfoRequest, HeartbeatRequest,
        ServiceCapabilities, client::AllnodesServiceClient,
    },
    futures_util::future::join_all,
    log::{debug, trace, warn},
    solana_keypair::Keypair,
    solana_signer::Signer,
    std::{
        future::Future,
        net::{IpAddr, SocketAddr},
        ops::Not,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    },
    tokio::{
        sync::{Mutex, RwLock},
        time::Instant,
    },
    tonic::{
        Response, Status,
        transport::{Channel, Endpoint},
    },
};
pub use {
    constants::{CONSTANTS, WrappedConst},
    observers::install_observers,
    routes::{
        ROUTING_CONFIG, RouteMap, Target as RouteTarget, decode_targets as decode_route_targets,
    },
    storage::{STORAGE, STORAGE_PATHS},
};

mod constants;
mod macros;
mod mid;
mod observers;
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

    async fn send_heartbeat(&self, identity: &Keypair, slot: u64) -> Beat {
        let request = build_heartbeat_request(identity, Some(slot));
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
        let Some(resp) = response else {
            return Beat::Unanswered;
        };
        let beat = if resp.routes.is_empty() {
            Beat::Empty
        } else {
            Beat::Routes
        };
        routes::apply_response(resp);
        beat
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

static QOS_CLASS: AtomicU64 = AtomicU64::new(0);

pub fn qos_class() -> Flags {
    Flags::from(QOS_CLASS.load(Ordering::Relaxed))
}

crate::constants! {
    const QOS_ENV: String = String::new();
    const QOS_MAX_TARGETS: usize = 16;
}

pub fn qos_targets() -> Vec<SocketAddr> {
    let name = &*QOS_ENV;
    if name.is_empty() {
        return Vec::new();
    }
    let Ok(raw) = std::env::var(name.as_str()) else {
        return Vec::new();
    };
    let mut out: Vec<SocketAddr> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Ok(addr) = token.parse::<SocketAddr>()
            && !out.contains(&addr)
        {
            out.push(addr);
        }
        if out.len() == *QOS_MAX_TARGETS {
            break;
        }
    }
    out
}

pub fn get_bootstrap_info(shred_version: u16) -> Option<BootstrapSnapshotNode> {
    let (bootstrap_snapshot_node, qos_profile) = block_on(async move {
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

    if let Some(flags) = qos_profile {
        QOS_CLASS.store(u64::from(flags), Ordering::Relaxed);
    }

    bootstrap_snapshot_node.flatten()
}

const HEARTBEAT_SIG_DOMAIN: &[u8] = b"ALLNODES_HEARTBEAT_V1\0";

fn build_heartbeat_request(identity: &Keypair, slot: Option<u64>) -> HeartbeatRequest {
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

const HEARTBEAT_RETRY: Duration = Duration::from_secs(15);
const HEARTBEAT_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Beat {
    Routes,
    Empty,
    Unanswered,
}

struct Schedule {
    interval: Duration,
    retry: Duration,
    due: Instant,
    empty_wait: Duration,
    asked_as: Option<solana_pubkey::Pubkey>,
}

impl Schedule {
    fn new(now: Instant, interval: Duration, retry: Duration) -> Self {
        Self {
            interval,
            retry,
            due: now,
            empty_wait: retry,
            asked_as: None,
        }
    }

    fn identity_changed(&mut self, identity: solana_pubkey::Pubkey, now: Instant) -> bool {
        let changed = self.asked_as.is_some_and(|asked| asked != identity);
        if changed {
            self.due = now;
        }
        changed
    }

    fn is_due(&self, now: Instant) -> bool {
        now >= self.due
    }

    fn record(&mut self, identity: solana_pubkey::Pubkey, beat: Beat, now: Instant) -> bool {
        self.asked_as = Some(identity);
        let (wait, drop_routes) = match beat {
            Beat::Routes => {
                self.empty_wait = self.retry;
                (self.interval, false)
            }
            Beat::Empty => {
                let wait = self.empty_wait;
                self.empty_wait = self.empty_wait.saturating_mul(2).min(self.interval);
                (wait, false)
            }
            Beat::Unanswered => (self.retry, true),
        };
        self.due = now.checked_add(wait).unwrap_or(now);
        drop_routes
    }
}

async fn heartbeat_sender_loop(
    identity_provider: Arc<dyn Fn() -> Arc<Keypair> + Send + Sync>,
    slot_provider: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
) {
    let Some(client) = Client::new().await else {
        return;
    };
    let mut schedule = Schedule::new(Instant::now(), *HEARTBEAT_INTERVAL, HEARTBEAT_RETRY);
    loop {
        let identity = identity_provider();
        let pubkey = identity.pubkey();
        if schedule.identity_changed(pubkey, Instant::now()) {
            routes::clear();
        }
        if schedule.is_due(Instant::now()) {
            let beat = match slot_provider() {
                Some(slot) => client.send_heartbeat(&identity, slot).await,
                None => Beat::Unanswered,
            };
            if schedule.record(pubkey, beat, Instant::now()) {
                routes::clear();
            }
        }
        tokio::time::sleep(HEARTBEAT_POLL).await;
    }
}

pub fn qos_epoch() -> u64 {
    routes::first_flags()
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
