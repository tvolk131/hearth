use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use pve_dev::control::{
    maintenance::{self, Pins},
    retention::DEFAULT_RETENTION_DAYS,
};

#[derive(Parser)]
#[command(
    about = "Offline paired controller/helper backup, QUARANTINED restoration and history retention; never activates workers"
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
    /// Export finished history older than the retention age to a new JSONL
    /// archive, then prune it from the stopped controller and helper.
    /// Non-terminal operations, anything they or retained state reference,
    /// and every request-ID record are always kept. If interrupted, run again.
    Prune {
        #[arg(long)]
        controller_dir: PathBuf,
        #[arg(long)]
        helper_dir: PathBuf,
        /// New archive file to create (absolute path; must not exist).
        #[arg(long, required_unless_present = "dry_run")]
        archive: Option<PathBuf>,
        /// Report what would be pruned without writing anything.
        #[arg(long, conflicts_with = "archive")]
        dry_run: bool,
        #[arg(long, default_value_t = DEFAULT_RETENTION_DAYS, value_parser = clap::value_parser!(u64).range(1..=36500))]
        older_than_days: u64,
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
    /// Produce new quarantined state with raised compute ceilings; never modifies live state or activates workers.
    ExpandCompute {
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
    /// Produce new quarantined state reconciling a verified offline shrink; never activates workers or changes disks.
    OfflineShrinkReconcile {
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
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        expected_disk_mib: u64,
        #[arg(long)]
        target_disk_mib: u64,
    },
}

fn run() -> Result<()> {
    let receipt = match Cli::parse().command {
        Command::Prune {
            controller_dir,
            helper_dir,
            archive,
            dry_run: _,
            older_than_days,
            expected,
        } => {
            let receipt = maintenance::prune(
                &controller_dir,
                &helper_dir,
                archive.as_deref(),
                older_than_days,
                &expected.pins(),
            )?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            return Ok(());
        }
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
        Command::ExpandCompute {
            bundle,
            output,
            expected_bundle,
            managed_config,
            expected_config_sha256,
            expected,
        } => maintenance::prepare_compute_expansion(
            &bundle,
            &output,
            &expected.pins(),
            &expected_bundle,
            &managed_config,
            &expected_config_sha256,
        )?,
        Command::OfflineShrinkReconcile {
            bundle,
            output,
            expected_bundle,
            managed_config,
            expected_config_sha256,
            expected,
            vm_id,
            expected_disk_mib,
            target_disk_mib,
        } => maintenance::prepare_offline_adopted_shrink(
            &bundle,
            &output,
            &expected.pins(),
            &expected_bundle,
            &managed_config,
            &expected_config_sha256,
            vm_id,
            expected_disk_mib,
            target_disk_mib,
        )?,
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
