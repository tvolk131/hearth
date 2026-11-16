use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointAddr, SecretKey, endpoint::presets};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::timeout,
};

use super::{
    protocol::*,
    store::{Binding, Store, failure},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, clap::ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Loopback,
    Direct,
    Public,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub schema: u32,
    pub binding: Binding,
    pub host_socket: PathBuf,
    pub network: Network,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_network: Option<Network>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub schema: u32,
    pub endpoint: EndpointAddr,
    pub environment: String,
    pub network: Network,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema: u32,
    pub identity: PathBuf,
    pub server: Descriptor,
}

/// Public identity metadata only. Routes and private-key paths are not authority.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileBinding {
    pub server_id: String,
    pub environment: String,
    pub device_id: String,
}

fn binding_for_key(profile: &Profile, key: &SecretKey) -> Result<ProfileBinding> {
    ensure!(
        profile.schema == 1 && profile.server.schema == 1,
        "unsupported profile schema"
    );
    ensure!(
        super::host::valid_environment(&profile.server.environment),
        "invalid profile environment"
    );
    Ok(ProfileBinding {
        server_id: profile.server.endpoint.id.to_string(),
        environment: profile.server.environment.clone(),
        device_id: key.public().to_string(),
    })
}

pub fn profile_binding(profile: &Profile) -> Result<ProfileBinding> {
    binding_for_key(profile, &load_identity(&profile.identity)?)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    schema: u32,
    environment: String,
    request: Request,
}

