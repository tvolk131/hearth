//! Offline paired journal export and quarantined restoration. This is not a
//! reconciliation or activation API: restored state must never start workers.
use crate::control::JobKind;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path},
};

use anyhow::{Context, Result, ensure};
use iroh::SecretKey;
use redb::{Database, Durability, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    host::{self, Journal, MachineState, World},
    protocol::{AccountState, OperationStatus, VmState},
    store::Catalog,
    transport::ServerConfig,
};

const CATALOG: TableDefinition<u8, &[u8]> = TableDefinition::new("controller_catalog_v1");
const JOURNAL: TableDefinition<u8, &[u8]> = TableDefinition::new("host_journal_v1");
const LIMIT: u64 = 64 * 1024 * 1024;
pub const QUARANTINE: &str = "restore-review-required";

unsafe extern "C" {
    fn geteuid() -> u32;
}

// Only the offline operator and root may be able to rename an output ancestor.
// A sticky root-owned temporary directory protects its individually owned
// children; ordinary group/world-writable ancestors do not.
fn trusted_output_parent(path: &Path) -> Result<()> {
    directory(path)?;
    // SAFETY: POSIX geteuid has no arguments and no memory preconditions.
    let uid = unsafe { geteuid() };
    ensure!(
        fs::symlink_metadata(path)?.uid() == uid,
        "output parent must belong to the offline operator"
    );
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        ensure!(
            meta.uid() == 0 || meta.uid() == uid,
            "untrusted owner in output ancestry"
        );
        ensure!(
            meta.mode() & 0o022 == 0 || meta.mode() & 0o1000 != 0,
            "non-sticky writable output ancestor refused"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    pub environment: String,
    pub server_id: String,
    pub pool_guid: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    schema: u32,
    created_at: u64,
    config: ServerConfig,
    identity: Vec<u8>,
    catalog: Catalog,
    journal: Journal,
    simulated_world: Option<World>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    schema: u32,
    payload_sha256: String,
    payload: Payload,
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub schema: u32,
    pub payload_sha256: String,
    pub environment: String,
    pub server_id: String,
    pub pool_guid: String,
    pub controller_revision: u64,
    pub helper_sequence: u64,
    pub operation_count: usize,
    pub quarantined: bool,
    pub activates_workers: bool,
}

/// A dangling symlink, directory, or unreadable marker also means quarantine.
pub fn ensure_not_quarantined(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory.join(QUARANTINE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => anyhow::bail!(
            "restored state is quarantined pending independent infrastructure reconciliation"
        ),
    }
}

fn canonical(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "use an absolute canonical path");
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "dot and parent path components are refused"
    );
    ensure!(
        fs::canonicalize(path)? == path,
        "symlink or noncanonical path refused"
    );
    Ok(())
}

fn directory(path: &Path) -> Result<()> {
    canonical(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.mode() & 0o077 == 0,
        "state/bundle directories must be private (0700)"
    );
    Ok(())
}

fn new_directory(path: &Path) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    trusted_output_parent(parent)?;
    ensure!(
        path.file_name().is_some() && path.is_absolute(),
        "invalid output path"
    );
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "unsafe output path"
    );
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .context("destination must not already exist")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn regular(path: &Path) -> Result<()> {
    canonical(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o077 == 0,
        "state files must be private regular files with one link"
    );
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>> {
    regular(path)?;
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= LIMIT,
        "file exceeds maintenance size limit"
    );
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "file exceeds maintenance size limit"
    );
    Ok(bytes)
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}

