use std::{path::PathBuf, sync::Arc};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use hearth::control::host::{Host, HostConfig, HostRequest, SHUTDOWN_GRACE, rpc, serve_until};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(
    name = "hearth-host",
    about = "Privileged Proxmox helper for explicit lab or bounded physical trials"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
                "hearth-host serving environment {} on {}",
                host.environment(),
                socket.display()
            );
            serve_until(host, &socket, stop_signals()?, SHUTDOWN_GRACE).await
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

/// Forward every SIGTERM and SIGINT. The first lets the in-flight step finish;
/// a second one stops the helper immediately.
fn stop_signals() -> Result<tokio::sync::mpsc::Receiver<()>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(()) = terminate.recv() => {}
                Some(()) = interrupt.recv() => {}
                else => break,
            }
            if sender.send(()).await.is_err() {
                break;
            }
        }
    });
    Ok(receiver)
}