pub fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir(path)
        .with_context(|| format!("create new private directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::File::open(path.parent().context("file has no parent")?)?.sync_all()?;
    Ok(())
}

pub fn generate_identity(path: &Path) -> Result<SecretKey> {
    let key = SecretKey::generate();
    write_new(path, &key.to_bytes())?;
    Ok(key)
}

pub fn load_identity(path: &Path) -> Result<SecretKey> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "identity must be a regular private file (chmod 600), not a symlink"
    );
    let bytes: [u8; 32] = fs::read(path)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("identity file must contain exactly 32 bytes"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

pub async fn endpoint(
    key: SecretKey,
    network: Network,
    port: u16,
    server: bool,
) -> Result<Endpoint> {
    let mut builder = match network {
        Network::Public => Endpoint::builder(presets::N0),
        Network::Loopback | Network::Direct => Endpoint::builder(presets::Minimal),
    }
    .secret_key(key)
    .alpns(if server {
        vec![ALPN.to_vec()]
    } else {
        Vec::new()
    });
    if network != Network::Public {
        let address: SocketAddr = format!(
            "{}:{port}",
            if network == Network::Loopback {
                "127.0.0.1"
            } else {
                "0.0.0.0"
            }
        )
        .parse()?;
        builder = builder
            .clear_ip_transports()
            .clear_relay_transports()
            .clear_address_lookup()
            .bind_addr(address)?;
    }
    Ok(builder.bind().await?)
}

pub fn initialize(directory: &Path, config: &ServerConfig, admin: &str) -> Result<()> {
    config.binding.validate()?;
    ensure!(
        config.network != Network::Public || config.binding.managed.is_some(),
        "public control transport requires the managed backend"
    );
    ensure!(
        config.binding.physical_trial.is_none() || config.network == Network::Loopback,
        "physical trial transport must remain loopback-only"
    );
    ensure!(
        config.schema == 1 && config.host_socket.is_absolute(),
        "invalid controller configuration"
    );
    ensure!(
        config.binding.environment.len() == 32
            && config
                .binding
                .environment
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "environment must be a 32-character lower-case hex nonce"
    );
    private_directory(directory)?;
    generate_identity(&directory.join("identity.key"))?;
    Store::create(&directory.join("state.redb"), config.binding.clone(), admin)?;
    fs::set_permissions(
        directory.join("state.redb"),
        fs::Permissions::from_mode(0o600),
    )?;
    write_new(
        &directory.join("config.json"),
        &serde_json::to_vec_pretty(config)?,
    )?;
    write_new(&directory.join("initialized"), b"controller-v1\n")?;
    Ok(())
}

pub fn open(directory: &Path) -> Result<(ServerConfig, Arc<Store>, SecretKey)> {
    super::maintenance::ensure_not_quarantined(directory)?;
    ensure!(
        fs::read(directory.join("initialized"))? == b"controller-v1\n",
        "controller initialization incomplete; refusing automatic repair"
    );
    let config: ServerConfig = serde_json::from_slice(&fs::read(directory.join("config.json"))?)?;
    ensure!(config.schema == 1, "unsupported controller schema");
    ensure!(
        config.binding.physical_trial.is_none() || config.network == Network::Loopback,
        "physical trial transport must remain loopback-only"
    );
    ensure!(
        config.network != Network::Public || config.binding.managed.is_some(),
        "public control transport requires the managed backend"
    );
    let store = Arc::new(Store::open(&directory.join("state.redb"), &config.binding)?);
    let key = load_identity(&directory.join("identity.key"))?;
    Ok((config, store, key))
}

pub async fn rpc(profile: &Profile, request: Request) -> Result<Response> {
    rpc_bound(profile, request, None).await
}

pub async fn rpc_bound(
    profile: &Profile,
    request: Request,
    expected: Option<&ProfileBinding>,
) -> Result<Response> {
    // Load the key once, check the exact profile/key used below, then connect.
    // A profile-info preflight alone would leave a retargeting race.
    let key = load_identity(&profile.identity)?;
    let actual = binding_for_key(profile, &key)?;
    if let Some(expected) = expected {
        ensure!(
            actual == *expected,
            "profile identity changed; no request was sent"
        );
    }
    let request = serde_json::to_vec(&WireRequest {
        schema: 1,
        environment: profile.server.environment.clone(),
        request,
    })?;
    ensure!(
        request.len() <= MAX_REQUEST,
        "request exceeds protocol size limit"
    );
    let endpoint = endpoint(key, profile.server.network, 0, false).await?;
    // Which step a timeout interrupted: no connection points at the network,
    // a missing reply at the controller.
    let stage = std::cell::Cell::new("connecting");
    let result = timeout(Duration::from_secs(25), async {
        let connection = endpoint
            .connect(profile.server.endpoint.clone(), ALPN)
            .await?;
        stage.set("opening a stream");
        let (mut send, mut receive) = connection.open_bi().await?;
        stage.set("sending the request");
        send.write_all(&request).await?;
        send.finish()?;
        stage.set("waiting for the reply");
        let bytes = receive.read_to_end(MAX_RESPONSE).await?;
        let response: Response = serde_json::from_slice(&bytes)?;
        connection.close(0u32.into(), b"response received");
        Ok::<_, anyhow::Error>(response)
    })
    .await
    .with_context(|| {
        format!(
            "controller request timed out while {}; retry mutations only with the same request_id",
            stage.get()
        )
    });
    endpoint.close().await;
    result?
}

/// Connections still in the QUIC/TLS handshake. Nothing is known about the
/// peer yet, so these are cheap to take and released as soon as the
/// handshake finishes or [`HANDSHAKE_TIMEOUT`] passes.
pub const HANDSHAKE_SLOTS: usize = 64;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections from enrolled devices that may run a request at once.
pub const ENROLLED_SLOTS: usize = 32;
/// Share of [`ENROLLED_SLOTS`] that one device may hold, so one device
/// cannot starve the others.
pub const DEVICE_SLOTS: usize = 8;
/// Upper bound for one enrolled connection, from admission to close.
pub const ENROLLED_TIMEOUT: Duration = Duration::from_secs(30);
/// Unknown or revoked peers that get a full "not enrolled" reply at once.
/// Beyond this, they are closed straight away with the same reason.
pub const REJECTION_SLOTS: usize = 4;
pub const REJECTION_TIMEOUT: Duration = Duration::from_secs(3);

/// Application close codes the controller sends.
const CLOSE_DONE: u32 = 0;
const CLOSE_NOT_ENROLLED: u32 = 1;
const CLOSE_BUSY: u32 = 2;
const CLOSE_TIMEOUT: u32 = 3;
const NOT_ENROLLED: &str = "device is not enrolled or has been revoked";

/// Per-device count of held enrolled connections. Only enrolled devices get
/// an entry, and an entry is removed when its last connection ends.
#[derive(Default)]
struct DeviceSlots(std::sync::Mutex<HashMap<String, usize>>);

struct DeviceSlot {
    slots: Arc<DeviceSlots>,
    peer: String,
}

impl DeviceSlots {
    fn try_acquire(self: &Arc<Self>, peer: &str) -> Option<DeviceSlot> {
        let mut held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = held.entry(peer.to_owned()).or_default();
        if *count >= DEVICE_SLOTS {
            return None;
        }
        *count += 1;
        Some(DeviceSlot {
            slots: self.clone(),
            peer: peer.to_owned(),
        })
    }
}

impl Drop for DeviceSlot {
    fn drop(&mut self) {
        let mut held = self.slots.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = held.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.peer);
            }
        }
    }
}