fn sha(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn read_record<T: serde::de::DeserializeOwned>(
    db: &Database,
    table: TableDefinition<u8, &[u8]>,
) -> Result<T> {
    let tx = db.begin_read()?;
    let table = tx.open_table(table)?;
    let row = table.get(0)?.context("missing durable journal record")?;
    ensure!(
        row.value().len() as u64 <= LIMIT,
        "journal exceeds maintenance size limit"
    );
    Ok(serde_json::from_slice(row.value())?)
}

fn write_record(
    path: &Path,
    table: TableDefinition<u8, &[u8]>,
    value: &impl Serialize,
) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let db = Database::builder().create_file(file)?;
    let mut tx = db.begin_write()?;
    tx.set_durability(Durability::Immediate)?;
    tx.open_table(table)?
        .insert(0, serde_json::to_vec(value)?.as_slice())?;
    tx.commit()?;
    drop(db);
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn validate(payload: &Payload, pins: &Pins) -> Result<()> {
    ensure!(
        payload.schema == 1
            && payload.config.schema == 1
            && matches!(payload.catalog.schema, 1..=7)
            && matches!(payload.journal.schema, 1..=7),
        "unsupported maintenance/state schema"
    );
    ensure!(
        host::valid_environment(&pins.environment),
        "invalid expected environment"
    );
    let identity: [u8; 32] = payload
        .identity
        .as_slice()
        .try_into()
        .context("invalid server identity length")?;
    ensure!(
        SecretKey::from_bytes(&identity).public().to_string() == pins.server_id,
        "server identity mismatch"
    );
    ensure!(
        payload.config.binding == payload.catalog.binding,
        "controller configuration/catalog binding mismatch"
    );
    ensure!(
        payload.config.binding.environment == pins.environment
            && payload.journal.binding.environment == pins.environment
            && payload.config.binding.backend == payload.journal.binding.backend
            && payload.journal.binding.pool_guid == pins.pool_guid,
        "controller/helper/expected environment, backend or pool mismatch"
    );
    payload.config.binding.validate()?;
    payload.journal.binding.validate()?;
    host::validate_scope(&payload.journal)?;
    super::store::validate_scope(&payload.catalog)?;
    ensure!(
        payload.config.binding.attested_by(&payload.journal.binding),
        "controller/helper physical scope binding mismatch"
    );
    ensure!(
        payload.config.host_socket.is_absolute(),
        "invalid helper socket path"
    );
    match (
        &payload.simulated_world,
        payload.config.binding.backend.as_str(),
    ) {
        (Some(world), "simulated") => ensure!(
            world.lab_id == pins.environment && world.pool_guid == pins.pool_guid,
            "simulated world identity mismatch"
        ),
        (None, "lab" | "physical_trial" | "managed") => {}
        _ => anyhow::bail!("backup must include exactly the simulated backend's external world"),
    }
    for (id, operation) in &payload.catalog.operations {
        ensure!(
            id == &operation.public.operation_id && id == &operation.job.operation_id,
            "controller operation key mismatch"
        );
        if let Some(job) = payload.journal.jobs.get(id) {
            ensure!(
                job.job.operation_id == *id
                    && job.fingerprint == host::fingerprint(&job.job)?
                    && host::fingerprint(&job.job)? == host::fingerprint(&operation.job)?,
                "mixed controller/helper frozen operation payloads"
            );
            if matches!(
                operation.public.status,
                OperationStatus::Verified | OperationStatus::Blocked | OperationStatus::Cancelled
            ) {
                ensure!(
                    operation.public.status == job.status,
                    "controller terminal proof is absent from helper journal (stale pair)"
                );
            }
        } else {
            ensure!(
                operation.public.status == OperationStatus::Queued,
                "controller operation has progress without helper evidence (stale pair)"
            );
        }
    }
    for id in payload.journal.jobs.keys() {
        ensure!(
            payload.catalog.operations.contains_key(id),
            "helper operation missing from controller (stale pair)"
        );
    }
    for (id, user) in &payload.catalog.users {
        if user.state != AccountState::Active {
            ensure!(
                !user.ready
                    && payload
                        .catalog
                        .devices
                        .values()
                        .filter(|device| device.user_id == *id)
                        .all(|device| device.revoked),
                "inactive user must remain frozen with all devices revoked"
            );
        }
        if user.state == AccountState::Deleted {
            let owner = payload
                .journal
                .owners
                .get(id)
                .context("deleted controller user has no helper tombstone")?;
            ensure!(
                owner.state == AccountState::Deleted
                    && !owner.ready
                    && owner.policy_revision == user.policy_revision,
                "controller user release lacks matching helper account deletion proof"
            );
            let operation = owner
                .delete_operation
                .as_ref()
                .and_then(|id| payload.catalog.operations.get(id))
                .context("deleted owner lacks its retained controller deletion operation")?;
            ensure!(
                operation.public.kind == JobKind::DeleteUser
                    && operation.public.owner == *id
                    && operation.public.status == OperationStatus::Verified
                    && operation.job.owner.role == super::protocol::Role::Member
                    && user.role == super::protocol::Role::Member
                    && operation.job.owner.policy_revision == user.policy_revision
                    && operation.job.owner.policy() == user.policy(),
                "deleted owner tombstone does not match a verified delete_user operation"
            );
            ensure!(
                payload
                    .catalog
                    .devices
                    .values()
                    .filter(|device| device.user_id == *id)
                    .all(|device| device.revoked),
                "deleted user still has an enrolled device"
            );
        }
    }
    for (id, machine) in &payload.journal.machines {
        let vm = payload
            .catalog
            .vms
            .get(id)
            .context("helper machine missing from controller")?;
        ensure!(
            machine.vm_id == *id && vm.vm_id == *id && machine.owner == vm.owner,
            "controller/helper machine identity mismatch"
        );
        let mut observed = serde_json::to_value(vm)?;
        observed.as_object_mut().expect("VM object").remove("vm_id");
        observed.as_object_mut().unwrap().remove("owner");
        observed.as_object_mut().unwrap().remove("state");
        let expected = serde_json::to_value(&machine.profile)?;
        let resize_profile = payload
            .catalog
            .operations
            .values()
            .filter(|r| {
                !r.public.status.is_terminal()
                    && r.public.vm_id == Some(*id)
                    && r.public.owner == vm.owner
            })
            .any(|r| {
                let target = if let Some(spec) = &r.job.disk_shrink {
                    Some(spec.target_disk_mib)
                } else if let Some(super::vm_change::JobExtension::Grow(spec)) =
                    r.job.extension.as_deref()
                {
                    Some(spec.target_disk_mib)
                } else {
                    None
                };
                let mut target_profile = observed.clone();
                if let Some(size) = target {
                    target_profile["disk_mib"] = serde_json::json!(size);
                }
                target.is_some() && target_profile == expected
            });
        let incoming_profile=payload.catalog.migrations.values().filter(|r|r.role==super::migration::Role::Destination && r.plan.target_vm_id==*id && r.plan.target_owner==vm.owner && (!r.phase.terminal() || r.phase==super::migration::Phase::Aborted && vm.state==VmState::Deleted && machine.state==MachineState::Deleted)).any(|r| {
            let mut staging=observed.clone();
            staging.as_object_mut().unwrap().remove("guest");
            let approved=r.plan.vm(super::migration::Role::Destination);
            approved.guest==vm.guest && staging==expected && payload.journal.jobs.values().any(|job|matches!(job.job.extension.as_deref(),Some(super::vm_change::JobExtension::IncomingMigration(plan)) if plan==&r.plan))
        });
        ensure!(
            observed == expected || resize_profile || incoming_profile,
            "controller/helper VM profile mismatch without a matching retained resize or incoming transfer"
        );
        if vm.state == VmState::Deleted {
            ensure!(
                machine.state == MachineState::Deleted,
                "controller tombstone missing helper deletion proof"
            );
        }
    }
    Ok(())
}

fn receipt(bundle: &Bundle, pins: &Pins, quarantined: bool) -> Receipt {
    Receipt {
        schema: 1,
        payload_sha256: bundle.payload_sha256.clone(),
        environment: pins.environment.clone(),
        server_id: pins.server_id.clone(),
        pool_guid: pins.pool_guid.clone(),
        controller_revision: bundle.payload.catalog.revision,
        helper_sequence: bundle.payload.journal.sequence,
        operation_count: bundle.payload.catalog.operations.len(),
        quarantined,
        activates_workers: false,
    }
}

/// Locks BOTH databases for the entire export. This refuses running services.
/// The operator must additionally quiesce external tasks; a DB lock cannot prove
/// that a Proxmox task or orphan command has stopped.
pub fn backup(
    controller: &Path,
    helper: &Path,
    simulated_world: Option<&Path>,
    output: &Path,
    pins: &Pins,
    external_effects_quiesced: bool,
) -> Result<Receipt> {
    ensure!(
        external_effects_quiesced,
        "explicit external-effects quiescence assertion required"
    );
    directory(controller)?;
    directory(helper)?;
    ensure_not_quarantined(controller)?;
    ensure_not_quarantined(helper)?;
    let controller_db = controller.join("state.redb");
    let helper_db = helper.join("host-journal.redb");
    regular(&controller_db)?;
    regular(&helper_db)?;
    let controller_lock = Database::open(&controller_db).context("controller must be stopped")?;
    let helper_lock = Database::open(&helper_db).context("helper must be stopped")?;
    ensure!(
        read(&controller.join("initialized"))? == b"controller-v1\n",
        "controller initialization incomplete"
    );
    ensure!(
        read(&helper.join("initialized"))? == format!("{}\n", pins.environment).as_bytes(),
        "helper initialization incomplete"
    );
    let payload = Payload {
        schema: 1,
        created_at: super::store::now(),
        config: serde_json::from_slice(&read(&controller.join("config.json"))?)?,
        identity: read(&controller.join("identity.key"))?,
        catalog: read_record(&controller_lock, CATALOG)?,
        journal: read_record(&helper_lock, JOURNAL)?,
        simulated_world: simulated_world
            .map(|path| -> Result<World> { Ok(serde_json::from_slice(&read(path)?)?) })
            .transpose()?,
    };
    validate(&payload, pins)?;
    let bundle = Bundle {
        schema: 1,
        payload_sha256: sha(&payload)?,
        payload,
    };
    let bytes = serde_json::to_vec(&bundle)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "bundle exceeds maintenance size limit"
    );
    new_directory(output)?;
    write(&output.join("bundle.json"), &bytes)?;
    Ok(receipt(&bundle, pins, false))
}

