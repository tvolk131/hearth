use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use pve_dev::control::maintenance::{self, Pins};

#[derive(Parser)]
#[command(
    about = "Offline paired controller/helper backup and QUARANTINED restoration; never activates workers"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Expected {
    #[arg(long)]
    expected_environment: String,
    #[arg(long)]
    expected_server: String,
    #[arg(long)]
    expected_pool_guid: String,
}

impl Expected {
    fn pins(&self) -> Pins {
        Pins {
            environment: self.expected_environment.clone(),
            server_id: self.expected_server.clone(),
            pool_guid: self.expected_pool_guid.clone(),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    PrepareAdoption {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        expected_bundle: String,
        #[arg(long)]
        managed_config: PathBuf,
        #[arg(long)]
        expected_config_sha256: String,
        #[command(flatten)]
        expected: Expected,
    },
    Backup {
        #[arg(long)]
        controller_dir: PathBuf,
        #[arg(long)]
        helper_dir: PathBuf,
        #[arg(long)]
        simulated_world: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
        /// Assert services stopped AND all external/orphan Proxmox tasks quiesced.
        #[arg(long, required = true)]
        external_effects_quiesced: bool,
        #[command(flatten)]
        expected: Expected,
    },
    Verify {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        expected_bundle: String,
        #[command(flatten)]
        expected: Expected,
    },
    Restore {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        expected_bundle: String,
        #[command(flatten)]
        expected: Expected,
    },
}

fn run() -> Result<()> {
    let receipt = match Cli::parse().command {
        Command::PrepareAdoption {
            bundle,
            output,
            expected_bundle,
            managed_config,
            expected_config_sha256,
            expected,
        } => maintenance::prepare_adoption(
            &bundle,
            &output,
            &expected.pins(),
            &expected_bundle,
            &managed_config,
            &expected_config_sha256,
        )?,
        Command::Backup {
            controller_dir,
            helper_dir,
            simulated_world,
            output,
            external_effects_quiesced,
            expected,
        } => maintenance::backup(
            &controller_dir,
            &helper_dir,
            simulated_world.as_deref(),
            &output,
            &expected.pins(),
            external_effects_quiesced,
        )?,
        Command::Verify {
            bundle,
            expected_bundle,
            expected,
        } => maintenance::verify(&bundle, &expected.pins(), &expected_bundle)?,
        Command::Restore {
            bundle,
            output,
            expected_bundle,
            expected,
        } => maintenance::restore(&bundle, &output, &expected.pins(), &expected_bundle)?,
    };
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