struct Admission {
    handshakes: Arc<Semaphore>,
    enrolled: Arc<Semaphore>,
    rejections: Arc<Semaphore>,
    devices: Arc<DeviceSlots>,
}

impl Admission {
    fn new() -> Self {
        Self {
            handshakes: Arc::new(Semaphore::new(HANDSHAKE_SLOTS)),
            enrolled: Arc::new(Semaphore::new(ENROLLED_SLOTS)),
            rejections: Arc::new(Semaphore::new(REJECTION_SLOTS)),
            devices: Arc::default(),
        }
    }
}

fn encode(result: Result<serde_json::Value, ApiError>) -> Result<Vec<u8>> {
    let response = serde_json::to_vec(&Response::from_result(result))?;
    if response.len() <= MAX_RESPONSE {
        return Ok(response);
    }
    Ok(serde_json::to_vec(&Response::from_result(Err(failure(
        "unavailable",
        "response exceeds protocol limit; administrator must archive history",
    ))))?)
}

/// How long to let a peer close the connection itself after it has
/// acknowledged the response. Closing from this side any earlier can make the
/// peer's QUIC stack drop the response before its application reads it.
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Write the one response, wait until the peer has acknowledged it, then give
/// the peer up to [`CLOSE_GRACE`] to close. The caller closes the connection
/// afterwards, so a peer that never closes holds its slot only briefly.
async fn respond(
    connection: &iroh::endpoint::Connection,
    mut send: iroh::endpoint::SendStream,
    response: &[u8],
) -> Result<()> {
    send.write_all(response).await?;
    send.finish()?;
    send.stopped().await?;
    let _ = timeout(CLOSE_GRACE, connection.closed()).await;
    Ok(())
}

/// Takes a connection from handshake to close. Each stage holds only its own
/// slot, and unknown peers never reach the enrolled pool.
async fn admit(
    incoming: iroh::endpoint::Incoming,
    handshake: OwnedSemaphorePermit,
    admission: Arc<Admission>,
    store: Arc<Store>,
    services: Option<Arc<super::services::Services>>,
) {
    // Handshake failures and stalls come from unauthenticated peers; they are
    // not worth logging.
    let started = Instant::now();
    let Ok(Ok(connection)) = timeout(HANDSHAKE_TIMEOUT, incoming).await else {
        return;
    };
    drop(handshake);
    let peer = connection.remote_id().to_string();
    let handshake = started.elapsed();
    if !store.authorized(&peer) {
        reject(&connection, &admission).await;
        report_slow(&peer, "unenrolled", started, handshake, "done");
        return;
    }
    let Some(_device) = admission.devices.try_acquire(&peer) else {
        connection.close(
            CLOSE_BUSY.into(),
            b"too many concurrent connections from this device; retry shortly",
        );
        return;
    };
    let Ok(_slot) = admission.enrolled.clone().try_acquire_owned() else {
        connection.close(CLOSE_BUSY.into(), b"controller is busy; retry shortly");
        return;
    };
    let stage = std::sync::Mutex::new("accepting a stream");
    match timeout(
        ENROLLED_TIMEOUT,
        enrolled(&connection, &peer, store, services, &stage),
    )
    .await
    {
        Ok(Ok(())) => connection.close(CLOSE_DONE.into(), b"response sent"),
        Ok(Err(error)) => {
            connection.close(CLOSE_DONE.into(), b"connection failed");
            eprintln!("control connection failed: {error:#}");
        }
        Err(_) => {
            connection.close(CLOSE_TIMEOUT.into(), b"request timed out");
            let stage = *stage.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!(
                "control connection from {} timed out after {}s while {stage} (handshake {} ms)",
                short(&peer),
                ENROLLED_TIMEOUT.as_secs(),
                handshake.as_millis()
            );
            return;
        }
    }
    let stage = *stage.lock().unwrap_or_else(|e| e.into_inner());
    report_slow(&peer, "enrolled", started, handshake, stage);
}

