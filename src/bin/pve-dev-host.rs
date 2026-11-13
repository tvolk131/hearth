use std::{path::PathBuf, sync::Arc};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use pve_dev::control::host::{Host, HostConfig, HostRequest, repair_managed_schema, rpc, serve};

#[derive(Parser)]
#[command(
    name = "pve-dev-host",
    about = "Privileged Proxmox helper for explicit lab or bounded physical trials"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Offline marker-only repair of the narrowly identified managed6 submission bug.
    RepairManagedSchema {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        managed_config: PathBuf,
        #[arg(long)]
        expected_journal_sha256: String,
        #[arg(long)]
        backup_directory: PathBuf,
        /// Validate native redb crash recovery in memory before allowing it on the pinned original.
        #[arg(long)]
        allow_native_recovery: bool,
    },
    Serve {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, conflicts_with_all = ["simulate", "environment", "world", "physical_trial_config", "managed_config"], required_unless_present_any = ["simulate", "physical_trial_config", "managed_config"])]
        lab_config: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["simulate", "environment", "world", "lab_config"])]
        physical_trial_config: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["simulate", "environment", "world", "lab_config", "physical_trial_config"])]
        managed_config: Option<PathBuf>,
        #[arg(long, requires = "environment")]
        simulate: bool,
        #[arg(long, requires = "simulate")]
        environment: Option<String>,
        #[arg(long, requires = "simulate")]
        world: Option<PathBuf>,
    },
    Health {
        #[arg(long)]
        socket: PathBuf,
    },
    Provenance {
        #[arg(long)]
        socket: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::RepairManagedSchema {
            state_dir,
            managed_config,
            expected_journal_sha256,
            backup_directory,
            allow_native_recovery,
        } => {
            let receipt = repair_managed_schema(
                &state_dir,
                &managed_config,
                &expected_journal_sha256,
                &backup_directory,
                allow_native_recovery,
            )?;
            println!("{}", serde_json::to_string(&receipt)?);
            Ok(())
        }
        Command::Serve {
            state_dir,
            socket,
            lab_config,
            physical_trial_config,
            managed_config,
            simulate,
            environment,
            world,
        } => {
            let config = match (lab_config, physical_trial_config, managed_config) {
                (Some(file), None, None) => HostConfig::lab(state_dir, &file)?,
                (None, Some(file), None) => HostConfig::physical_trial(state_dir, &file)?,
                (None, None, Some(file)) => HostConfig::managed(state_dir, &file)?,
                (None, None, None) => {
                    ensure!(simulate, "either --lab-config or --simulate is required");
                    let environment = environment.unwrap_or_default();
                    let world = world.unwrap_or_else(|| state_dir.join("simulated-world.json"));
                    HostConfig::simulated(state_dir, &environment, world)
                }
                _ => anyhow::bail!("choose exactly one helper backend"),
            };
            let host = Arc::new(Host::open(config).await?);
            eprintln!(
                "pve-dev-host serving environment {} on {}",
                host.environment(),
                socket.display()
            );
            serve(host, &socket).await
        }
        Command::Health { socket } => {
            let response = rpc(&socket, &HostRequest::Health).await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
        Command::Provenance { socket } => {
            let response = rpc(&socket, &HostRequest::OwnedResources).await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
    }
}