fn load(input: &Path, pins: &Pins, expected_bundle: &str) -> Result<Bundle> {
    directory(input)?;
    let bundle: Bundle = serde_json::from_slice(&read(&input.join("bundle.json"))?)?;
    ensure!(
        bundle.schema == 1
            && bundle.payload_sha256 == expected_bundle
            && sha(&bundle.payload)? == expected_bundle,
        "bundle digest/schema mismatch; use the independently retained backup receipt"
    );
    validate(&bundle.payload, pins)?;
    Ok(bundle)
}

pub fn verify(input: &Path, pins: &Pins, expected_bundle: &str) -> Result<Receipt> {
    Ok(receipt(&load(input, pins, expected_bundle)?, pins, false))
}

/// Prepare a NEW, inactive paired state tree for a reviewed metadata-only
/// adoption. The source backup and running-state paths are never rewritten.
pub fn prepare_adoption(
    input: &Path,
    output: &Path,
    pins: &Pins,
    expected_bundle: &str,
    managed_config: &Path,
    expected_config_sha256: &str,
) -> Result<Receipt> {
    let mut bundle = load(input, pins, expected_bundle)?;
    let config_bytes = read(managed_config)?;
    ensure!(
        format!("{:x}", Sha256::digest(&config_bytes)) == expected_config_sha256,
        "adoption configuration digest differs from review"
    );
    let config: super::managed::ManagedConfig = serde_json::from_slice(&config_bytes)?;
    config.binding.validate(&config.environment)?;
    ensure!(
        config.environment == pins.environment,
        "adoption environment changed"
    );
    let previous = bundle
        .payload
        .catalog
        .binding
        .managed
        .as_ref()
        .context("adoption requires managed state")?;
    ensure!(
        bundle
            .payload
            .catalog
            .operations
            .values()
            .all(|o| o.public.status.is_terminal())
            && bundle
                .payload
                .journal
                .jobs
                .values()
                .all(|o| o.status.is_terminal()),
        "adoption requires all accepted work to be terminal"
    );
    ensure!(
        bundle
            .payload
            .catalog
            .users
            .get(&previous.admin_user)
            .is_some_and(|u| u.ready && u.state == AccountState::Active)
            && bundle
                .payload
                .journal
                .owners
                .get(&previous.admin_user)
                .is_some_and(|u| u.ready && u.state == AccountState::Active),
        "adoption requires the existing administrator allocation to be ready on both sides"
    );
    let mut expected = config.binding.clone();
    expected.adopted_vms = previous.adopted_vms.clone();
    ensure!(
        &expected == previous,
        "adoption may only extend explicit existing-resource bindings"
    );
    ensure!(
        config.binding.adopted_vms.len() > previous.adopted_vms.len()
            && previous
                .adopted_vms
                .iter()
                .all(|v| config.binding.adopted_vms.contains(v)),
        "adoption must add resources without changing prior bindings"
    );
    let additions = config
        .binding
        .adopted_vms
        .iter()
        .filter(|v| previous.adopted(v.vm.vm_id).is_none())
        .cloned()
        .collect::<Vec<_>>();
    for row in &additions {
        ensure!(
            !bundle.payload.catalog.vms.contains_key(&row.vm.vm_id)
                && !bundle.payload.journal.machines.contains_key(&row.vm.vm_id),
            "adopted VMID already has management history"
        );
        bundle
            .payload
            .catalog
            .vms
            .insert(row.vm.vm_id, row.vm.clone());
        bundle
            .payload
            .journal
            .machines
            .insert(row.vm.vm_id, row.machine_record(&pins.environment));
    }
    bundle.payload.catalog.binding.managed = Some(config.binding.clone());
    bundle.payload.journal.binding.managed = Some(config.binding);
    bundle.payload.config.binding = bundle.payload.catalog.binding.clone();
    bundle.payload.catalog.revision += 1;
    bundle
        .payload
        .catalog
        .audit
        .push(serde_json::json!({"event":"existing_resources_adopted",
        "at": super::store::now(), "previous_bundle_sha256": expected_bundle,
        "vm_ids": additions.iter().map(|a| a.vm.vm_id).collect::<Vec<_>>(),
        "guest_or_storage_effects": false}));
    validate(&bundle.payload, pins)?;
    bundle.payload_sha256 = sha(&bundle.payload)?;
    new_directory(output)?;
    write(&output.join("bundle.json"), &serde_json::to_vec(&bundle)?)?;
    // Keep prepared state quarantined until the deployment operator independently
    // reconciles every root-approved live resource and both old-state digests.
    let receipt = restore(
        output,
        &output.join("prepared-state"),
        pins,
        &bundle.payload_sha256,
    )?;
    write(
        &output.join("adoption-receipt.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(receipt)
}

/// Operator-only reconciliation after an independently verified offline shrink.
/// Produces new quarantined state; never touches disks, live DBs, or workers.
#[allow(clippy::too_many_arguments)] // Explicit offline pins and reviewed shrink parameters.
pub fn prepare_offline_adopted_shrink(
    input: &Path,
    output: &Path,
    pins: &Pins,
    expected_bundle: &str,
    managed_config: &Path,
    expected_config_sha256: &str,
    vm_id: u16,
    expected_disk_mib: u64,
    target_disk_mib: u64,
) -> Result<Receipt> {
    let mut bundle = load(input, pins, expected_bundle)?;
    let bytes = read(managed_config)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected_config_sha256,
        "reviewed configuration digest differs"
    );
    let mut config: super::managed::ManagedConfig = serde_json::from_slice(&bytes)?;
    let previous = bundle
        .payload
        .catalog
        .binding
        .managed
        .as_ref()
        .context("requires existing managed deployment")?
        .clone();
    ensure!(
        config.environment == pins.environment
            && target_disk_mib >= 4096
            && target_disk_mib < expected_disk_mib,
        "invalid shrink target/environment"
    );
    ensure!(
        bundle
            .payload
            .catalog
            .operations
            .values()
            .all(|o| o.public.status.is_terminal())
            && bundle
                .payload
                .journal
                .jobs
                .values()
                .all(|j| j.status.is_terminal()),
        "all accepted effects must be terminal"
    );
    let index = previous
        .adopted_vms
        .iter()
        .position(|v| v.vm.vm_id == vm_id)
        .context("VM is not adopted")?;
    let old = &previous.adopted_vms[index];
    ensure!(
        old.vm.disk_mib == expected_disk_mib
            && bundle
                .payload
                .catalog
                .vms
                .get(&vm_id)
                .is_some_and(|v| v.state == VmState::Stopped),
        "old size differs or stopped state is not journaled"
    );
    let next = config
        .binding
        .adopted_vms
        .get(index)
        .context("adoption missing")?;
    let mut allowed = old.clone();
    allowed.vm.disk_mib = target_disk_mib;
    allowed.vm.state = VmState::Stopped;
    let attachment = old.configuration["scsi0"]
        .as_str()
        .context("missing attachment")?
        .split(',')
        .filter(|v| !v.starts_with("size="))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let actual_attachment = next.configuration["scsi0"]
        .as_str()
        .context("missing replacement attachment")?;
    let actual_parts = actual_attachment
        .split(',')
        .filter(|v| !v.starts_with("size="))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let sizes = actual_attachment
        .split(',')
        .filter(|v| v.starts_with("size="))
        .collect::<Vec<_>>();
    let mib = format!("size={target_disk_mib}M");
    let gib = format!("size={}G", target_disk_mib / 1024);
    ensure!(
        actual_parts == attachment
            && sizes.len() == 1
            && (sizes[0] == mib || target_disk_mib.is_multiple_of(1024) && sizes[0] == gib),
        "replacement attachment differs from the reviewed target size/options"
    );
    allowed.configuration["scsi0"] = serde_json::json!(actual_attachment);
    for key in ["digest", "description"] {
        ensure!(
            old.configuration.get(key).is_some() == next.configuration.get(key).is_some(),
            "informational/configuration digest field presence changed"
        );
        if let Some(value) = next.configuration.get(key) {
            allowed.configuration[key] = value.clone();
        }
    }
    // Description is informational; all access, boot, networking and key fields
    // remain byte-for-byte pinned by equality below.

    for key in ["guid", "volsize", "refreservation"] {
        allowed.root_properties.insert(
            key.into(),
            next.root_properties
                .get(key)
                .context("missing root pin")?
                .clone(),
        );
    }
    for key in ["quota", "reservation"] {
        allowed.dataset_properties.insert(
            key.into(),
            next.dataset_properties
                .get(key)
                .context("missing allocation pin")?
                .clone(),
        );
    }
    ensure!(
        &allowed == next,
        "unrelated adopted VM/configuration/storage field changed"
    );
    ensure!(
        next.root_properties["origin"] == "-"
            && next.root_properties["guid"] != old.root_properties["guid"]
            && next.dataset_properties["quota"].parse::<u64>()?
                < old.dataset_properties["quota"].parse::<u64>()?,
        "replacement must be independent, newly identified, and release allocation"
    );
    let mut expected = previous.clone();
    expected.adopted_vms[index] = allowed;
    expected
        .imported_vms
        .iter_mut()
        .filter(|v| v.vm.vm_id == vm_id)
        .for_each(|v| v.vm.disk_mib = target_disk_mib);
    expected.scope.capacity_mib = config.binding.scope.capacity_mib;
    expected.scope.limits.disk_mib = config.binding.scope.limits.disk_mib;
    expected.scope.limits.budget_mib = config.binding.scope.limits.budget_mib;
    ensure!(
        expected == config.binding,
        "unrelated management authority changed"
    );
    // Only fingerprints from this verified, already-terminal pair can become
    // history pins. New effect validation never consults these pins.
    for operation in bundle.payload.catalog.operations.values() {
        if config.binding.validate_job(&operation.job).is_err() {
            config
                .binding
                .settled_job_pins
                .insert(host::fingerprint(&operation.job)?);
        }
    }
    config.binding.validate(&pins.environment)?;
    bundle.payload.catalog.binding.disk_capacity_mib = config.binding.scope.capacity_mib;
    bundle.payload.catalog.binding.managed = Some(config.binding.clone());
    bundle.payload.journal.binding.managed = Some(config.binding.clone());
    bundle.payload.config.binding = bundle.payload.catalog.binding.clone();
    bundle.payload.catalog.imported_vms = config.binding.imported_vms.clone();
    bundle.payload.journal.imported_vms = config.binding.imported_vms.clone();
    bundle
        .payload
        .catalog
        .vms
        .get_mut(&vm_id)
        .context("missing VM")?
        .disk_mib = target_disk_mib;
    let machine = bundle
        .payload
        .journal
        .machines
        .get_mut(&vm_id)
        .context("missing helper machine")?;
    machine.profile.disk_mib = target_disk_mib;
    machine.pinned_config = Some(config.binding.adopted_vms[index].configuration.clone());
    bundle.payload.catalog.revision += 1;
    bundle.payload.catalog.audit.push(
        serde_json::json!({"event":"offline_adopted_shrink_reconciled",
        "at":super::store::now(),"vm_id":vm_id,"old_disk_mib":expected_disk_mib,
        "new_disk_mib":target_disk_mib,"previous_bundle_sha256":expected_bundle,
        "requires_independent_live_reconciliation":true,"activates_workers":false}),
    );
    validate(&bundle.payload, pins)?;
    bundle.payload_sha256 = sha(&bundle.payload)?;
    new_directory(output)?;
    write(&output.join("bundle.json"), &serde_json::to_vec(&bundle)?)?;
    write(
        &output.join("managed-prepared.json"),
        &serde_json::to_vec_pretty(&config)?,
    )?;
    restore(
        output,
        &output.join("prepared-state"),
        pins,
        &bundle.payload_sha256,
    )
}

/// Offline root-reviewed compute ceiling expansion only. Existing resources,
/// accounts, permissions and work history are immutable. Never activates workers.
pub fn prepare_compute_expansion(
    input: &Path,
    output: &Path,
    pins: &Pins,
    expected_bundle: &str,
    managed_config: &Path,
    expected_config_sha256: &str,
) -> Result<Receipt> {
    let mut bundle = load(input, pins, expected_bundle)?;
    let bytes = read(managed_config)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected_config_sha256,
        "reviewed configuration digest differs"
    );
    let config: super::managed::ManagedConfig = serde_json::from_slice(&bytes)?;
    ensure!(
        config.environment == pins.environment,
        "environment changed"
    );
    config.binding.validate(&pins.environment)?;
    let previous = bundle
        .payload
        .catalog
        .binding
        .managed
        .as_ref()
        .context("compute expansion requires managed state")?;
    ensure!(
        bundle
            .payload
            .catalog
            .operations
            .values()
            .all(|o| o.public.status.is_terminal())
            && bundle
                .payload
                .journal
                .jobs
                .values()
                .all(|o| o.status.is_terminal()),
        "compute expansion requires all accepted effects to be terminal"
    );
    let old = &previous.scope;
    let new = &config.binding.scope;
    ensure!(
        new.min_memory_capacity_mib >= old.min_memory_capacity_mib
            && new.memory_capacity_mib >= old.memory_capacity_mib
            && new.cores_capacity >= old.cores_capacity
            && new.limits.min_memory_mib >= old.limits.min_memory_mib
            && new.limits.memory_mib >= old.limits.memory_mib
            && new.limits.cores >= old.limits.cores,
        "compute ceiling contraction refused"
    );
    let mut expected = previous.clone();
    expected.scope.min_memory_capacity_mib = new.min_memory_capacity_mib;
    expected.scope.memory_capacity_mib = new.memory_capacity_mib;
    expected.scope.cores_capacity = new.cores_capacity;
    expected.scope.limits.min_memory_mib = new.limits.min_memory_mib;
    expected.scope.limits.memory_mib = new.limits.memory_mib;
    expected.scope.limits.cores = new.limits.cores;
    ensure!(
        expected == config.binding && &expected != previous,
        "only an actual compute ceiling expansion is permitted"
    );
    let before = serde_json::to_value(&previous.scope)?;
    bundle.payload.catalog.binding.min_memory_capacity_mib = new.min_memory_capacity_mib;
    bundle.payload.catalog.binding.managed = Some(config.binding.clone());
    bundle.payload.journal.binding.managed = Some(config.binding.clone());
    bundle.payload.config.binding = bundle.payload.catalog.binding.clone();
    bundle.payload.catalog.revision += 1;
    bundle.payload.catalog.audit.push(serde_json::json!({
        "event":"managed_compute_ceilings_expanded", "at":super::store::now(),
        "previous_bundle_sha256":expected_bundle, "before_scope":before,
        "after_scope":new, "guest_or_storage_effects":false,
        "requires_independent_live_reconciliation":true, "activates_workers":false
    }));
    validate(&bundle.payload, pins)?;
    bundle.payload_sha256 = sha(&bundle.payload)?;
    new_directory(output)?;
    write(&output.join("bundle.json"), &serde_json::to_vec(&bundle)?)?;
    write(
        &output.join("managed-prepared.json"),
        &serde_json::to_vec_pretty(&config)?,
    )?;
    restore(
        output,
        &output.join("prepared-state"),
        pins,
        &bundle.payload_sha256,
    )
}

