use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, EndpointAddr, SecretKey, endpoint::presets};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, task::JoinSet, time::timeout};

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
    let result = timeout(Duration::from_secs(25), async {
        let connection = endpoint
            .connect(profile.server.endpoint.clone(), ALPN)
            .await?;
        let (mut send, mut receive) = connection.open_bi().await?;
        send.write_all(&request).await?;
        send.finish()?;
        let bytes = receive.read_to_end(MAX_RESPONSE).await?;
        let response: Response = serde_json::from_slice(&bytes)?;
        connection.close(0u32.into(), b"response received");
        Ok::<_, anyhow::Error>(response)
    })
    .await
    .context("controller request timed out; retry mutations only with the same request_id");
    endpoint.close().await;
    result?
}

async fn connection(
    incoming: iroh::endpoint::Incoming,
    store: Arc<Store>,
    services: Option<Arc<super::services::Services>>,
) -> Result<()> {
    let connection = incoming.await?;
    let peer = connection.remote_id().to_string();
    let authorized = store.authorized(&peer);
    let (mut send, mut receive) = connection.accept_bi().await?;
    let bytes = receive.read_to_end(MAX_REQUEST).await?;
    let result = if !authorized {
        Err(failure(
            "forbidden",
            "device is not enrolled or has been revoked",
        ))
    } else {
        match serde_json::from_slice::<WireRequest>(&bytes) {
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
                } else {
                    if let Some(services) = services {
                        services.handle(&store, &peer, request.request).await
                    } else {
                        store.handle(&peer, request.request)
                    }
                }
            }
        }
    };
    let mut response = serde_json::to_vec(&Response::from_result(result))?;
    if response.len() > MAX_RESPONSE {
        response = serde_json::to_vec(&Response::from_result(Err(failure(
            "unavailable",
            "response exceeds protocol limit; administrator must archive history",
        ))))?;
    }
    send.write_all(&response).await?;
    send.finish()?;
    connection.closed().await;
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
    let permits = Arc::new(Semaphore::new(32));
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
                let Ok(permit) = permits.clone().try_acquire_owned() else { incoming.refuse(); continue; };
                let store = store.clone();
                let services=services.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Ok(Err(error)) = timeout(Duration::from_secs(30), connection(incoming, store, services)).await {
                        eprintln!("control connection failed: {error}");
                    }
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    endpoint.close().await;
    Ok(())
}