/// Connections slower than this are logged with their stage timings.
const SLOW_CONNECTION: Duration = Duration::from_secs(2);

fn short(peer: &str) -> &str {
    &peer[..peer.len().min(10)]
}

fn report_slow(peer: &str, kind: &str, started: Instant, handshake: Duration, stage: &str) {
    let total = started.elapsed();
    if total >= SLOW_CONNECTION {
        eprintln!(
            "slow {kind} control connection from {}: {} ms total, {} ms handshake, last stage {stage}",
            short(peer),
            total.as_millis(),
            handshake.as_millis()
        );
    }
}

/// Tell an unknown or revoked peer why it was refused, within a small pool and
/// a short deadline, then close from this side. When the pool is full, the
/// close reason alone carries the message.
async fn reject(connection: &iroh::endpoint::Connection, admission: &Admission) {
    if let Ok(_slot) = admission.rejections.clone().try_acquire_owned() {
        let _ = timeout(REJECTION_TIMEOUT, async {
            let (send, mut receive) = connection.accept_bi().await?;
            // Read the bounded request so the client's write is not stopped
            // before it can read the reply.
            receive.read_to_end(MAX_REQUEST).await?;
            respond(
                connection,
                send,
                &encode(Err(failure("forbidden", NOT_ENROLLED)))?,
            )
            .await
        })
        .await;
    }
    connection.close(CLOSE_NOT_ENROLLED.into(), NOT_ENROLLED.as_bytes());
}

async fn enrolled(
    connection: &iroh::endpoint::Connection,
    peer: &str,
    store: Arc<Store>,
    services: Option<Arc<super::services::Services>>,
    stage: &std::sync::Mutex<&'static str>,
) -> Result<()> {
    let set = |next: &'static str| *stage.lock().unwrap_or_else(|e| e.into_inner()) = next;
    let (send, mut receive) = connection.accept_bi().await?;
    set("reading the request");
    let bytes = receive.read_to_end(MAX_REQUEST).await?;
    set("handling the request");
    let result = match serde_json::from_slice::<WireRequest>(&bytes) {
        Err(_) => Err(failure(
            "invalid",
            "malformed or unsupported control request",
        )),
        Ok(request) => {
            let binding = store.read()?.binding;
            if request.schema != 1 || request.environment != binding.environment {
                Err(failure(
                    "forbidden",
                    "controller environment identity mismatch",
                ))
            } else if let Some(services) = services {
                services.handle(&store, peer, request.request).await
            } else {
                store.handle(peer, request.request)
            }
        }
    };
    set("sending the reply");
    respond(connection, send, &encode(result)?).await?;
    set("done");
    Ok(())
}

pub async fn serve(
    endpoint: Endpoint,
    store: Arc<Store>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_with_services(endpoint, store, None, shutdown).await
}

pub async fn serve_with_services(
    endpoint: Endpoint,
    store: Arc<Store>,
    services: Option<Arc<super::services::Services>>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let admission = Arc::new(Admission::new());
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed { eprintln!("control connection task failed: {error}"); }
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break; };
                let Ok(handshake) = admission.handshakes.clone().try_acquire_owned() else { incoming.refuse(); continue; };
                tasks.spawn(admit(incoming, handshake, admission.clone(), store.clone(), services.clone()));
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    endpoint.close().await;
    Ok(())
}