/// Always creates a NEW quarantined staging tree; never replaces active state,
/// rewrites route/configuration, deletes old state, or activates either worker.
pub fn restore(input: &Path, output: &Path, pins: &Pins, expected_bundle: &str) -> Result<Receipt> {
    let bundle = load(input, pins, expected_bundle)?;
    new_directory(output)?;
    write(&output.join(QUARANTINE), b"OFFLINE RESTORE: independent infrastructure reconciliation required; no activation tool exists.\n")?;
    let controller = output.join("controller");
    let helper = output.join("helper");
    new_directory(&controller)?;
    new_directory(&helper)?;
    for dir in [&controller, &helper] {
        write(
            &dir.join(QUARANTINE),
            b"Restored state: activation refused pending independent reconciliation.\n",
        )?;
    }
    write(&controller.join("identity.key"), &bundle.payload.identity)?;
    write(
        &controller.join("config.json"),
        &serde_json::to_vec_pretty(&bundle.payload.config)?,
    )?;
    write_record(
        &controller.join("state.redb"),
        CATALOG,
        &bundle.payload.catalog,
    )?;
    write_record(
        &helper.join("host-journal.redb"),
        JOURNAL,
        &bundle.payload.journal,
    )?;
    if let Some(world) = &bundle.payload.simulated_world {
        write(
            &output.join("simulated-world.json"),
            &serde_json::to_vec(world)?,
        )?;
    }
    write(
        &helper.join("initialized"),
        format!("{}\n", pins.environment).as_bytes(),
    )?;
    write(&controller.join("initialized"), b"controller-v1\n")?;
    write(
        &output.join("restore-receipt.json"),
        &serde_json::to_vec_pretty(&receipt(&bundle, pins, true))?,
    )?;
    fs::File::open(output)?.sync_all()?;
    Ok(receipt(&bundle, pins, true))
}

