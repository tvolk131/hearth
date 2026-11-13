use std::{fs, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use pve_dev::control::{
    host::{self, HostRequest},
    protocol::OperationStatus,
    store::{Binding, Store},
    transport::{self, Descriptor, Network, ServerConfig},
};

#[derive(Parser)]
#[command(
    about = "Experimental durable multi-user controller with explicit bounded trial backends"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        admin: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        host_socket: PathBuf,
        #[arg(long, default_value = "lab")]
        backend: String,
        #[arg(long)]
        physical_trial_config: Option<PathBuf>,
        #[arg(long, conflicts_with = "physical_trial_config")]
        managed_config: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "loopback")]
        network: Network,
        #[arg(long, value_enum)]
        migration_network: Option<Network>,
        #[arg(long, default_value = "0")]
        port: u16,
        #[arg(long, default_value = "2048")]
        disk_capacity_mib: u64,
        #[arg(long, default_value = "1024")]
        min_memory_capacity_mib: u64,
        #[arg(long = "guest-image-sha256")]
        guest_images: Vec<String>,
    },
    Serve {
        #[arg(long)]
        data_dir: PathBuf,
    },
    InspectOffline {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        expected_server: String,
        #[arg(long)]
        expected_environment: String,
    },
}

async fn worker(store: Arc<Store>, config: ServerConfig, binding: host::HostBinding) -> Result<()> {
    loop {
        for record in store.pending()? {
            let response = host::rpc(
                &config.host_socket,
                &HostRequest::SubmitBound {
                    binding: Box::new(binding.clone()),
                    job: record.job,
                },
            )
            .await;
            match response {
                Ok(response) => {
                    ensure!(
                        response
                            .binding
                            .as_ref()
                            .is_some_and(|binding| config.binding.attested_by(binding)),
                        "host backend/scope identity changed; stopping controller worker"
                    );
                    store.progress_detailed(
                        &record.public.operation_id,
                        response.status,
                        &response.step,
                        response.error,
                        response.progress,
                    )?;
                }
                Err(error) => {
                    eprintln!(
                        "host helper unavailable for {}: {error:#}",
                        record.public.operation_id
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn attest_managed_imports(socket: &std::path::Path, binding: &Binding) -> Result<()> {
    if let Some(m) = &binding.managed {
        let health = host::rpc(socket, &HostRequest::Health).await?;
        ensure!(
            health.status == OperationStatus::Verified
                && health
                    .binding
                    .as_ref()
                    .is_some_and(|b| binding.attested_by(b)),
            "managed helper binding differs from controller root config"
        );
        let response = host::rpc(
            socket,
            &HostRequest::ImportedVms {
                binding: Box::new(health.binding.expect("attested helper")),
            },
        )
        .await?;
        ensure!(
            response.status == OperationStatus::Verified
                && response
                    .binding
                    .as_ref()
                    .is_some_and(|b| binding.attested_by(b))
                && response.inspection == Some(serde_json::json!(m.imported_vms)),
            "helper imported metadata does not match root-pinned controller records"
        );
    }
    Ok(())
}

async fn shutdown() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
}

async fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Init {
            data_dir,
            admin,
            environment,
            host_socket,
            backend,
            physical_trial_config,
            managed_config,
            network,
            migration_network,
            port,
            disk_capacity_mib,
            min_memory_capacity_mib,
            guest_images,
        } => {
            let physical_trial = match physical_trial_config {
                Some(path) => {
                    let trial = pve_dev::control::physical_trial::TrialConfig::read(&path)?;
                    ensure!(
                        backend == "physical_trial"
                            && environment == trial.environment
                            && network == Network::Loopback,
                        "physical trial requires its matching backend/environment and loopback transport"
                    );
                    ensure!(
                        guest_images
                            .iter()
                            .all(|digest| trial.images.iter().any(|image| &image.sha256 == digest)),
                        "controller image allowlist escapes root-owned physical trial config"
                    );
                    Some(trial.binding())
                }
                None => {
                    ensure!(
                        backend != "physical_trial",
                        "physical trial config is required"
                    );
                    None
                }
            };
            let managed = match managed_config {
                Some(path) => {
                    let c = pve_dev::control::managed::ManagedConfig::read(&path)?;
                    ensure!(
                        backend == "managed" && environment == c.environment,
                        "managed config backend/environment mismatch"
                    );
                    ensure!(
                        guest_images.is_empty()
                            || guest_images
                                == c.binding
                                    .images
                                    .iter()
                                    .map(|i| i.image_sha256.clone())
                                    .collect::<Vec<_>>(),
                        "controller image allowlist differs from managed root pin"
                    );
                    ensure!(
                        migration_network.is_none(),
                        "managed migration transport is disabled"
                    );
                    Some(c.binding)
                }
                None => {
                    ensure!(backend != "managed", "managed root config is required");
                    None
                }
            };
            let binding = Binding {
                environment,
                backend,
                disk_capacity_mib: managed
                    .as_ref()
                    .map_or(disk_capacity_mib, |m| m.scope.capacity_mib),
                min_memory_capacity_mib: managed
                    .as_ref()
                    .map_or(min_memory_capacity_mib, |m| m.scope.min_memory_capacity_mib),
                guest_images: managed.as_ref().map_or(guest_images, |m| {
                    m.images.iter().map(|i| i.image_sha256.clone()).collect()
                }),
                physical_trial,
                managed,
            };
            attest_managed_imports(&host_socket, &binding).await?;
            transport::initialize(
                &data_dir,
                &ServerConfig {
                    schema: 1,
                    binding,
                    host_socket,
                    network,
                    port,
                    migration_network,
                },
                &admin,
            )?;
            println!(
                "{}",
                serde_json::json!({"initialized":true,"data_dir":data_dir})
            );
        }
        Commands::Serve { data_dir } => {
            let (config, store, key) = transport::open(&data_dir)?;
            let health = host::rpc(&config.host_socket, &HostRequest::Health).await?;
            attest_managed_imports(&config.host_socket, &config.binding).await?;
            ensure!(
                health
                    .binding
                    .as_ref()
                    .is_some_and(|binding| config.binding.attested_by(binding))
                    && health.status == OperationStatus::Verified,
                "host helper does not attest this environment"
            );
            let endpoint =
                transport::endpoint(key.clone(), config.network, config.port, true).await?;
            if config.network == Network::Public {
                tokio::time::timeout(Duration::from_secs(45), endpoint.online()).await
                    .map_err(|_| anyhow::anyhow!("public controller could not reach an iroh relay; no ready descriptor was published"))?;
            }
            let descriptor = Descriptor {
                schema: 1,
                endpoint: endpoint.addr(),
                environment: config.binding.environment.clone(),
                network: config.network,
            };
            let temporary = data_dir.join("server.json.tmp");
            if temporary.exists() {
                fs::remove_file(&temporary)?;
            }
            transport::write_new(&temporary, &serde_json::to_vec_pretty(&descriptor)?)?;
            fs::rename(temporary, data_dir.join("server.json"))?;
            fs::File::open(&data_dir)?.sync_all()?;
            println!(
                "{}",
                serde_json::json!({"ready":true,"server_id":endpoint.id().to_string(),"descriptor":data_dir.join("server.json"),"backend":config.binding.backend})
            );
            let migration = if config.binding.feature("migration") {
                Some(
                    pve_dev::control::migration_transport::Service::bind(
                        store.clone(),
                        key,
                        config.host_socket.clone(),
                        config.migration_network.unwrap_or(Network::Loopback),
                        0,
                    )
                    .await?,
                )
            } else {
                None
            };
            let services = Arc::new(pve_dev::control::services::Services {
                host_socket: config.host_socket.clone(),
                migration: migration.clone(),
            });
            let transfer_service = async {
                if let Some(service) = migration {
                    service.serve(std::future::pending()).await
                } else {
                    std::future::pending::<Result<()>>().await
                }
            };
            tokio::select! {
                result = transport::serve_with_services(endpoint, store.clone(), Some(services), shutdown()) => { result?; },
                result = worker(store,config,health.binding.expect("verified helper attestation")) => {result?;anyhow::bail!("controller worker unexpectedly stopped");},
                result = transfer_service => {result?;anyhow::bail!("migration service unexpectedly stopped");},
            }
        }
        Commands::InspectOffline {
            data_dir,
            expected_server,
            expected_environment,
        } => {
            ensure!(
                host::valid_environment(&expected_environment),
                "invalid expected environment"
            );
            let expected_server = expected_server.parse::<iroh::EndpointId>()?.to_string();
            let (config, store, key) = transport::open(&data_dir)?;
            let server_id = key.public().to_string();
            ensure!(
                server_id == expected_server && config.binding.environment == expected_environment,
                "offline controller identity pin mismatch"
            );
            let catalog = store.read()?;
            let vms: Vec<_> = catalog.vms.values().map(|vm| serde_json::json!({
                "vm_id":vm.vm_id,"owner":vm.owner,"name":vm.name,"disk_mib":vm.disk_mib,
                "memory_mib":vm.memory_mib,"min_memory_mib":vm.min_memory_mib,"cores":vm.cores,"state":vm.state
            })).collect();
            let operations: Vec<_> = catalog
                .operations
                .values()
                .map(|record| &record.public)
                .collect();
            println!(
                "{}",
                serde_json::json!({"server_id":server_id,"binding":catalog.binding,
                "users":catalog.users.values().collect::<Vec<_>>(),"vms":vms,"imported_vms":catalog.imported_vms,"operations":operations})
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
