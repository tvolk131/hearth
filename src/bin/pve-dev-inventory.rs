//! Observation-only discovery CLI. Offline commands preserve their original
//! behavior. Explicit `observe` runs only the fixed collector over pinned SSH;
//! no command writes to Proxmox or a controller database.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use pve_dev::discovery::observation::{ObservationProfile, collect};
use pve_dev::discovery::{
    DEFAULT_ZFS_ROOT, HostPin, Mapping, ObservationStore, ValidatedSnapshot, collector_sha256,
    collector_source, propose_mapping, ssh_arguments, ssh_command_line, validate_snapshot,
};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "pve-dev-inventory",
    about = "Read-only Proxmox discovery: print the collector, validate snapshots, and import explicitly confirmed observations"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Args)]
struct Pin {
    /// Snapshot file produced by the collector.
    #[arg(long)]
    snapshot: PathBuf,
    /// Expected Proxmox node name (explicit host pin).
    #[arg(long)]
    node: String,
    /// Expected Ed25519 host key fingerprint, e.g. SHA256:... (explicit host pin).
    #[arg(long)]
    host_key: String,
}

#[derive(Subcommand)]
enum Action {
    /// Inspect a local read-only host profile (does not connect).
    ObserveProfileInfo {
        #[arg(long)]
        profile: PathBuf,
    },
    /// Run the fixed read-only collector over pinned SSH and print a GUI read model.
    Observe {
        #[arg(long)]
        profile: PathBuf,
        /// Public host binding saved when the local profile was explicitly trusted.
        #[arg(long)]
        expected_binding: String,
        #[arg(long, default_value_t = 60)]
        timeout_seconds: u64,
    },
    /// Print the read-only collector Python source to stdout.
    Collector {
        #[arg(long, default_value = DEFAULT_ZFS_ROOT)]
        zfs_root: String,
    },
    /// Print the exact SSH command an operator can run (not executed here).
    SshCommand {
        #[arg(long)]
        destination: String,
        #[arg(long)]
        known_hosts: PathBuf,
        #[arg(long)]
        identity: Option<PathBuf>,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        collector: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = DEFAULT_ZFS_ROOT)]
        zfs_root: String,
    },
    /// Validate a snapshot offline and print its digest and warnings.
    Validate(Pin),
    /// Print an unconfirmed mapping proposal (all owners null).
    Propose {
        #[command(flatten)]
        pin: Pin,
        /// Write to a new file instead of stdout; never overwrites.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Store a validated snapshot in the observation database (no ownership).
    Record {
        #[arg(long)]
        db: PathBuf,
        #[command(flatten)]
        pin: Pin,
    },
    /// Import observed state with an explicitly confirmed mapping.
    Import {
        #[arg(long)]
        db: PathBuf,
        #[command(flatten)]
        pin: Pin,
        #[arg(long)]
        mapping: PathBuf,
        /// Must equal the snapshot digest printed by `validate`.
        #[arg(long)]
        confirm_snapshot: String,
    },
    /// Verify database integrity and print a summary.
    Verify {
        #[arg(long)]
        db: PathBuf,
    },
    /// Print one stored import record.
    Show {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        host_id: String,
        #[arg(long)]
        snapshot_digest: String,
    },
}

fn load(pin: &Pin) -> Result<ValidatedSnapshot> {
    let bytes =
        fs::read(&pin.snapshot).with_context(|| format!("reading {}", pin.snapshot.display()))?;
    validate_snapshot(
        &bytes,
        &HostPin {
            node: pin.node.clone(),
            host_key_sha256: pin.host_key.clone(),
        },
    )
}

fn print(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("refusing to overwrite or create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::File::open(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Action::ObserveProfileInfo { profile } => {
            let profile = ObservationProfile::load(&profile)?;
            profile.checked_known_hosts()?;
            print(&profile.public_identity())?;
        }
        Action::Observe {
            profile,
            expected_binding,
            timeout_seconds,
        } => {
            let profile = ObservationProfile::load(&profile)?;
            print(&collect(&profile, &expected_binding, timeout_seconds).await?)?;
        }
        Action::Collector { zfs_root } => print!("{}", collector_source(&zfs_root)?),
        Action::SshCommand {
            destination,
            known_hosts,
            identity,
            port,
            collector,
            output,
            zfs_root,
        } => {
            let arguments = ssh_arguments(&destination, &known_hosts, identity.as_deref(), port)?;
            print(&json!({
                "executed": false,
                "collector_sha256": collector_sha256(&zfs_root)?,
                "prepare": format!("pve-dev-inventory collector --zfs-root {zfs_root} > {}", collector.display()),
                "argv": arguments,
                "command": ssh_command_line(&arguments, &collector, &output),
                "note": "Review the collector source first. Run once, by an operator, against the pinned host only."
            }))?;
        }
        Action::Validate(pin) => print(&load(&pin)?.report())?,
        Action::Propose { pin, output } => {
            let proposal = propose_mapping(&load(&pin)?);
            let text = serde_json::to_string_pretty(&proposal)? + "\n";
            match output {
                Some(path) => write_new(&path, text.as_bytes())?,
                None => print!("{text}"),
            }
        }
        Action::Record { db, pin } => {
            let validated = load(&pin)?;
            let outcome = ObservationStore::open(&db)?.record_snapshot(&validated)?;
            print(&json!({
                "outcome": outcome,
                "host_id": validated.host_id(),
                "snapshot_digest": validated.digest,
            }))?;
        }
        Action::Import {
            db,
            pin,
            mapping,
            confirm_snapshot,
        } => {
            let validated = load(&pin)?;
            let mapping: Mapping = serde_json::from_slice(
                &fs::read(&mapping).with_context(|| format!("reading {}", mapping.display()))?,
            )
            .context("mapping does not match the mapping schema")?;
            let result =
                ObservationStore::open(&db)?.import(&validated, &mapping, &confirm_snapshot)?;
            print(&result)?;
        }
        Action::Verify { db } => {
            ensure!(db.exists(), "{} does not exist", db.display());
            print(&ObservationStore::open(&db)?.verify()?)?;
        }
        Action::Show {
            db,
            host_id,
            snapshot_digest,
        } => {
            ensure!(db.exists(), "{} does not exist", db.display());
            let record = ObservationStore::open(&db)?
                .import_record(&host_id, &snapshot_digest)?
                .context("no such import")?;
            print(&record)?;
        }
    }
    Ok(())
}