#[cfg(test)]
mod managed_backup_tests {
    use super::*;
    use crate::control::{host::HostBinding, managed::ManagedConfig, store::Binding, transport};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn adoption_prepares_only_a_new_quarantined_pair_and_retains_old_history() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let (controller, helper, pins) = pair(&root);
        let raw = root.join("raw-backup");
        let initial = backup(&controller, &helper, None, &raw, &pins, true).unwrap();
        let mut source = load(&raw, &pins, &initial.payload_sha256).unwrap();
        // Settle the synthetic bootstrap pair. These are record fixtures, not
        // evidence about live storage; deployment must separately reconcile it.
        let operation = source
            .payload
            .catalog
            .operations
            .values_mut()
            .next()
            .unwrap();
        operation.public.status = OperationStatus::Verified;
        let id = operation.public.operation_id.clone();
        let user = source.payload.catalog.users.values_mut().next().unwrap();
        user.ready = true;
        source.payload.journal.owners.insert(
            user.user_id.clone(),
            host::OwnerRecord {
                user_id: user.user_id.clone(),
                budget_mib: user.budget_mib,
                operation_id: id.clone(),
                ready: true,
                identities: BTreeMap::new(),
                policy_revision: 0,
                policy: Some(user.policy()),
                state: AccountState::Active,
                delete_operation: None,
            },
        );
        source.payload.journal.jobs.insert(
            id,
            host::HostJob {
                sequence: 1,
                fingerprint: host::fingerprint(&operation.job).unwrap(),
                job: operation.job.clone(),
                status: OperationStatus::Verified,
                step: "complete".into(),
                error: None,
                attempts: 1,
                steps: Vec::new(),
                started_at: None,
                updated_at: None,
                finished_at: None,
            },
        );
        source.payload.journal.sequence = 1;
        source.payload_sha256 = sha(&source.payload).unwrap();
        validate(&source.payload, &pins).unwrap();
        let input = root.join("settled-input");
        new_directory(&input).unwrap();
        write(
            &input.join("bundle.json"),
            &serde_json::to_vec(&source).unwrap(),
        )
        .unwrap();
        let before_bytes = fs::read(input.join("bundle.json")).unwrap();
        let config = root.join("adoption.json");
        let config_bytes = include_bytes!("../../tests/fixtures/managed-adoption-config.json");
        write(&config, config_bytes).unwrap();
        let expected_config = format!("{:x}", Sha256::digest(config_bytes));
        assert!(
            prepare_adoption(
                &input,
                &root.join("bad-digest"),
                &pins,
                &source.payload_sha256,
                &config,
                &"0".repeat(64)
            )
            .is_err()
        );
        let output = root.join("prepared-adoption");
        let result = prepare_adoption(
            &input,
            &output,
            &pins,
            &source.payload_sha256,
            &config,
            &expected_config,
        )
        .unwrap();
        assert!(result.quarantined && !result.activates_workers);
        let prepared = load(&output, &pins, &result.payload_sha256).unwrap();
        assert_eq!(fs::read(input.join("bundle.json")).unwrap(), before_bytes);
        assert_eq!(
            sha(&prepared.payload.catalog.operations).unwrap(),
            sha(&source.payload.catalog.operations).unwrap()
        );
        assert_eq!(
            sha(&prepared.payload.catalog.requests).unwrap(),
            sha(&source.payload.catalog.requests).unwrap()
        );
        assert_eq!(
            sha(&prepared.payload.catalog.devices).unwrap(),
            sha(&source.payload.catalog.devices).unwrap()
        );
        assert_eq!(
            sha(&prepared.payload.catalog.users).unwrap(),
            sha(&source.payload.catalog.users).unwrap()
        );
        assert_eq!(
            sha(&prepared.payload.journal.jobs).unwrap(),
            sha(&source.payload.journal.jobs).unwrap()
        );
        assert_eq!(prepared.payload.identity, source.payload.identity);
        assert_eq!(
            prepared.payload.catalog.imported_vms,
            source.payload.catalog.imported_vms
        );
        assert_eq!(
            prepared.payload.journal.sequence,
            source.payload.journal.sequence
        );
        assert_eq!(prepared.payload.catalog.vms[&101].owner, "erin");
        assert_eq!(prepared.payload.journal.machines[&101].owner, "erin");
        for path in [
            "prepared-state",
            "prepared-state/controller",
            "prepared-state/helper",
        ] {
            assert!(output.join(path).join(QUARANTINE).exists());
        }
        exercise_compute_expansion(&root, &output, &prepared, &pins);
        exercise_offline_shrink(&root, prepared, &pins);
        let mut changed: ManagedConfig = serde_json::from_slice(config_bytes).unwrap();
        changed.binding.admin_policy.budget_mib += 1;
        let altered = root.join("altered.json");
        let bytes = serde_json::to_vec(&changed).unwrap();
        write(&altered, &bytes).unwrap();
        assert!(
            prepare_adoption(
                &input,
                &root.join("bad-policy"),
                &pins,
                &source.payload_sha256,
                &altered,
                &format!("{:x}", Sha256::digest(&bytes))
            )
            .is_err()
        );
    }

    fn exercise_compute_expansion(root: &Path, input: &Path, source: &Bundle, pins: &Pins) {
        let original = fs::read(input.join("bundle.json")).unwrap();
        let previous = source.payload.catalog.binding.managed.as_ref().unwrap();
        let mut config = ManagedConfig {
            environment: pins.environment.clone(),
            binding: previous.clone(),
        };
        config.binding.scope.min_memory_capacity_mib = 16384;
        config.binding.scope.memory_capacity_mib = 32768;
        config.binding.scope.limits.min_memory_mib = 12288;
        config.binding.scope.limits.memory_mib = 32768;
        let path = root.join("compute-config.json");
        let bytes = serde_json::to_vec(&config).unwrap();
        write(&path, &bytes).unwrap();
        let output = root.join("compute-prepared");
        let receipt = prepare_compute_expansion(
            input,
            &output,
            pins,
            &source.payload_sha256,
            &path,
            &format!("{:x}", Sha256::digest(&bytes)),
        )
        .unwrap();
        assert!(receipt.quarantined && !receipt.activates_workers);
        let next = load(&output, pins, &receipt.payload_sha256).unwrap();
        assert_eq!(next.payload.identity, source.payload.identity);
        assert_eq!(
            next.payload.config.host_socket,
            source.payload.config.host_socket
        );
        assert_eq!(next.payload.catalog.binding.min_memory_capacity_mib, 16384);
        assert_eq!(
            next.payload.catalog.binding.managed.as_ref().unwrap(),
            &config.binding
        );
        assert_eq!(
            next.payload.journal.binding.managed.as_ref().unwrap(),
            &config.binding
        );
        for (a, b) in [
            (
                sha(&next.payload.catalog.users),
                sha(&source.payload.catalog.users),
            ),
            (
                sha(&next.payload.catalog.devices),
                sha(&source.payload.catalog.devices),
            ),
            (
                sha(&next.payload.catalog.vms),
                sha(&source.payload.catalog.vms),
            ),
            (
                sha(&next.payload.catalog.operations),
                sha(&source.payload.catalog.operations),
            ),
            (
                sha(&next.payload.catalog.requests),
                sha(&source.payload.catalog.requests),
            ),
            (
                sha(&next.payload.journal.jobs),
                sha(&source.payload.journal.jobs),
            ),
            (
                sha(&next.payload.journal.owners),
                sha(&source.payload.journal.owners),
            ),
            (
                sha(&next.payload.journal.machines),
                sha(&source.payload.journal.machines),
            ),
        ] {
            assert_eq!(a.unwrap(), b.unwrap());
        }
        assert_eq!(fs::read(input.join("bundle.json")).unwrap(), original);
        for side in ["", "controller", "helper"] {
            assert!(
                output
                    .join("prepared-state")
                    .join(side)
                    .join(QUARANTINE)
                    .is_file()
            );
        }
        assert!(
            prepare_compute_expansion(
                input,
                &root.join("compute-digest-refused"),
                pins,
                &source.payload_sha256,
                &path,
                &"0".repeat(64)
            )
            .is_err()
        );
        for case in 0..8 {
            let mut bad = config.clone();
            match case {
                0 => bad.binding.scope.limits.memory_mib = previous.scope.limits.memory_mib - 1,
                1 => bad.binding.admin_only_mutations = false,
                2 => bad.binding.scope.capacity_mib += 1024,
                3 => {
                    bad.binding.scope.owners.insert("foreign".into());
                }
                4 => bad.binding.guest_network.bridge = "foreign".into(),
                5 => bad.binding.adopted_vms[0].vm.memory_mib += 1024,
                6 => {
                    bad.binding.settled_job_pins.insert("0".repeat(64));
                }
                _ => bad.binding = previous.clone(),
            }
            let bytes = serde_json::to_vec(&bad).unwrap();
            let path = root.join(format!("compute-bad-{case}.json"));
            write(&path, &bytes).unwrap();
            let out = root.join(format!("compute-refused-{case}"));
            assert!(
                prepare_compute_expansion(
                    input,
                    &out,
                    pins,
                    &source.payload_sha256,
                    &path,
                    &format!("{:x}", Sha256::digest(&bytes))
                )
                .is_err()
            );
            assert!(!out.exists());
        }
        let mut active: Bundle = serde_json::from_slice(&original).unwrap();
        active
            .payload
            .catalog
            .operations
            .values_mut()
            .next()
            .unwrap()
            .public
            .status = OperationStatus::Queued;
        active.payload_sha256 = sha(&active.payload).unwrap();
        let active_input = root.join("compute-unsettled-input");
        new_directory(&active_input).unwrap();
        write(
            &active_input.join("bundle.json"),
            &serde_json::to_vec(&active).unwrap(),
        )
        .unwrap();
        assert!(
            prepare_compute_expansion(
                &active_input,
                &root.join("compute-unsettled-refused"),
                pins,
                &active.payload_sha256,
                &path,
                &format!("{:x}", Sha256::digest(&bytes))
            )
            .is_err()
        );
    }

    fn exercise_offline_shrink(root: &Path, mut source: Bundle, pins: &Pins) {
        let vm_id = 101;
        source.payload.catalog.vms.get_mut(&vm_id).unwrap().state = VmState::Stopped;
        let mut op = source
            .payload
            .catalog
            .operations
            .values()
            .next()
            .unwrap()
            .clone();
        op.public.operation_id = "existing-shutdown".into();
        op.public.kind = JobKind::Shutdown;
        op.public.vm_id = Some(vm_id);
        op.job.operation_id = op.public.operation_id.clone();
        op.job.kind = JobKind::Shutdown;
        op.job.owner = source.payload.catalog.users["erin"].clone();
        op.job.vm = Some(source.payload.catalog.vms[&vm_id].clone());
        let mut host_job = source.payload.journal.jobs.values().next().unwrap().clone();
        host_job.job = op.job.clone();
        host_job.fingerprint = host::fingerprint(&op.job).unwrap();
        source
            .payload
            .journal
            .jobs
            .insert(op.public.operation_id.clone(), host_job);
        source
            .payload
            .catalog
            .operations
            .insert(op.public.operation_id.clone(), op.clone());
        validate(&source.payload, pins).unwrap();
        source.payload_sha256 = sha(&source.payload).unwrap();
        let input = root.join("offline-shrink-input");
        new_directory(&input).unwrap();
        write(
            &input.join("bundle.json"),
            &serde_json::to_vec(&source).unwrap(),
        )
        .unwrap();
        let original = fs::read(input.join("bundle.json")).unwrap();
        let old_size = source.payload.catalog.vms[&vm_id].disk_mib;
        let mut next: ManagedConfig = serde_json::from_slice(include_bytes!(
            "../../tests/fixtures/managed-adoption-config.json"
        ))
        .unwrap();
        let row = &mut next.binding.adopted_vms[0];
        row.vm.disk_mib = 409600;
        row.vm.state = VmState::Stopped;
        row.configuration["scsi0"] = serde_json::json!("dev-erin:vm-101-disk-0,size=409600M");
        row.root_properties
            .insert("guid".into(), "987654321".into());
        row.root_properties
            .insert("volsize".into(), (400 * 1024u64.pow(3)).to_string());
        row.root_properties
            .insert("refreservation".into(), (407 * 1024u64.pow(3)).to_string());
        for key in ["quota", "reservation"] {
            row.dataset_properties
                .insert(key.into(), (425 * 1024u64.pow(3)).to_string());
        }
        next.binding
            .imported_vms
            .iter_mut()
            .filter(|v| v.vm.vm_id == vm_id)
            .for_each(|v| v.vm.disk_mib = 409600);
        let config = root.join("offline-next.json");
        let bytes = serde_json::to_vec(&next).unwrap();
        write(&config, &bytes).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let output = root.join("offline-prepared");
        let result = prepare_offline_adopted_shrink(
            &input,
            &output,
            pins,
            &source.payload_sha256,
            &config,
            &digest,
            vm_id,
            old_size,
            409600,
        )
        .unwrap();
        assert!(result.quarantined && !result.activates_workers);
        let prepared = load(&output, pins, &result.payload_sha256).unwrap();
        assert_eq!(fs::read(input.join("bundle.json")).unwrap(), original);
        assert_eq!(
            sha(&prepared.payload.catalog.operations).unwrap(),
            sha(&source.payload.catalog.operations).unwrap()
        );
        assert_eq!(
            sha(&prepared.payload.catalog.requests).unwrap(),
            sha(&source.payload.catalog.requests).unwrap()
        );
        assert_eq!(
            sha(&prepared.payload.journal.jobs).unwrap(),
            sha(&source.payload.journal.jobs).unwrap()
        );
        assert_eq!(prepared.payload.identity, source.payload.identity);
        assert_eq!(prepared.payload.catalog.vms[&vm_id].disk_mib, 409600);
        let binding = prepared.payload.catalog.binding.managed.as_ref().unwrap();
        assert!(binding.validate_job(&op.job).is_err());
        binding.validate_retained_job(&op.job, true).unwrap();
        assert!(binding.validate_retained_job(&op.job, false).is_err());
        let mut tampered = op.job.clone();
        tampered.vm.as_mut().unwrap().cores += 1;
        assert!(binding.validate_retained_job(&tampered, true).is_err());
        for case in 0..4 {
            let mut bad = next.clone();
            match case {
                0 => bad.binding.admin_only_mutations = false,
                1 => {
                    bad.binding.adopted_vms[0].configuration["sshkeys"] =
                        serde_json::json!("foreign")
                }
                2 => bad.binding.guest_network.bridge = "foreign".into(),
                _ => bad.binding.adopted_vms[0]
                    .root_properties
                    .insert("origin".into(), "foreign@snapshot".into())
                    .map(|_| ())
                    .unwrap_or(()),
            }
            let path = root.join(format!("bad-offline-{case}.json"));
            let bytes = serde_json::to_vec(&bad).unwrap();
            write(&path, &bytes).unwrap();
            assert!(
                prepare_offline_adopted_shrink(
                    &input,
                    &root.join(format!("bad-offline-{case}")),
                    pins,
                    &source.payload_sha256,
                    &path,
                    &format!("{:x}", Sha256::digest(&bytes)),
                    vm_id,
                    old_size,
                    409600
                )
                .is_err()
            );
        }
        assert!(
            output
                .join("prepared-state/controller")
                .join(QUARANTINE)
                .is_file()
        );
    }

    fn pair(root: &Path) -> (std::path::PathBuf, std::path::PathBuf, Pins) {
        let managed: ManagedConfig =
            serde_json::from_str(include_str!("../../tests/fixtures/managed-config.json")).unwrap();
        let controller = root.join("controller");
        let helper = root.join("helper");
        fs::DirBuilder::new().mode(0o700).create(&helper).unwrap();
        let binding = Binding {
            environment: managed.environment.clone(),
            backend: "managed".into(),
            disk_capacity_mib: managed.binding.scope.capacity_mib,
            min_memory_capacity_mib: managed.binding.scope.min_memory_capacity_mib,
            guest_images: managed
                .binding
                .images
                .iter()
                .map(|i| i.image_sha256.clone())
                .collect(),
            physical_trial: None,
            managed: Some(managed.binding.clone()),
        };
        let config = ServerConfig {
            schema: 1,
            binding,
            host_socket: root.join("host.sock"),
            network: transport::Network::Public,
            port: 0,
            migration_network: None,
        };
        transport::initialize(
            &controller,
            &config,
            &SecretKey::generate().public().to_string(),
        )
        .unwrap();
        let (_, store, key) = transport::open(&controller).unwrap();
        assert_eq!(
            store.read().unwrap().imported_vms,
            managed.binding.imported_vms
        );
        drop(store);
        let journal = Journal {
            schema: 7,
            binding: HostBinding {
                environment: managed.environment.clone(),
                backend: "managed".into(),
                pool_guid: managed.binding.pool_guid.clone(),
                physical_trial: None,
                managed: Some(managed.binding.clone()),
            },
            imported_vms: managed.binding.imported_vms.clone(),
            sequence: 0,
            control: None,
            owners: BTreeMap::new(),
            machines: BTreeMap::new(),
            jobs: BTreeMap::new(),
            disk_shrinks: BTreeMap::new(),
            migrations: BTreeMap::new(),
        };
        write_record(&helper.join("host-journal.redb"), JOURNAL, &journal).unwrap();
        write(
            &helper.join("initialized"),
            format!("{}\n", managed.environment).as_bytes(),
        )
        .unwrap();
        (
            controller,
            helper,
            Pins {
                environment: managed.environment,
                server_id: key.public().to_string(),
                pool_guid: managed.binding.pool_guid,
            },
        )
    }

    #[test]
    fn managed_pair_exports_imports_and_restores_only_into_quarantine() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let (controller, helper, pins) = pair(&root);
        let output = root.join("backup");
        let (_, active, _) = transport::open(&controller).unwrap();
        assert!(backup(&controller, &helper, None, &output, &pins, true).is_err());
        drop(active);
        let receipt = backup(&controller, &helper, None, &output, &pins, true).unwrap();
        let bundle = output.join("bundle.json");
        assert!(!receipt.activates_workers);
        verify(&output, &pins, &receipt.payload_sha256).unwrap();
        let restored = root.join("restored");
        let restored_receipt = restore(&output, &restored, &pins, &receipt.payload_sha256).unwrap();
        assert!(restored_receipt.quarantined && !restored_receipt.activates_workers);
        for side in ["controller", "helper"] {
            assert!(ensure_not_quarantined(&restored.join(side)).is_err());
        }
        assert!(transport::open(&restored.join("controller")).is_err());
        let data: Bundle = serde_json::from_slice(&read(&bundle).unwrap()).unwrap();
        assert!(!data.payload.catalog.imported_vms.is_empty());
        assert_eq!(
            data.payload.catalog.imported_vms,
            data.payload.journal.imported_vms
        );
        assert!(data.payload.catalog.vms.is_empty() && data.payload.journal.machines.is_empty());
    }

    #[test]
    fn managed_pair_refuses_import_or_root_scope_substitution() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let (controller, helper, pins) = pair(&root);
        let output = root.join("backup");
        backup(&controller, &helper, None, &output, &pins, true).unwrap();
        let mut data: Bundle =
            serde_json::from_slice(&read(&output.join("bundle.json")).unwrap()).unwrap();
        data.payload.journal.imported_vms.clear();
        assert!(validate(&data.payload, &pins).is_err());
        data.payload.journal.imported_vms = data.payload.catalog.imported_vms.clone();
        data.payload
            .journal
            .binding
            .managed
            .as_mut()
            .unwrap()
            .scope
            .root_guid = "999".into();
        assert!(validate(&data.payload, &pins).is_err());
    }
}
