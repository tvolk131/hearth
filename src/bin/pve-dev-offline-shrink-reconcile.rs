//! Explicit offline operator tool. Never activates workers or changes disks.
use clap::Parser;
use pve_dev::control::maintenance::{self, Pins};
use std::path::PathBuf;
#[derive(Parser)]
struct Args {
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
    #[arg(long)]
    expected_environment: String,
    #[arg(long)]
    expected_server: String,
    #[arg(long)]
    expected_pool_guid: String,
    #[arg(long)]
    vm_id: u16,
    #[arg(long)]
    expected_disk_mib: u64,
    #[arg(long)]
    target_disk_mib: u64,
}
fn main() -> anyhow::Result<()> {
    let a = Args::parse();
    let receipt = maintenance::prepare_offline_adopted_shrink(
        &a.bundle,
        &a.output,
        &Pins {
            environment: a.expected_environment,
            server_id: a.expected_server,
            pool_guid: a.expected_pool_guid,
        },
        &a.expected_bundle,
        &a.managed_config,
        &a.expected_config_sha256,
        a.vm_id,
        a.expected_disk_mib,
        a.target_disk_mib,
    )?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}
