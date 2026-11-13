use crate::control::JobKind;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Notify,
    time::{Instant, sleep, timeout},
};

mod adoption;
mod enrollment;
mod grow;
mod managed;
mod migration;
mod recovery;
mod schema_repair;
mod shrink;
mod worker;

use worker::{DEFAULT_GRACE, Worker};

pub use schema_repair::{ManagedSchemaRepairReceipt, repair_managed_schema};

use super::{
    managed::{ManagedBinding, ManagedConfig, Namespace},
    physical_trial::{TrialBinding, TrialConfig},
    protocol::{AccountState, Guest, OperationStatus, Role, Vm},
    store::{Job, PolicyChange},
};

pub const NODE: &str = "pve-lab";
pub const POOL: &str = "labpool";
pub const CONTROL_DATASET: &str = "labpool/control";
pub const LAB_ID_FILE: &str = "/etc/pve-dev-lab-id";
pub const MAX_HOST_MESSAGE: usize = 256 * 1024;
pub const SAFETY_FLOOR_MIB: u64 = 512;
pub const MIB: u64 = 1024 * 1024;

const JOURNAL_FILE: &str = "host-journal.redb";
const SENTINEL_FILE: &str = "initialized";
const JOURNAL: TableDefinition<u8, &[u8]> = TableDefinition::new("host_journal_v1");
const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const MANAGED_DEVICES: [&str; 2] = ["ide2", "scsi0"];
const DEVICE_PREFIXES: [&str; 16] = [
    "ide", "sata", "scsi", "virtio", "efidisk", "tpmstate", "unused", "net", "usb", "hostpci",
    "serial", "parallel", "rng", "virtiofs", "audio", "mp",
];
const FORBIDDEN_KEYS: [&str; 10] = [
    "args",
    "hookscript",
    "cicustom",
    "lock",
    "watchdog",
    "ivshmem",
    "vmstate",
    "parent",
    "template",
    "snaptime",
];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequest {
    VmStates {
        binding: Box<HostBinding>,
    },
    Inventory {
        binding: Box<HostBinding>,
    },
    Migration {
        environment: String,
        request: Box<super::migration::HostMigrationRequest>,
    },
    PlanEnrollment {
        environment: String,
        vm_id: u16,
        owner: String,
    },
    InspectOperation {
        environment: String,
        operation_id: String,
    },
    ImportedVms {
        binding: Box<HostBinding>,
    },
    Health,
    OwnedResources,
    Submit {
        environment: String,
        job: Job,
    },
    SubmitBound {
        binding: Box<HostBinding>,
        job: Job,
    },
    Status {
        environment: String,
        operation_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostResponse {
    pub environment: String,
    pub status: OperationStatus,
    pub step: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<HostBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<OwnedResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<super::protocol::OperationProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OwnedResources {
    pub binding: HostBinding,
    pub datasets: BTreeMap<String, Value>,
    pub storages: BTreeMap<String, Value>,
    pub unsettled_jobs: BTreeMap<String, OperationStatus>,
    pub machines: BTreeMap<u16, MachineRecord>,
    pub control: Option<Value>,
    pub owners: BTreeMap<String, OwnedOwnerResources>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OwnedOwnerResources {
    pub state: AccountState,
    pub ready: bool,
    pub dataset: Option<Value>,
    pub storage: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<HostResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug)]
pub struct Crash;

impl std::fmt::Display for Crash {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "simulated helper process crash")
    }
}

impl std::error::Error for Crash {}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Recover,
    Enroll,
    GrowPrepare,
    GrowVolume,
    GrowPartition,
    GrowFilesystem,
    GrowUnlock,
    ShrinkStage,
    ShrinkSwap,
    ShrinkUnlock,
    ShrinkFinish,
    Control,
    Dataset,
    Storage,
    Preflight,
    Vm,
    Volume,
    Image,
    Attach,
    CloudInit,
    Verify,
    Start,
    Shutdown,
    Stop,
    Unprotect,
    Destroy,
    Release,
    PolicyPreflight,
    PolicyFirst,
    PolicySecond,
    PolicyCommit,
    OwnerPreflight,
    OwnerStorage,
    OwnerDataset,
    OwnerRelease,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Recover => "recover",
            Step::Enroll => "enroll",
            Step::GrowPrepare => "grow_prepare",
            Step::GrowVolume => "grow_volume",
            Step::GrowPartition => "grow_partition",
            Step::GrowFilesystem => "grow_filesystem",
            Step::GrowUnlock => "grow_unlock",
            Step::ShrinkStage => "shrink_stage",
            Step::ShrinkSwap => "shrink_swap",
            Step::ShrinkUnlock => "shrink_unlock",
            Step::ShrinkFinish => "shrink_finish",
            Step::Control => "control",
            Step::Dataset => "dataset",
            Step::Storage => "storage",
            Step::Preflight => "preflight",
            Step::Vm => "vm",
            Step::Volume => "volume",
            Step::Image => "image",
            Step::Attach => "attach",
            Step::CloudInit => "cloud_init",
            Step::Verify => "verify",
            Step::Start => "start",
            Step::Shutdown => "shutdown",
            Step::Stop => "stop",
            Step::Unprotect => "unprotect",
            Step::Destroy => "destroy",
            Step::Release => "release",
            Step::PolicyPreflight => "policy_preflight",
            Step::PolicyFirst => "policy_first",
            Step::PolicySecond => "policy_second",
            Step::PolicyCommit => "policy_commit",
            Step::OwnerPreflight => "owner_preflight",
            Step::OwnerStorage => "owner_storage",
            Step::OwnerDataset => "owner_dataset",
            Step::OwnerRelease => "owner_release",
        }
    }

    fn persistent(self) -> bool {
        matches!(
            self,
            Step::Control
                | Step::Dataset
                | Step::Storage
                | Step::Vm
                | Step::Volume
                | Step::Image
                | Step::Attach
                | Step::CloudInit
        )
    }

    fn runtime(self) -> bool {
        matches!(
            self,
            Step::Start | Step::Shutdown | Step::Stop | Step::Unprotect
        )
    }
}

pub fn plan(kind: JobKind) -> Option<&'static [Step]> {
    match kind {
        JobKind::RecoverOperation => Some(&[Step::Recover]),
        JobKind::EnrollVm => Some(&[Step::Enroll, Step::Verify]),
        JobKind::GrowVm => Some(&[
            Step::GrowPrepare,
            Step::GrowVolume,
            Step::GrowPartition,
            Step::GrowFilesystem,
            Step::GrowUnlock,
        ]),
        JobKind::ShrinkVm => Some(&[Step::ShrinkStage, Step::ShrinkSwap, Step::ShrinkUnlock]),
        JobKind::FinishVmShrink => Some(&[Step::ShrinkFinish]),
        JobKind::EnsureUser => Some(&[Step::Control, Step::Dataset, Step::Storage]),
        JobKind::DeleteUser => Some(&[
            Step::OwnerPreflight,
            Step::OwnerStorage,
            Step::OwnerDataset,
            Step::OwnerRelease,
        ]),
        JobKind::UpdateUserPolicy => Some(&[
            Step::PolicyPreflight,
            Step::PolicyFirst,
            Step::PolicySecond,
            Step::PolicyCommit,
        ]),
        JobKind::Create => Some(&[
            Step::Preflight,
            Step::Vm,
            Step::Volume,
            Step::Attach,
            Step::CloudInit,
            Step::Verify,
        ]),
        JobKind::CreateGuest => Some(&[
            Step::Preflight,
            Step::Vm,
            Step::Volume,
            Step::Image,
            Step::Attach,
            Step::CloudInit,
            Step::Verify,
        ]),
        JobKind::Start => Some(&[Step::Verify, Step::Start]),
        JobKind::Shutdown => Some(&[Step::Verify, Step::Shutdown]),
        JobKind::Stop => Some(&[Step::Verify, Step::Stop]),
        JobKind::Delete => Some(&[
            Step::Verify,
            Step::Stop,
            Step::Unprotect,
            Step::Destroy,
            Step::Release,
        ]),
        JobKind::Migration | JobKind::Inspect => None,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Planned,
    Intent,
    Verified,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostBinding {
    pub environment: String,
    pub backend: String,
    pub pool_guid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_trial: Option<TrialBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedBinding>,
}

impl HostBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_environment(&self.environment) && valid_guid(&self.pool_guid),
            "invalid helper binding"
        );
        ensure!(
            self.managed.is_none() || self.physical_trial.is_none(),
            "mixed managed/trial bindings"
        );
        if let Some(m) = &self.managed {
            m.validate(&self.environment)?;
            ensure!(
                self.backend == "managed" && self.pool_guid == m.pool_guid,
                "managed backend/pool binding differs"
            );
            return Ok(());
        }
        match (&self.physical_trial, self.backend.as_str()) {
            (Some(trial), "physical_trial") => {
                trial.validate(&self.environment)?;
                ensure!(
                    trial.pool_guid == self.pool_guid,
                    "physical trial pool binding mismatch"
                );
            }
            (None, "lab" | "simulated") => {}
            _ => bail!("unsupported helper backend or mismatched physical scope"),
        }
        Ok(())
    }

    fn namespace(&self) -> Option<Namespace> {
        self.managed
            .as_ref()
            .map(|m| Namespace::from(&m.scope))
            .or_else(|| {
                self.physical_trial
                    .as_ref()
                    .map(|t| Namespace::from(&t.scope))
            })
    }
    fn limits(&self) -> super::protocol::ResourceLimits {
        self.managed
            .as_ref()
            .map(|m| m.scope.limits)
            .or_else(|| self.physical_trial.as_ref().and_then(|t| t.scope.limits))
            .unwrap_or_default()
    }
    fn expanded(&self) -> bool {
        self.managed.is_some()
            || self
                .physical_trial
                .as_ref()
                .is_some_and(|t| t.scope.limits.is_some())
    }
    fn job_plan(&self, job: &Job) -> Result<&'static [Step]> {
        if self.managed.as_ref().is_some_and(|m| {
            job.vm
                .as_ref()
                .is_some_and(|v| m.adopted(v.vm_id).is_some())
        }) {
            self.validate_job(job)?;
            return plan(job.kind).context("unsupported adopted operation");
        }
        validate_with_limits(
            job,
            self.limits(),
            self.expanded(),
            self.managed.as_ref().map(|m| &m.scope.vm_ids),
        )
    }
    pub fn validate_job(&self, job: &Job) -> Result<()> {
        if let Some(m) = &self.managed {
            return m.validate_job(job);
        }
        ensure!(
            !matches!(job.extension.as_deref(), Some(super::vm_change::JobExtension::Recover(spec))
            if spec.kind == super::vm_change::RecoveryKind::AcknowledgeFailedStart),
            "failed-start acknowledgment requires a managed binding"
        );
        ensure!(
            self.physical_trial
                .as_ref()
                .is_none_or(|b| job.disk_shrink.is_none() || b.scope.features.contains("resize")),
            "disk shrink is not qualified for physical-host backends"
        );
        ensure!(
            self.physical_trial
                .as_ref()
                .is_none_or(|b| job.kind != JobKind::Shutdown
                    || b.scope.features.contains("resize")
                    || b.scope.features.contains("migration")),
            "graceful shutdown is currently lab-qualified only"
        );
        if let Some(trial) = &self.physical_trial {
            if let Some(super::vm_change::JobExtension::IncomingMigration(plan)) =
                job.extension.as_deref()
            {
                ensure!(
                    plan.destination.environment == self.environment,
                    "incoming staging environment differs"
                );
            }
            if let Some(extension) = job.extension.as_deref() {
                let feature = match extension {
                    super::vm_change::JobExtension::Grow(_) => "resize",
                    super::vm_change::JobExtension::Enroll(_) => "enrollment",
                    super::vm_change::JobExtension::Recover(_) => "recovery",
                    super::vm_change::JobExtension::IncomingMigration(_) => "migration",
                };
                ensure!(
                    trial.scope.features.contains(feature),
                    "job feature is outside the immutable physical scope"
                );
            }
            ensure!(
                trial
                    .scope
                    .permits(&job.owner.user_id, job.vm.as_ref().map(|vm| vm.vm_id)),
                "helper job escapes explicit physical trial scope"
            );
            ensure!(
                (job.owner.user_id == "trial-admin") == (job.owner.role == Role::Admin),
                "physical trial role differs from owner scope"
            );
        }
        Ok(())
    }
}

pub fn validate_scope(journal: &Journal) -> Result<()> {
    journal.binding.validate()?;
    super::shrink::validate_journal(journal)?;
    super::vm_change::validate_journal(journal)?;
    ensure!(
        matches!(journal.schema, 1..=7)
            && if let Some(scope) = &journal.binding.physical_trial {
                journal.schema
                    >= if scope.scope.features.is_empty() {
                        4
                    } else {
                        6
                    }
            } else {
                journal.schema != 4
            },
        "helper schema/scope binding mismatch"
    );
    if let Some(trial) = &journal.binding.physical_trial {
        for (id, owner) in &journal.owners {
            ensure!(
                id == &owner.user_id && trial.scope.permits(id, None),
                "helper owner history escapes physical trial scope"
            );
        }
        for (id, machine) in &journal.machines {
            ensure!(
                *id == machine.vm_id && trial.scope.permits(&machine.owner, Some(*id)),
                "helper VM history escapes physical trial scope"
            );
        }
        for job in journal.jobs.values() {
            journal.binding.validate_job(&job.job)?;
        }
    }
    if let Some(m) = &journal.binding.managed {
        for adopted in &m.adopted_vms {
            let machine = journal
                .machines
                .get(&adopted.vm.vm_id)
                .context("adopted VM is missing from helper history")?;
            ensure!(
                machine.owner == adopted.vm.owner
                    && machine.profile == profile(&adopted.vm)
                    && machine.pinned_config.as_ref() == Some(&adopted.configuration),
                "adopted helper profile/configuration differs from root binding"
            );
        }
        ensure!(
            journal.schema == 7 && journal.imported_vms == m.imported_vms,
            "managed helper imports/schema differ from root pin"
        );
        for (id, o) in &journal.owners {
            ensure!(
                id == &o.user_id && m.scope.permits(id, None),
                "managed helper owner escapes scope"
            );
        }
        for (id, v) in &journal.machines {
            ensure!(
                *id == v.vm_id
                    && m.permits_vm(&v.owner, *id)
                    && (!journal.imported_vms.iter().any(|r| r.vm.vm_id == *id)
                        || m.adopted(*id).is_some()),
                "managed helper VM escapes scope"
            );
        }
        for r in journal.jobs.values() {
            m.validate_retained_job(&r.job, r.status.is_terminal())?;
        }
        ensure!(
            journal.disk_shrinks.is_empty() && journal.migrations.is_empty(),
            "unqualified managed helper feature history"
        );
    } else {
        ensure!(
            journal.imported_vms.is_empty(),
            "imports require root managed binding"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StepRecord {
    pub step: Step,
    pub phase: Phase,
    pub boot_id: Option<String>,
    pub upid: Option<String>,
    pub identity: Option<Value>,
    pub witness: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostJob {
    pub sequence: u64,
    pub fingerprint: String,
    pub job: Job,
    pub status: OperationStatus,
    pub step: String,
    pub error: Option<String>,
    pub attempts: u64,
    pub steps: Vec<StepRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OwnerRecord {
    pub user_id: String,
    pub budget_mib: u64,
    pub operation_id: String,
    pub ready: bool,
    pub identities: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "zero_revision")]
    pub policy_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<super::protocol::ResourcePolicy>,
    #[serde(default, skip_serializing_if = "AccountState::is_active")]
    pub state: AccountState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_operation: Option<String>,
}

fn zero_revision(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub disk_mib: u64,
    pub memory_mib: u64,
    pub min_memory_mib: u64,
    pub cores: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<Box<Guest>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Creating,
    Present,
    Deleting,
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MachineRecord {
    pub vm_id: u16,
    pub owner: String,
    pub profile: Profile,
    pub create_operation: String,
    pub delete_operation: Option<String>,
    pub state: MachineState,
    pub identities: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_config: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Journal {
    pub schema: u32,
    pub binding: HostBinding,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imported_vms: Vec<super::protocol::ImportedVm>,
    pub sequence: u64,
    pub control: Option<Value>,
    pub owners: BTreeMap<String, OwnerRecord>,
    pub machines: BTreeMap<u16, MachineRecord>,
    pub jobs: BTreeMap<String, HostJob>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub disk_shrinks: BTreeMap<u16, super::shrink::PendingShrink>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub migrations: BTreeMap<String, super::migration::HostTransferRecord>,
}

#[derive(Clone, Debug)]
pub enum BackendConfig {
    Managed {
        binding: Box<ManagedBinding>,
    },
    Lab {
        pool_guid: String,
        images: Vec<LabImage>,
    },
    Simulated {
        world: PathBuf,
    },
    PhysicalTrial {
        binding: Box<TrialBinding>,
        images: Vec<LabImage>,
    },
}

impl BackendConfig {
    fn managed(&self) -> Option<&ManagedBinding> {
        match self {
            Self::Managed { binding } => Some(binding),
            _ => None,
        }
    }
    fn physical_trial(&self) -> Option<&TrialBinding> {
        match self {
            Self::PhysicalTrial { binding, .. } => Some(binding),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub state_dir: PathBuf,
    pub environment: String,
    pub backend: BackendConfig,
    pub task_timeout: Duration,
    pub poll_interval: Duration,
    pub command_timeout: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabConfig {
    pub environment: String,
    pub pool_guid: String,
    #[serde(default)]
    pub images: Vec<LabImage>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabImage {
    pub path: PathBuf,
    pub sha256: String,
}

impl HostConfig {
    pub fn managed(state_dir: PathBuf, config_file: &Path) -> Result<Self> {
        let c = ManagedConfig::read(config_file)?;
        Ok(Self {
            state_dir,
            environment: c.environment,
            backend: BackendConfig::Managed {
                binding: Box::new(c.binding),
            },
            task_timeout: Duration::from_secs(900),
            poll_interval: Duration::from_secs(1),
            command_timeout: Duration::from_secs(600),
        })
    }
    pub fn physical_trial(state_dir: PathBuf, config_file: &Path) -> Result<Self> {
        let config = TrialConfig::read(config_file)?;
        Ok(Self {
            state_dir,
            environment: config.environment.clone(),
            backend: BackendConfig::PhysicalTrial {
                binding: Box::new(config.binding()),
                images: config.images,
            },
            task_timeout: Duration::from_secs(900),
            poll_interval: Duration::from_secs(1),
            command_timeout: Duration::from_secs(600),
        })
    }
    pub fn lab(state_dir: PathBuf, config_file: &Path) -> Result<Self> {
        let metadata = fs::metadata(config_file)
            .with_context(|| format!("reading lab config {}", config_file.display()))?;
        ensure!(
            metadata.is_file() && metadata.len() <= 4096,
            "lab config must be a small regular file"
        );
        let config: LabConfig =
            serde_json::from_slice(&fs::read(config_file)?).context("invalid lab config JSON")?;
        ensure!(
            valid_environment(&config.environment),
            "lab environment must be 32 lowercase hexadecimal characters"
        );
        ensure!(
            valid_guid(&config.pool_guid),
            "pool_guid must be the decimal ZFS pool GUID"
        );
        ensure!(
            config.images.len() <= 8
                && config
                    .images
                    .iter()
                    .all(|image| super::protocol::valid_image_digest(&image.sha256)
                        && image.path
                            == Path::new(&format!(
                                "/var/lib/pve-dev-images/{}.qcow2",
                                image.sha256
                            ))),
            "invalid content-addressed lab image allowlist"
        );
        ensure!(
            config
                .images
                .iter()
                .map(|image| &image.sha256)
                .collect::<BTreeSet<_>>()
                .len()
                == config.images.len(),
            "duplicate lab image digest"
        );
        Ok(Self {
            state_dir,
            environment: config.environment,
            backend: BackendConfig::Lab {
                pool_guid: config.pool_guid,
                images: config.images,
            },
            task_timeout: Duration::from_secs(900),
            poll_interval: Duration::from_secs(1),
            command_timeout: Duration::from_secs(600),
        })
    }

    pub fn simulated(state_dir: PathBuf, environment: &str, world: PathBuf) -> Self {
        Self {
            state_dir,
            environment: environment.into(),
            backend: BackendConfig::Simulated { world },
            task_timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(5),
            command_timeout: Duration::from_secs(10),
        }
    }
}

pub fn valid_environment(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_guid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn valid_label(value: &str, max: usize) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= max
        && bytes[0].is_ascii_lowercase()
        && (bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

pub(crate) fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

pub fn valid_upid(value: &str) -> bool {
    valid_upid_on(value, NODE)
}

fn valid_upid_on(value: &str, node: &str) -> bool {
    value.len() <= 256
        && value.starts_with(&format!("UPID:{node}:"))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b":@._-!".contains(&byte))
}

// A full resize can scan/copy a disk several times. This is derived only
// from an admitted profile; network clients cannot request arbitrary timeouts.
fn disk_maintenance_timeout(disk_mib: u64) -> Duration {
    Duration::from_secs(
        disk_mib
            .saturating_mul(8)
            .div_ceil(64)
            .saturating_add(1800)
            .clamp(3600, 72 * 3600),
    )
}

/// Domain tag for [`fingerprint`]. Bump the version when the meaning of a job
/// changes, so a stored fingerprint can never match a differently-read job.
const JOB_FINGERPRINT_DOMAIN: &[u8] = b"hearth-job-v1\n";

/// Identity of a job for request de-duplication and journal integrity.
///
/// Hashes canonical JSON (object keys sorted, via `serde_json::Value`'s
/// ordered map) behind a versioned domain tag, so the result does not depend
/// on struct field order.
pub fn fingerprint(job: &Job) -> Result<String> {
    let canonical = serde_json::to_vec(&serde_json::to_value(job)?)?;
    let mut digest = Sha256::new();
    digest.update(JOB_FINGERPRINT_DOMAIN);
    digest.update(&canonical);
    Ok(format!("{:x}", digest.finalize()))
}

fn profile(vm: &Vm) -> Profile {
    Profile {
        name: vm.name.clone(),
        disk_mib: vm.disk_mib,
        memory_mib: vm.memory_mib,
        min_memory_mib: vm.min_memory_mib,
        cores: vm.cores,
        guest: vm.guest.clone(),
    }
}

pub fn validate(job: &Job) -> Result<&'static [Step]> {
    validate_with_limits(job, super::protocol::ResourceLimits::default(), false, None)
}

fn validate_with_limits(
    job: &Job,
    limits: super::protocol::ResourceLimits,
    expanded: bool,
    managed_ids: Option<&BTreeSet<u16>>,
) -> Result<&'static [Step]> {
    let incoming = matches!(
        job.extension.as_deref(),
        Some(super::vm_change::JobExtension::IncomingMigration(_))
    );
    let expanded = expanded || incoming;
    let steps = plan(job.kind)
        .ok_or_else(|| anyhow!("unsupported helper operation kind {:?}", job.kind))?;
    let owner = &job.owner;
    if let Some(super::vm_change::JobExtension::IncomingMigration(plan)) = job.extension.as_deref()
    {
        plan.validate()?;
        let mut expected = plan.vm(super::migration::Role::Destination);
        expected.guest = None;
        ensure!(
            job.kind == JobKind::Create
                && job.operation_id
                    == super::migration_transport::target_create_id(&plan.migration_id)
                && job
                    .vm
                    .as_ref()
                    .is_some_and(|vm| profile(vm) == profile(&expected)
                        && vm.vm_id == expected.vm_id
                        && vm.owner == expected.owner)
                && job.disk_shrink.is_none()
                && job.policy_change.is_none(),
            "incoming staging differs from authorized destination plan"
        );
    } else if job.kind == JobKind::RecoverOperation {
        let Some(super::vm_change::JobExtension::Recover(spec)) = job.extension.as_deref() else {
            bail!("recovery specification missing")
        };
        ensure!(
            valid_operation_id(&spec.operation_id)
                && valid_operation_id(&spec.inspection_id)
                && owner.ready
                && job.policy_change.is_none()
                && job.disk_shrink.is_none(),
            "invalid recovery payload"
        );
    } else if job.kind == JobKind::GrowVm {
        let Some(super::vm_change::JobExtension::Grow(spec)) = job.extension.as_deref() else {
            bail!("growth specification missing")
        };
        let vm = job.vm.as_ref().context("growth VM missing")?;
        ensure!(
            spec.valid()
                && vm.disk_mib == spec.previous_disk_mib
                && spec.target_disk_mib <= limits.disk_mib
                && vm.state == super::protocol::VmState::Stopped
                && owner.ready
                && job.policy_change.is_none()
                && job.disk_shrink.is_none(),
            "growth requires an exact stopped source profile and admitted target"
        );
    } else if job.kind == JobKind::EnrollVm {
        let Some(super::vm_change::JobExtension::Enroll(spec)) = job.extension.as_deref() else {
            bail!("enrollment specification missing")
        };
        ensure!(
            spec.plan.eligible
                && job
                    .vm
                    .as_ref()
                    .is_some_and(|vm| vm.vm_id == spec.plan.vm.vm_id
                        && vm.owner == spec.plan.vm.owner
                        && profile(vm) == profile(&spec.plan.vm))
                && valid_operation_id(&spec.plan.plan_id),
            "enrollment profile differs from reviewed plan"
        );
    } else {
        ensure!(job.extension.is_none(), "unexpected extended job payload");
    }
    if matches!(job.kind, JobKind::ShrinkVm | JobKind::FinishVmShrink) {
        let spec = job
            .disk_shrink
            .as_ref()
            .context("disk shrink specification missing")?;
        let vm = job.vm.as_ref().context("disk shrink VM missing")?;
        ensure!(
            spec.valid() && job.policy_change.is_none() && owner.ready,
            "unsupported shrink profile"
        );
        if job.kind == JobKind::ShrinkVm {
            ensure!(
                spec.preparation_id.is_none()
                    && vm.disk_mib == spec.previous_disk_mib
                    && vm.state == super::protocol::VmState::Stopped,
                "shrink requires the exact stopped source profile"
            );
        } else {
            ensure!(
                spec.preparation_id
                    .as_deref()
                    .is_some_and(valid_operation_id)
                    && vm.disk_mib == spec.target_disk_mib
                    && vm.state == super::protocol::VmState::Stopped,
                "finish requires a prepared stopped target profile"
            );
        }
    } else {
        ensure!(job.disk_shrink.is_none(), "unexpected disk shrink payload");
    }
    ensure!(
        valid_label(&owner.user_id, 24),
        "owner name must be 1..24 lowercase ASCII letters, digits or hyphens"
    );
    ensure!(
        (64..=limits.budget_mib).contains(&owner.budget_mib),
        "owner disk budget must be 64..8192 MiB"
    );
    if job.kind == JobKind::DeleteUser {
        ensure!(
            job.vm.is_none()
                && job.policy_change.is_none()
                && owner.ready
                && owner.state == AccountState::Active
                && owner.role == Role::Member,
            "delete_user requires a frozen active member account without VM payload"
        );
        return Ok(steps);
    }
    ensure!(
        owner.state == AccountState::Active,
        "operation owner is inactive"
    );
    if job.kind == JobKind::UpdateUserPolicy {
        let change = job
            .policy_change
            .as_ref()
            .ok_or_else(|| anyhow!("policy payload missing"))?;
        ensure!(
            job.vm.is_none() && job.owner == change.previous,
            "policy operation owner/payload mismatch"
        );
        ensure!(
            change.previous.ready
                && change.target.ready
                && change.target.policy().valid_with_limits(limits),
            "invalid target policy"
        );
        ensure!(
            change.previous.user_id == change.target.user_id
                && change.previous.role == change.target.role
                && change.previous.policy_revision.checked_add(1)
                    == Some(change.target.policy_revision),
            "invalid policy identity or revision"
        );
        ensure!(
            change.previous.policy() != change.target.policy(),
            "unchanged policy"
        );
        return Ok(steps);
    }
    ensure!(
        job.policy_change.is_none(),
        "unexpected policy payload on VM/ensure operation"
    );
    if job.kind == JobKind::EnsureUser {
        ensure!(job.vm.is_none(), "ensure_user carries no VM");
        return Ok(steps);
    }
    let vm = job
        .vm
        .as_ref()
        .ok_or_else(|| anyhow!("VM operation requires a VM profile"))?;
    ensure!(
        job.kind != JobKind::CreateGuest || vm.guest.is_some(),
        "create_guest requires an immutable guest specification"
    );
    ensure!(
        job.kind != JobKind::Create || vm.guest.is_none(),
        "blank create cannot carry a guest specification"
    );
    if let Some(guest) = &vm.guest {
        guest.validate()?;
    }
    ensure!(
        vm.owner == owner.user_id,
        "VM owner does not match the operation owner"
    );
    ensure!(
        managed_ids.map_or_else(
            || (9001..=9099).contains(&vm.vm_id),
            |ids| ids.contains(&vm.vm_id)
        ),
        "VMID must be within the lab range 9001..9099"
    );
    ensure!(
        valid_label(&vm.name, 48),
        "VM name must be 1..48 lowercase ASCII letters, digits or hyphens"
    );
    ensure!(
        (16..=if vm.guest.is_some() || expanded {
            limits.disk_mib
        } else {
            512
        })
            .contains(&vm.disk_mib),
        "VM disk exceeds its lab profile limit"
    );
    ensure!(
        (128..=if vm.guest.is_some() || expanded {
            limits.memory_mib
        } else {
            512
        })
            .contains(&vm.memory_mib),
        "VM memory exceeds its lab profile limit"
    );
    ensure!(
        vm.min_memory_mib >= 128 && vm.min_memory_mib <= vm.memory_mib,
        "VM minimum memory must be 128 MiB..memory"
    );
    ensure!(
        (1..=if expanded { limits.cores } else { 4 }).contains(&vm.cores),
        "VM cores exceed explicit profile limit"
    );
    Ok(steps)
}

fn text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => {
            if *value {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_device(key: &str) -> bool {
    DEVICE_PREFIXES.iter().any(|prefix| {
        key.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn devices(config: &Value) -> BTreeSet<String> {
    config
        .as_object()
        .map(|map| map.keys().filter(|key| is_device(key)).cloned().collect())
        .unwrap_or_default()
}

fn attachment_bytes(attachment: &str) -> Result<u64> {
    let sizes: Vec<_> = attachment
        .split(',')
        .filter_map(|p| p.strip_prefix("size="))
        .collect();
    ensure!(
        sizes.len() == 1 && sizes[0].len() <= 32,
        "disk attachment must have one bounded size"
    );
    let value = sizes[0];
    let (number, power) = match value.as_bytes().last().copied() {
        Some(b'K') => (&value[..value.len() - 1], 1),
        Some(b'M') => (&value[..value.len() - 1], 2),
        Some(b'G') => (&value[..value.len() - 1], 3),
        Some(b'T') => (&value[..value.len() - 1], 4),
        Some(c) if c.is_ascii_digit() => (value, 0),
        _ => bail!("unsupported attachment size"),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    ensure!(
        !whole.is_empty()
            && whole.len() <= 20
            && fraction.len() <= 12
            && whole.bytes().all(|c| c.is_ascii_digit())
            && fraction.bytes().all(|c| c.is_ascii_digit()),
        "invalid attachment size number"
    );
    let scale = 10u128.pow(fraction.len() as u32);
    let numerator = whole
        .parse::<u128>()?
        .checked_mul(scale)
        .and_then(|n| {
            n.checked_add(if fraction.is_empty() {
                0
            } else {
                fraction.parse().unwrap_or(u128::MAX)
            })
        })
        .context("attachment size overflow")?;
    let bytes = numerator
        .checked_mul(1024u128.pow(power))
        .context("attachment size overflow")?;
    ensure!(
        bytes % scale == 0,
        "attachment size is not a whole number of bytes"
    );
    u64::try_from(bytes / scale).context("attachment size overflow")
}

fn normalized_pinned_config(config: &Value) -> Result<Value> {
    let map = config
        .as_object()
        .context("pinned VM configuration must be an object")?;
    let mut normalized = serde_json::Map::new();
    for (key, value) in map {
        if matches!(key.as_str(), "digest" | "protection" | "lock") {
            continue;
        }
        let mut value = text(value);
        if matches!(key.as_str(), "scsi0" | "ide2") {
            value = value
                .split(',')
                .filter(|p| !p.starts_with("size="))
                .collect::<Vec<_>>()
                .join(",");
        }
        normalized.insert(key.clone(), Value::String(value));
    }
    Ok(Value::Object(normalized))
}

fn safe_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("..")
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/:@._-!".contains(&byte))
}

fn safe_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/:@._-=,".contains(&byte))
}

#[derive(Clone)]
struct Context {
    environment: String,
    operation_id: String,
    kind: JobKind,
    owner: String,
    budget_mib: u64,
    vm: Option<Vm>,
    boot_id: String,
    policy_revision: u64,
    owner_policy: super::protocol::ResourcePolicy,
    policy_change: Option<Box<PolicyChange>>,
    scope: Option<Namespace>,
    managed: Option<ManagedBinding>,
    disk_shrink: Option<Box<super::shrink::ShrinkSpec>>,
    extension: Option<Box<super::vm_change::JobExtension>>,
}

impl Context {
    fn node(&self) -> &str {
        self.scope
            .as_ref()
            .map_or(NODE, |scope| scope.node.as_str())
    }
    fn control_dataset(&self) -> String {
        self.scope
            .as_ref()
            .map_or_else(|| CONTROL_DATASET.into(), Namespace::control_dataset)
    }
    fn vm(&self) -> Result<&Vm> {
        self.vm
            .as_ref()
            .ok_or_else(|| anyhow!("operation has no VM"))
    }
    fn vm_id(&self) -> u16 {
        self.vm.as_ref().map(|vm| vm.vm_id).unwrap_or_default()
    }
    fn control_marker(&self) -> String {
        format!("pve-dev-control:{}", self.environment)
    }
    fn owner_marker(&self) -> String {
        format!("pve-dev-control:{}:{}", self.environment, self.owner)
    }
    fn vm_marker(&self) -> String {
        format!(
            "pve-dev-control:{}:{}:{}",
            self.environment,
            self.owner,
            self.vm_id()
        )
    }
    fn adopted(&self) -> Option<&super::adoption::AdoptedVm> {
        self.managed.as_ref().and_then(|m| m.adopted(self.vm_id()))
    }
    fn dataset(&self) -> String {
        if let Some(a) = self.adopted() {
            return a.dataset.clone();
        }
        format!("{}/{}", self.control_dataset(), self.owner)
    }
    fn storage(&self) -> String {
        if let Some(a) = self.adopted() {
            return a.storage.clone();
        }
        format!(
            "{}{}",
            self.scope
                .as_ref()
                .map_or("ctl-", |scope| scope.storage_prefix.as_str()),
            self.owner
        )
    }
    fn root_name(&self) -> String {
        format!("vm-{}-disk-0", self.vm_id())
    }
    fn cloud_init_name(&self) -> String {
        format!("vm-{}-cloudinit", self.vm_id())
    }
    fn root_volume(&self) -> String {
        format!("{}/{}", self.dataset(), self.root_name())
    }
    fn cloud_init_volume(&self) -> String {
        format!("{}/{}", self.dataset(), self.cloud_init_name())
    }
    fn vm_path(&self) -> String {
        format!("/nodes/{}/qemu/{}", self.node(), self.vm_id())
    }
    fn delete_arguments(&self) -> [(&'static str, String); 2] {
        [
            ("purge", if self.scope.is_some() { "0" } else { "1" }.into()),
            ("destroy-unreferenced-disks", "0".into()),
        ]
    }
    fn expected_config(&self) -> Result<Vec<(&'static str, String)>> {
        let vm = self.vm()?;
        let mut config = vec![
            ("name", vm.name.clone()),
            ("description", self.vm_marker()),
            ("cores", vm.cores.to_string()),
            ("memory", vm.memory_mib.to_string()),
            ("balloon", vm.min_memory_mib.to_string()),
            (
                "cpu",
                self.managed
                    .as_ref()
                    .map_or("qemu64", |m| m.cpu.as_str())
                    .into(),
            ),
            ("kvm", if self.managed.is_some() { "1" } else { "0" }.into()),
            ("scsihw", "virtio-scsi-pci".into()),
            ("cpuunits", "100".into()),
        ];
        if let Some(guest) = &vm.guest {
            config.extend([
                (
                    "net0",
                    if let Some(m) = &self.managed {
                        format!(
                            "virtio=02:{}:{}:{}:{:02X}:{:02X},bridge={}",
                            self.environment[0..2].to_ascii_uppercase(),
                            self.environment[2..4].to_ascii_uppercase(),
                            self.environment[4..6].to_ascii_uppercase(),
                            vm.vm_id >> 8,
                            vm.vm_id & 255,
                            m.guest_network.bridge
                        )
                    } else {
                        format!(
                            "virtio=02:50:00:00:00:{:02X},bridge=vmbr77",
                            vm.vm_id - 9000
                        )
                    },
                ),
                (
                    "ipconfig0",
                    if let Some(m) = &self.managed {
                        m.guest_network.ipconfig(vm.vm_id)?
                    } else {
                        format!("ip={}/24,gw=10.77.0.1", super::guest::address(vm.vm_id)?)
                    },
                ),
                ("citype", "nocloud".into()),
                ("ciupgrade", "0".into()),
                (
                    "nameserver",
                    self.managed
                        .as_ref()
                        .map_or_else(|| "10.77.0.1".into(), |m| m.guest_network.dns.to_string()),
                ),
                (
                    "searchdomain",
                    if self.managed.is_some() {
                        "guest.invalid"
                    } else {
                        "lab.invalid"
                    }
                    .into(),
                ),
                ("sshkeys", super::guest::encode_key(&guest.ssh_public_key)),
                ("serial0", "socket".into()),
                ("vga", "serial0".into()),
            ]);
        }
        Ok(config)
    }

    fn managed_devices(&self) -> BTreeSet<String> {
        let mut devices: BTreeSet<String> = MANAGED_DEVICES
            .iter()
            .map(|device| device.to_string())
            .collect();
        if self.vm.as_ref().is_some_and(|vm| vm.guest.is_some()) {
            devices.extend(["net0".into(), "serial0".into()]);
        }
        devices
    }
}

pub struct Facts {
    pub lab_id: String,
    pub hostname: String,
    pub boot_id: String,
    pub machine_id_sha256: Option<String>,
}

pub struct LabBackend {
    command_timeout: Duration,
    images: Vec<LabImage>,
    scope: Option<Namespace>,
}

impl LabBackend {
    async fn image(&self, vm: &Vm) -> Result<PathBuf> {
        let guest = vm
            .guest
            .as_ref()
            .ok_or_else(|| anyhow!("guest specification missing"))?;
        let image = self
            .images
            .iter()
            .find(|image| image.sha256 == guest.image_sha256)
            .ok_or_else(|| anyhow!("image digest is not in the root-owned lab allowlist"))?;
        let path = image.path.clone();
        let expected = image.sha256.clone();
        let checked = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            ensure!(
                path == Path::new(&format!("/var/lib/pve-dev-images/{expected}.qcow2")),
                "image must use the fixed content-addressed lab image directory"
            );
            ensure!(
                path.canonicalize()? == path,
                "image path may not contain symlinks"
            );
            for ancestor in path.ancestors() {
                let metadata = fs::metadata(ancestor)?;
                ensure!(
                    metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
                    "image ancestry must be root-owned and not group/world writable"
                );
            }
            let metadata = fs::metadata(&path)?;
            ensure!(
                metadata.is_file()
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o222 == 0
                    && metadata.len() <= 1024 * MIB,
                "image must be a single-link read-only regular file of at most 1 GiB"
            );
            let mut source = fs::File::open(&path)?;
            let mut hash = Sha256::new();
            let mut buffer = vec![0_u8; 1024 * 1024];
            loop {
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            ensure!(
                format!("{:x}", hash.finalize()) == expected,
                "pinned guest image checksum mismatch"
            );
            Ok(path)
        })
        .await??;
        let info: Value = serde_json::from_str(
            &self
                .run(
                    "qemu-img",
                    vec![
                        "info".into(),
                        "--output=json".into(),
                        checked.display().to_string(),
                    ],
                )
                .await?,
        )?;
        super::guest::image_metadata(&info, vm.disk_mib)?;
        Ok(checked)
    }

    async fn run(&self, program: &str, arguments: Vec<String>) -> Result<String> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(&arguments)
            .gid(0)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .with_context(|| format!("spawning {program}"))?;
        let output = timeout(self.command_timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                anyhow!(
                    "{program} {} timed out; its effect is uncertain",
                    arguments.first().cloned().unwrap_or_default()
                )
            })??;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail: String = stderr.trim().chars().take(2000).collect();
            bail!(
                "{program} {} failed ({}): {detail}",
                arguments
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
                output.status
            );
        }
        String::from_utf8(output.stdout).context("command output is not UTF-8")
    }

    async fn pvesh(
        &self,
        method: &str,
        path: &str,
        arguments: &[(&str, String)],
    ) -> Result<String> {
        ensure!(safe_path(path), "refusing unsafe API path {path:?}");
        let mut argv = vec![method.to_string(), path.to_string()];
        for (key, value) in arguments {
            ensure!(
                key.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
                "refusing unsafe API key"
            );
            ensure!(
                safe_value(value)
                    || (*key == "sshkeys"
                        && value.len() <= 200
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"%-_.~".contains(&byte))),
                "refusing unsafe API value for {key}"
            );
            argv.push(format!("--{key}"));
            argv.push(value.clone());
        }
        argv.push("--output-format".into());
        argv.push("json".into());
        self.run("pvesh", argv).await
    }

    async fn zfs(&self, arguments: &[&str]) -> Result<String> {
        self.run(
            "zfs",
            arguments.iter().map(|value| value.to_string()).collect(),
        )
        .await
    }
}

pub fn parse_task(output: &str) -> Result<String> {
    parse_task_on(output, NODE)
}

fn parse_task_on(output: &str, node: &str) -> Result<String> {
    let line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow!("missing Proxmox task receipt"))?;
    let upid: String = serde_json::from_str(line.trim())
        .context("final output line is not a JSON task receipt")?;
    ensure!(
        valid_upid_on(&upid, node),
        "invalid Proxmox task receipt {upid:?}"
    );
    Ok(upid)
}

pub enum Backend {
    Lab(Box<LabBackend>),
    Simulated(SimulatedWorld),
}

fn allocation_space(pool: u64, reserved_parent: Option<u64>, allocating: bool) -> Result<u64> {
    if let Some(available) = reserved_parent {
        // The parent has already reserved its full immutable cap. Descendant
        // reservations consume that grant; do not charge it again against
        // globally unreserved pool space. Keep the global safety floor too.
        ensure!(
            !allocating || pool >= SAFETY_FLOOR_MIB * MIB,
            "pool is below the physical trial's global safety floor"
        );
        Ok(available)
    } else {
        Ok(pool)
    }
}

impl Backend {
    fn scope(&self) -> Option<&Namespace> {
        match self {
            Self::Lab(lab) => lab.scope.as_ref(),
            Self::Simulated(_) => None,
        }
    }
    fn pool(&self) -> &str {
        self.scope().map_or(POOL, |scope| scope.pool.as_str())
    }
    fn node(&self) -> &str {
        self.scope().map_or(NODE, |scope| scope.node.as_str())
    }
    fn control_dataset(&self) -> String {
        self.scope()
            .map_or_else(|| CONTROL_DATASET.into(), Namespace::control_dataset)
    }
    async fn available(&self, allocating: bool) -> Result<String> {
        let pool = self.zfs_get(self.pool(), &["available"]).await?["available"].parse::<u64>()?;
        let parent = if let Some(scope) = self.scope() {
            Some(self.zfs_get(&scope.root_dataset, &["available"]).await?["available"].parse()?)
        } else {
            None
        };
        Ok(allocation_space(pool, parent, allocating)?.to_string())
    }
    async fn check_image(&self, vm: &Vm) -> Result<()> {
        if let Backend::Lab(lab) = self {
            lab.image(vm).await?;
        }
        Ok(())
    }

    async fn import_image(&self, context: &Context) -> Result<()> {
        let vm = context.vm()?;
        let guest = vm
            .guest
            .as_ref()
            .ok_or_else(|| anyhow!("guest specification missing"))?;
        let name = context.root_volume();
        match self {
            Backend::Lab(lab) => {
                let image = lab.image(vm).await?;
                let target = format!("/dev/zvol/{name}");
                lab.run("udevadm", vec!["settle".into(), "--timeout=30".into()])
                    .await?;
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if fs::metadata(&target)
                        .is_ok_and(|metadata| metadata.file_type().is_block_device())
                    {
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "owned root zvol device did not appear; import has not started"
                    );
                    sleep(Duration::from_millis(200)).await;
                }
                lab.run(
                    "qemu-img",
                    vec![
                        "convert".into(),
                        "-n".into(),
                        "-f".into(),
                        "qcow2".into(),
                        "-O".into(),
                        "raw".into(),
                        image.display().to_string(),
                        target.clone(),
                    ],
                )
                .await?;
                lab.run("blockdev", vec!["--flushbufs".into(), target])
                    .await?;
                lab.run(
                    "qemu-img",
                    vec![
                        "compare".into(),
                        "-f".into(),
                        "qcow2".into(),
                        "-F".into(),
                        "raw".into(),
                        image.display().to_string(),
                        format!("/dev/zvol/{name}"),
                    ],
                )
                .await?;
                lab.zfs(&[
                    "set",
                    &format!("org.pve-dev:image={}", guest.image_sha256),
                    &name,
                ])
                .await?;
                lab.run("zpool", vec!["sync".into(), self.pool().into()])
                    .await?;
            }
            Backend::Simulated(world) => world.edit(|world| {
                let volume = world
                    .datasets
                    .get_mut(&name)
                    .ok_or_else(|| anyhow!("root volume missing"))?;
                volume.insert("org.pve-dev:image".into(), guest.image_sha256.clone());
                Ok(())
            })?,
        }
        Ok(())
    }

    async fn facts(&self) -> Result<Facts> {
        match self {
            Backend::Lab(lab) => {
                let read = |path: &'static str| async move {
                    tokio::fs::read_to_string(path)
                        .await
                        .map(|value| value.trim().to_string())
                        .with_context(|| format!("reading {path}"))
                };
                Ok(Facts {
                    lab_id: if lab.scope.is_some() {
                        String::new()
                    } else {
                        read(LAB_ID_FILE).await?
                    },
                    hostname: read("/proc/sys/kernel/hostname").await?,
                    boot_id: read("/proc/sys/kernel/random/boot_id").await?,
                    machine_id_sha256: if lab.scope.is_some() {
                        Some(format!(
                            "{:x}",
                            Sha256::digest(tokio::fs::read("/etc/machine-id").await?)
                        ))
                    } else {
                        None
                    },
                })
            }
            Backend::Simulated(world) => {
                let world = world.load()?;
                Ok(Facts {
                    lab_id: world.lab_id,
                    hostname: world.hostname,
                    boot_id: world.boot_id,
                    machine_id_sha256: None,
                })
            }
        }
    }

    async fn pool_guid(&self) -> Result<String> {
        match self {
            Backend::Lab(lab) => Ok(lab
                .run(
                    "zpool",
                    ["get", "-H", "-p", "-o", "value", "guid", self.pool()]
                        .map(String::from)
                        .to_vec(),
                )
                .await?
                .trim()
                .to_string()),
            Backend::Simulated(world) => Ok(world.load()?.pool_guid),
        }
    }

    async fn zfs_names(&self) -> Result<BTreeSet<String>> {
        match self {
            Backend::Lab(lab) => Ok(lab
                .zfs(&[
                    "list",
                    "-H",
                    "-o",
                    "name",
                    "-t",
                    "filesystem,volume",
                    "-r",
                    self.pool(),
                ])
                .await?
                .lines()
                .map(str::to_string)
                .collect()),
            Backend::Simulated(world) => Ok(world.load()?.datasets.keys().cloned().collect()),
        }
    }

    async fn zfs_get(&self, name: &str, keys: &[&str]) -> Result<BTreeMap<String, String>> {
        let values = match self {
            Backend::Lab(lab) => {
                let output = lab
                    .zfs(&[
                        "get",
                        "-H",
                        "-p",
                        "-o",
                        "property,value",
                        &keys.join(","),
                        name,
                    ])
                    .await?;
                output
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect()
            }
            Backend::Simulated(world) => world.load()?.zfs_get(name, keys)?,
        };
        for key in keys {
            ensure!(
                values.contains_key(*key),
                "ZFS property {key} missing for {name}"
            );
        }
        Ok(values)
    }

    async fn zfs_create(
        &self,
        name: &str,
        volume_mib: Option<u64>,
        properties: &[(&str, String)],
    ) -> Result<()> {
        match self {
            Backend::Lab(lab) => {
                let mut argv = vec!["create".to_string()];
                if let Some(size) = volume_mib {
                    argv.push("-V".into());
                    argv.push(format!("{size}M"));
                }
                for (key, value) in properties {
                    ensure!(safe_value(value), "refusing unsafe ZFS property value");
                    argv.push("-o".into());
                    argv.push(format!("{key}={value}"));
                }
                argv.push(name.to_string());
                lab.run("zfs", argv).await.map(|_| ())
            }
            Backend::Simulated(world) => {
                world.edit(|world| world.zfs_create(name, volume_mib, properties))
            }
        }
    }

    async fn set_policy_property(&self, dataset: &str, key: &str, bytes: u64) -> Result<()> {
        ensure!(
            matches!(key, "quota" | "reservation") && bytes > 0,
            "refusing unlimited/unknown policy property"
        );
        match self {
            Backend::Lab(lab) => lab
                .run(
                    "zfs",
                    vec!["set".into(), format!("{key}={bytes}"), dataset.into()],
                )
                .await
                .map(|_| ()),
            Backend::Simulated(world) => world.edit(|world| {
                let old = world
                    .datasets
                    .get(dataset)
                    .ok_or_else(|| anyhow!("dataset missing"))?
                    .get(key)
                    .cloned();
                let usage = world.used(dataset)
                    + world.datasets[dataset]
                        .get("extra_used")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                let quota: u64 = world.datasets[dataset]
                    .get("quota")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let reservation: u64 = world.datasets[dataset]
                    .get("reservation")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                ensure!(
                    key != "quota" || (bytes >= usage && bytes >= reservation),
                    "quota below used/reservation"
                );
                ensure!(
                    key != "reservation" || bytes <= quota,
                    "reservation exceeds quota"
                );
                world
                    .datasets
                    .get_mut(dataset)
                    .unwrap()
                    .insert(key.into(), bytes.to_string());
                if world.used(POOL) > world.pool_size_mib * MIB {
                    if let Some(old) = old {
                        world
                            .datasets
                            .get_mut(dataset)
                            .unwrap()
                            .insert(key.into(), old);
                    }
                    bail!("insufficient pool space for reservation");
                }
                Ok(())
            }),
        }
    }

    async fn owner_snapshots(&self, dataset: &str) -> Result<Vec<String>> {
        match self {
            Backend::Lab(lab) => Ok(lab
                .run(
                    "zfs",
                    vec![
                        "list".into(),
                        "-H".into(),
                        "-o".into(),
                        "name".into(),
                        "-t".into(),
                        "snapshot,bookmark".into(),
                        "-r".into(),
                        dataset.into(),
                    ],
                )
                .await?
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()),
            Backend::Simulated(world) => Ok(world
                .load()?
                .datasets
                .keys()
                .filter(|name| {
                    (name.starts_with(&format!("{dataset}@"))
                        || name.starts_with(&format!("{dataset}#")))
                        || (name.starts_with(&format!("{dataset}/")) && name.contains('@'))
                })
                .cloned()
                .collect()),
        }
    }

    async fn inspect_empty_owner(&self, dataset: &str, probe: &Path) -> Result<()> {
        match self {
            Backend::Simulated(world) => {
                let world = world.load()?;
                let properties = &world.datasets[dataset];
                ensure!(
                    properties
                        .get("file_count")
                        .is_none_or(|value| value == "0"),
                    "owner filesystem contains files; refusing account deletion"
                );
                ensure!(
                    !properties.contains_key("probe_failure"),
                    "read-only filesystem inspection unavailable; retaining allocation"
                );
                Ok(())
            }
            Backend::Lab(lab) => {
                // A crash can leave an inspection mount. Never guess that an
                // existing path/mount is ours or unmount it speculatively.
                fs::DirBuilder::new().mode(0o700).create(probe).context("fresh private empty-owner inspection path required; preserve any leftover mount for investigation")?;
                let probe_text = probe
                    .to_str()
                    .ok_or_else(|| anyhow!("inspection path must be UTF-8"))?
                    .to_string();
                lab.run(
                    "mount",
                    vec![
                        "-t".into(),
                        "zfs".into(),
                        "-o".into(),
                        "ro,zfsutil".into(),
                        dataset.into(),
                        probe_text.clone(),
                    ],
                )
                .await
                .context(
                    "cannot establish read-only owner filesystem inspection; allocation retained",
                )?;
                let info: Value = serde_json::from_str(
                    &lab.run(
                        "findmnt",
                        vec![
                            "--json".into(),
                            "--target".into(),
                            probe_text.clone(),
                            "--output".into(),
                            "SOURCE,FSTYPE,OPTIONS,TARGET".into(),
                        ],
                    )
                    .await?,
                )?;
                let mounts = info["filesystems"]
                    .as_array()
                    .ok_or_else(|| anyhow!("mount inspection missing"))?;
                ensure!(
                    mounts.len() == 1
                        && mounts[0]["source"] == dataset
                        && mounts[0]["fstype"] == "zfs"
                        && mounts[0]["target"] == probe_text
                        && text(&mounts[0]["options"])
                            .split(',')
                            .any(|option| option == "ro")
                        && !text(&mounts[0]["options"])
                            .split(',')
                            .any(|option| option == "rw"),
                    "owner inspection mount identity/read-only proof failed; preserve for investigation"
                );
                let mut empty = true;
                for entry in fs::read_dir(probe)? {
                    if entry?.file_name() != ".zfs" {
                        empty = false;
                        break;
                    }
                }
                lab.run("umount", vec![probe_text])
                    .await
                    .context("inspection unmount failed; preserve mount and allocation")?;
                fs::remove_dir(probe)?;
                ensure!(
                    empty,
                    "owner filesystem contains files or directories; refusing account deletion"
                );
                Ok(())
            }
        }
    }

    async fn remove_owner_storage(&self, storage: &str) -> Result<()> {
        match self {
            Backend::Lab(lab) => lab
                .pvesh("delete", &format!("/storage/{storage}"), &[])
                .await
                .map(|_| ()),
            Backend::Simulated(world) => world.edit(|world| {
                ensure!(
                    world.storages.remove(storage).is_some(),
                    "owner storage absent before effect"
                );
                Ok(())
            }),
        }
    }

    async fn remove_empty_owner_dataset(&self, dataset: &str) -> Result<()> {
        match self {
            Backend::Lab(lab) => lab
                .run("zfs", vec!["destroy".into(), dataset.into()])
                .await
                .map(|_| ()),
            Backend::Simulated(world) => world.edit(|world| {
                ensure!(
                    !world
                        .datasets
                        .keys()
                        .any(|name| name.starts_with(&format!("{dataset}/"))
                            || name.starts_with(&format!("{dataset}@"))),
                    "owner has children or snapshots"
                );
                ensure!(
                    world.datasets[dataset]
                        .get("file_count")
                        .is_none_or(|value| value == "0"),
                    "owner filesystem is not empty"
                );
                ensure!(
                    world.datasets.remove(dataset).is_some(),
                    "owner dataset absent before effect"
                );
                Ok(())
            }),
        }
    }

    async fn api_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        match self {
            Backend::Lab(lab) => serde_json::from_str(lab.pvesh("get", path, query).await?.trim())
                .with_context(|| format!("decoding {path}")),
            Backend::Simulated(world) => world.load()?.api_get(path, query),
        }
    }

    async fn api_set(&self, path: &str, arguments: &[(&str, String)]) -> Result<()> {
        match self {
            Backend::Lab(lab) => lab.pvesh("set", path, arguments).await.map(|_| ()),
            Backend::Simulated(world) => world.edit(|world| world.api_set(path, arguments)),
        }
    }

    async fn api_create(&self, path: &str, arguments: &[(&str, String)]) -> Result<()> {
        match self {
            Backend::Lab(lab) => lab.pvesh("create", path, arguments).await.map(|_| ()),
            Backend::Simulated(world) => world.edit(|world| world.api_create(path, arguments)),
        }
    }

    async fn api_task(
        &self,
        method: &str,
        path: &str,
        arguments: &[(&str, String)],
    ) -> Result<String> {
        match self {
            Backend::Lab(lab) => {
                parse_task_on(&lab.pvesh(method, path, arguments).await?, self.node())
            }
            Backend::Simulated(world) => {
                world.edit(|world| world.api_task(method, path, arguments))
            }
        }
    }

    fn trip(&self, step: Step, point: FaultPoint) -> Result<()> {
        match self {
            Backend::Lab(_) => Ok(()),
            Backend::Simulated(world) => {
                let tripped = world.edit(|world| {
                    Ok(
                        match world
                            .faults
                            .iter()
                            .position(|fault| fault.step == step && fault.point == point)
                        {
                            Some(index) => {
                                world.faults.remove(index);
                                true
                            }
                            None => false,
                        },
                    )
                })?;
                if tripped { Err(Crash.into()) } else { Ok(()) }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FaultPoint {
    BeforeEffect,
    AfterEffect,
    AfterReceipt,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Fault {
    pub step: Step,
    pub point: FaultPoint,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SimulatedVm {
    pub config: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending_config: BTreeMap<String, String>,
    pub running: bool,
    pub pid: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct World {
    pub lab_id: String,
    pub hostname: String,
    pub boot_id: String,
    pub boots: u64,
    pub pool_guid: String,
    pub pool_size_mib: u64,
    pub counter: u64,
    pub datasets: BTreeMap<String, BTreeMap<String, String>>,
    pub storages: BTreeMap<String, BTreeMap<String, String>>,
    pub vms: BTreeMap<u16, SimulatedVm>,
    pub tasks: BTreeMap<String, Value>,
    pub faults: Vec<Fault>,
    pub stall_tasks: bool,
}

impl World {
    pub fn new(environment: &str) -> Self {
        let digest = Sha256::digest(environment.as_bytes());
        let pool_guid =
            u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")).to_string();
        let mut root = BTreeMap::new();
        root.insert("type".into(), "filesystem".into());
        root.insert("guid".into(), pool_guid.clone());
        root.insert("createtxg".into(), "1".into());
        root.insert("org.pve-dev:lab".into(), environment.into());
        Self {
            lab_id: environment.into(),
            hostname: NODE.into(),
            boot_id: "simulated-boot-1".into(),
            boots: 1,
            pool_guid,
            pool_size_mib: 3072,
            counter: 1,
            datasets: BTreeMap::from([(POOL.to_string(), root)]),
            storages: BTreeMap::new(),
            vms: BTreeMap::new(),
            tasks: BTreeMap::new(),
            faults: Vec::new(),
            stall_tasks: false,
        }
    }

    pub fn new_boot(&mut self) {
        self.boots += 1;
        self.boot_id = format!("simulated-boot-{}", self.boots);
        for vm in self.vms.values_mut() {
            vm.running = false;
            vm.pid = None;
        }
        for task in self.tasks.values_mut() {
            if task["status"] == "running" {
                *task = json!({"status": "stopped", "exitstatus": "unexpected status"});
            }
        }
    }

    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    fn charge(&self, name: &str) -> u64 {
        let properties = &self.datasets[name];
        if properties.get("type").map(String::as_str) == Some("volume") {
            return properties
                .get("refreservation")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default();
        }
        let reservation: u64 = properties
            .get("reservation")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        reservation.max(self.used(name))
    }

    fn used(&self, name: &str) -> u64 {
        let prefix = format!("{name}/");
        self.datasets
            .keys()
            .filter(|child| {
                child
                    .strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.contains('/'))
            })
            .map(|child| self.charge(child))
            .sum()
    }

    fn available(&self) -> u64 {
        (self.pool_size_mib * MIB).saturating_sub(self.used(POOL))
    }

    fn zfs_get(&self, name: &str, keys: &[&str]) -> Result<BTreeMap<String, String>> {
        let properties = self
            .datasets
            .get(name)
            .ok_or_else(|| anyhow!("cannot open '{name}': dataset does not exist"))?;
        Ok(keys
            .iter()
            .map(|key| {
                let value = match *key {
                    "available" if name == POOL => self.available().to_string(),
                    "mounted" => properties
                        .get("mounted")
                        .cloned()
                        .unwrap_or_else(|| "no".into()),
                    "used" => (self.used(name)
                        + properties
                            .get("extra_used")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0))
                    .to_string(),
                    "quota" | "reservation" | "refreservation" => {
                        properties.get(*key).cloned().unwrap_or_else(|| "0".into())
                    }
                    _ => properties.get(*key).cloned().unwrap_or_else(|| "-".into()),
                };
                (key.to_string(), value)
            })
            .collect())
    }

    pub fn zfs_create(
        &mut self,
        name: &str,
        volume_mib: Option<u64>,
        properties: &[(&str, String)],
    ) -> Result<()> {
        ensure!(
            !self.datasets.contains_key(name),
            "cannot create '{name}': dataset already exists"
        );
        let (parent, _) = name
            .rsplit_once('/')
            .ok_or_else(|| anyhow!("cannot create pool root"))?;
        ensure!(
            self.datasets
                .get(parent)
                .is_some_and(|value| value["type"] == "filesystem"),
            "cannot create '{name}': parent does not exist"
        );
        let mut values: BTreeMap<String, String> = properties
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        let guid = (10_000_000_000_000 + self.next() * 7919).to_string();
        let createtxg = self.next().to_string();
        values.insert("guid".into(), guid);
        values.insert("createtxg".into(), createtxg);
        match volume_mib {
            Some(size) => {
                values.insert("type".into(), "volume".into());
                values.insert("volsize".into(), (size * MIB).to_string());
                values.insert("refreservation".into(), (size * MIB + MIB).to_string());
            }
            None => {
                values.insert("type".into(), "filesystem".into());
            }
        }
        for key in ["quota", "reservation"] {
            if let Some(value) = values.get(key).cloned() {
                values.insert(key.into(), parse_size(&value)?.to_string());
            }
        }
        self.datasets.insert(name.into(), values);
        let mut ancestor = name;
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            let quota: u64 = self.datasets[parent]
                .get("quota")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default();
            if quota > 0 && self.used(parent) > quota {
                self.datasets.remove(name);
                bail!("cannot create '{name}': out of space (quota of {parent} exceeded)");
            }
            ancestor = parent;
        }
        if self.used(POOL) > self.pool_size_mib * MIB {
            self.datasets.remove(name);
            bail!("cannot create '{name}': out of space");
        }
        Ok(())
    }

    fn storage_pool(&self, storage: &str) -> Result<String> {
        let entry = self
            .storages
            .get(storage)
            .ok_or_else(|| anyhow!("storage '{storage}' does not exist"))?;
        Ok(entry.get("pool").cloned().unwrap_or_default())
    }

    fn api_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["pools"] => Ok(json!([])),
            ["nodes", NODE, "storage", storage, "content"] => {
                let pool = self.storage_pool(storage)?;
                Ok(json!(self.datasets.keys().filter(|name| name.starts_with(&format!("{pool}/"))).collect::<Vec<_>>()))
            }
            ["cluster", "resources"] => Ok(Value::Array(
                self.vms.keys().map(|id| json!({"id": format!("qemu/{id}"), "vmid": id, "type": "qemu", "node": NODE})).collect(),
            )),
            ["storage"] => Ok(json!(self.storages.values().collect::<Vec<_>>())),
            ["storage", id] => Ok(json!(self.storages.get(*id).ok_or_else(|| anyhow!("storage '{id}' does not exist"))?)),
            ["nodes", NODE, "qemu", id, "config"] => {
                let vm = self.vms.get(&id.parse::<u16>()?).ok_or_else(|| anyhow!("Configuration file 'nodes/{NODE}/qemu-server/{id}.conf' does not exist"))?;
                let mut config = vm.config.clone();
                if !query.iter().any(|(key, value)| *key == "current" && value == "1") {
                    config.extend(vm.pending_config.clone());
                }
                Ok(json!(config))
            }
            ["nodes", NODE, "qemu", id, "status", "current"] => {
                let vm = self.vms.get(&id.parse::<u16>()?).ok_or_else(|| anyhow!("VM {id} does not exist"))?;
                let mut status = json!({"status": if vm.running { "running" } else { "stopped" }, "vmid": id.parse::<u16>()?});
                if let Some(pid) = vm.pid {
                    status["pid"] = json!(pid);
                }
                if let Some(lock) = vm.config.get("lock") {
                    status["lock"] = json!(lock);
                }
                Ok(status)
            }
            ["nodes", NODE, "tasks", upid, "status"] => self.tasks.get(*upid).cloned().ok_or_else(|| anyhow!("no such task")),
            _ => bail!("simulated API does not implement GET {path}"),
        }
    }

    fn api_set(&mut self, path: &str, arguments: &[(&str, String)]) -> Result<()> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let ["nodes", NODE, "qemu", id, "config"] = parts.as_slice() else {
            bail!("simulated API does not implement PUT {path}")
        };
        let vm_id: u16 = id.parse()?;
        let mut config = self
            .vms
            .get(&vm_id)
            .ok_or_else(|| anyhow!("VM {vm_id} does not exist"))?
            .config
            .clone();
        ensure!(!config.contains_key("lock"), "VM is locked");
        for (key, value) in arguments {
            match *key {
                "scsi0" => {
                    let (storage, volume) = value
                        .split_once(':')
                        .ok_or_else(|| anyhow!("invalid volume"))?;
                    let name = format!("{}/{volume}", self.storage_pool(storage)?);
                    let size = self
                        .datasets
                        .get(&name)
                        .and_then(|value| value.get("volsize"))
                        .ok_or_else(|| anyhow!("volume {name} does not exist"))?;
                    let size: u64 = size.parse()?;
                    config.insert("scsi0".into(), format!("{value},size={}M", size / MIB));
                }
                "ide2" => {
                    let (storage, volume) = value
                        .split_once(':')
                        .ok_or_else(|| anyhow!("invalid volume"))?;
                    let pool = self.storage_pool(storage)?;
                    if volume == "cloudinit" {
                        let name = format!("{pool}/vm-{vm_id}-cloudinit");
                        self.zfs_create(&name, Some(4), &[])?;
                        config.insert(
                            "ide2".into(),
                            format!("{storage}:vm-{vm_id}-cloudinit,media=cdrom"),
                        );
                    } else {
                        let base = volume.split(',').next().unwrap_or_default();
                        ensure!(
                            self.datasets.contains_key(&format!("{pool}/{base}")),
                            "volume does not exist"
                        );
                        config.insert("ide2".into(), value.clone());
                    }
                }
                _ => {
                    config.insert(key.to_string(), value.clone());
                }
            }
        }
        config.insert(
            "digest".into(),
            format!("{:x}", Sha256::digest(serde_json::to_vec(&config)?)),
        );
        self.vms.get_mut(&vm_id).expect("existing VM").config = config;
        Ok(())
    }

    fn api_create(&mut self, path: &str, arguments: &[(&str, String)]) -> Result<()> {
        ensure!(
            path == "/storage",
            "simulated API does not implement POST {path}"
        );
        let values: BTreeMap<String, String> = arguments
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        let storage = values
            .get("storage")
            .cloned()
            .ok_or_else(|| anyhow!("storage id required"))?;
        ensure!(
            !self.storages.contains_key(&storage),
            "storage ID '{storage}' already defined"
        );
        let pool = values.get("pool").cloned().unwrap_or_default();
        ensure!(
            self.datasets.contains_key(&pool),
            "pool {pool} does not exist"
        );
        self.storages.insert(storage, values);
        Ok(())
    }

    fn task(&mut self, kind: &str, vm_id: u16) -> String {
        let serial = self.next();
        let upid =
            format!("UPID:{NODE}:{serial:08X}:{serial:08X}:{serial:08X}:{kind}:{vm_id}:root@pam:");
        let status = if self.stall_tasks {
            json!({"status": "running"})
        } else {
            json!({"status": "stopped", "exitstatus": "OK"})
        };
        self.tasks.insert(upid.clone(), status);
        upid
    }

    fn api_task(
        &mut self,
        method: &str,
        path: &str,
        arguments: &[(&str, String)],
    ) -> Result<String> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("create", ["nodes", NODE, "qemu"]) => {
                let mut config: BTreeMap<String, String> = arguments
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.clone()))
                    .collect();
                let vm_id: u16 = config
                    .remove("vmid")
                    .ok_or_else(|| anyhow!("vmid required"))?
                    .parse()?;
                ensure!(
                    !self.vms.contains_key(&vm_id),
                    "unable to create VM {vm_id} - VM {vm_id} already exists"
                );
                let serial = self.next();
                config.insert(
                    "smbios1".into(),
                    format!("uuid=00000000-0000-4000-8000-{serial:012x}"),
                );
                config.insert(
                    "vmgenid".into(),
                    format!("11111111-0000-4000-8000-{serial:012x}"),
                );
                config.insert("meta".into(), "creation-qemu=simulated".into());
                self.vms.insert(
                    vm_id,
                    SimulatedVm {
                        config,
                        pending_config: BTreeMap::new(),
                        running: false,
                        pid: None,
                    },
                );
                Ok(self.task("qmcreate", vm_id))
            }
            ("create", ["nodes", NODE, "qemu", id, "status", action]) => {
                let vm_id: u16 = id.parse()?;
                let pid = self.next();
                let vm = self
                    .vms
                    .get_mut(&vm_id)
                    .ok_or_else(|| anyhow!("VM {vm_id} does not exist"))?;
                ensure!(!vm.config.contains_key("lock"), "VM is locked");
                match *action {
                    "start" => {
                        ensure!(!vm.running, "VM {vm_id} already running");
                        vm.running = true;
                        vm.pid = Some(pid);
                        Ok(self.task("qmstart", vm_id))
                    }
                    "stop" | "shutdown" => {
                        vm.running = false;
                        vm.pid = None;
                        Ok(self.task("qmstop", vm_id))
                    }
                    _ => bail!("unsupported VM action"),
                }
            }
            ("delete", ["nodes", NODE, "qemu", id]) => {
                let vm_id: u16 = id.parse()?;
                let vm = self
                    .vms
                    .get(&vm_id)
                    .ok_or_else(|| anyhow!("VM {vm_id} does not exist"))?
                    .clone();
                ensure!(
                    vm.config.get("protection").map(String::as_str) != Some("1"),
                    "can't remove VM {vm_id} - protection mode enabled"
                );
                ensure!(!vm.running, "VM {vm_id} is running - destroy failed");
                ensure!(!vm.config.contains_key("lock"), "VM is locked");
                for (key, value) in &vm.config {
                    if !is_device(key) {
                        continue;
                    }
                    if let Some((storage, volume)) = value
                        .split(',')
                        .next()
                        .and_then(|volume| volume.split_once(':'))
                        && let Ok(pool) = self.storage_pool(storage)
                    {
                        self.datasets.remove(&format!("{pool}/{volume}"));
                    }
                }
                self.vms.remove(&vm_id);
                Ok(self.task("qmdestroy", vm_id))
            }
            _ => bail!("simulated API does not implement {method} {path}"),
        }
    }
}

fn parse_size(value: &str) -> Result<u64> {
    if let Some(number) = value.strip_suffix('M') {
        return Ok(number.parse::<u64>()? * MIB);
    }
    Ok(value.parse()?)
}

pub struct SimulatedWorld {
    path: PathBuf,
    lock: Mutex<()>,
}

impl SimulatedWorld {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn create(path: &Path, environment: &str) -> Result<Self> {
        let world = Self::new(path.to_path_buf());
        ensure!(!path.exists(), "simulated world already exists");
        world.save(&World::new(environment))?;
        Ok(world)
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<World> {
        let bytes = fs::read(&self.path)
            .with_context(|| format!("reading simulated world {}", self.path.display()))?;
        serde_json::from_slice(&bytes).context("decoding simulated world")
    }

    fn save(&self, world: &World) -> Result<()> {
        let temporary = self.path.with_extension("tmp");
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(world)?)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.path)?;
        sync_parent(&self.path)
    }

    pub fn edit<T>(&self, action: impl FnOnce(&mut World) -> Result<T>) -> Result<T> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("simulated world lock poisoned"))?;
        let mut world = self.load()?;
        let result = action(&mut world)?;
        self.save(&world)?;
        Ok(result)
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn prepare_private_dir(path: &Path, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir(),
                "{} must be a real directory, not a symlink or file",
                path.display()
            );
            ensure!(
                metadata.mode() & 0o027 == 0,
                "{} must not be group-writable or accessible by others",
                path.display()
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(mode)
                .create(path)
                .with_context(|| format!("creating {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            sync_parent(path)
        }
        Err(error) => Err(error.into()),
    }
}

pub struct Host {
    database: Database,
    state_dir: PathBuf,
    binding: HostBinding,
    backend: Backend,
    task_timeout: Duration,
    poll_interval: Duration,
    notify: Notify,
    execution: tokio::sync::Mutex<()>,
}

fn response(binding: &HostBinding, job: &HostJob) -> HostResponse {
    HostResponse {
        environment: binding.environment.clone(),
        status: job.status.clone(),
        step: job.step.clone(),
        error: job.error.clone(),
        binding: Some(binding.clone()),
        resources: None,
        migration: None,
        enrollment: None,
        inspection: None,
        progress: job
            .updated_at
            .map(|updated_at| super::protocol::OperationProgress {
                started_at: job.started_at,
                updated_at,
                finished_at: job.finished_at,
                completed_steps: job
                    .steps
                    .iter()
                    .filter(|s| s.phase == Phase::Verified)
                    .count() as u32,
                total_steps: job.steps.len() as u32,
                detail: Some(job.step.clone()),
            }),
    }
}

async fn check_identity(backend: &Backend, binding: &HostBinding) -> Result<String> {
    binding.validate()?;
    let facts = backend.facts().await?;
    ensure!(
        backend.pool_guid().await? == binding.pool_guid,
        "pool GUID does not match the configured binding; refusing"
    );
    if let Some(m) = &binding.managed {
        ensure!(
            backend.scope() == Some(&Namespace::from(&m.scope)),
            "managed backend namespace differs"
        );
        let root = backend
            .zfs_get(
                &m.scope.root_dataset,
                &[
                    "type",
                    "guid",
                    "quota",
                    "reservation",
                    "org.pve-dev:managed",
                    "mountpoint",
                ],
            )
            .await?;
        m.verify_identity(
            &binding.environment,
            &facts.hostname,
            facts
                .machine_id_sha256
                .as_deref()
                .context("managed machine digest missing")?,
            &binding.pool_guid,
            &root,
        )?;
    } else if let Some(trial) = &binding.physical_trial {
        let scope = &trial.scope;
        ensure!(
            backend.scope() == Some(&Namespace::from(scope)),
            "physical backend scope differs from its immutable binding"
        );
        let root = backend
            .zfs_get(
                &scope.root_dataset,
                &[
                    "type",
                    "guid",
                    "quota",
                    "reservation",
                    "org.pve-dev:trial",
                    "mountpoint",
                ],
            )
            .await?;
        trial.verify_identity(
            &binding.environment,
            &facts.hostname,
            facts
                .machine_id_sha256
                .as_deref()
                .context("physical machine digest missing")?,
            &binding.pool_guid,
            &root,
        )?;
    } else {
        ensure!(
            backend.scope().is_none(),
            "unexpected physical trial backend scope"
        );
        ensure!(
            facts.lab_id == binding.environment,
            "{LAB_ID_FILE} does not match the configured lab environment; refusing non-lab host"
        );
        ensure!(
            facts.hostname == NODE,
            "hostname is not {NODE}; refusing non-lab host"
        );
        let marker = backend.zfs_get(POOL, &["org.pve-dev:lab"]).await?;
        ensure!(
            marker["org.pve-dev:lab"] == binding.environment,
            "{POOL} org.pve-dev:lab marker does not match the environment; refusing"
        );
    }
    ensure!(
        !facts.boot_id.is_empty() && facts.boot_id.len() <= 64,
        "invalid boot identity"
    );
    Ok(facts.boot_id)
}

impl Host {
    pub async fn open(config: HostConfig) -> Result<Self> {
        super::maintenance::ensure_not_quarantined(&config.state_dir)?;
        ensure!(
            valid_environment(&config.environment),
            "environment must be 32 lowercase hexadecimal characters"
        );
        prepare_private_dir(&config.state_dir, 0o700)?;
        let backend = match &config.backend {
            BackendConfig::Managed { binding } => {
                binding.validate(&config.environment)?;
                Backend::Lab(Box::new(LabBackend {
                    command_timeout: config.command_timeout,
                    images: binding.images.iter().map(|i| i.lab_image()).collect(),
                    scope: Some(Namespace::from(&binding.scope)),
                }))
            }
            BackendConfig::Lab { images, .. } => Backend::Lab(Box::new(LabBackend {
                command_timeout: config.command_timeout,
                images: images.clone(),
                scope: None,
            })),
            BackendConfig::PhysicalTrial { binding, images } => {
                binding.validate(&config.environment)?;
                Backend::Lab(Box::new(LabBackend {
                    command_timeout: config.command_timeout,
                    images: images.clone(),
                    scope: Some(Namespace::from(&binding.scope)),
                }))
            }
            BackendConfig::Simulated { world } => {
                Backend::Simulated(SimulatedWorld::new(world.clone()))
            }
        };
        let kind = match &config.backend {
            BackendConfig::Managed { .. } => "managed",
            BackendConfig::Lab { .. } => "lab",
            BackendConfig::Simulated { .. } => "simulated",
            BackendConfig::PhysicalTrial { .. } => "physical_trial",
        };
        let journal_path = config.state_dir.join(JOURNAL_FILE);
        let sentinel = config.state_dir.join(SENTINEL_FILE);
        if journal_path.exists() {
            ensure!(
                sentinel.exists(),
                "helper journal exists without its initialization sentinel; investigate rather than reset"
            );
            if let Backend::Simulated(world) = &backend {
                ensure!(
                    world.exists(),
                    "simulated world is missing for an initialized helper; refusing to recreate it"
                );
            }
            let database = Database::open(&journal_path)
                .context("opening helper journal (is another helper running?)")?;
            let journal = read_journal(&database)?;
            validate_scope(&journal)?;
            ensure!(
                matches!(journal.schema, 1..=7),
                "unsupported helper journal schema"
            );
            ensure!(
                journal.binding.environment == config.environment
                    && journal.binding.backend == kind
                    && journal.binding.physical_trial.as_ref() == config.backend.physical_trial()
                    && journal.binding.managed.as_ref() == config.backend.managed(),
                "helper journal is bound to a different environment or backend; refusing"
            );
            if let BackendConfig::Lab { pool_guid, .. } = &config.backend {
                ensure!(
                    &journal.binding.pool_guid == pool_guid,
                    "helper journal is bound to a different pool GUID; refusing"
                );
            }
            let host = Self {
                state_dir: config.state_dir.clone(),
                database,
                binding: journal.binding,
                backend,
                task_timeout: config.task_timeout,
                poll_interval: config.poll_interval,
                notify: Notify::new(),
                execution: tokio::sync::Mutex::new(()),
            };
            check_identity(&host.backend, &host.binding).await?;
            return Ok(host);
        }
        ensure!(
            !sentinel.exists(),
            "helper journal is missing although this state directory was initialized; refusing to reinitialize"
        );
        ensure!(
            config
                .backend
                .managed()
                .is_none_or(|m| m.adopted_vms.is_empty()),
            "existing-resource adoption requires the explicit paired-state preparation path"
        );
        if let BackendConfig::Simulated { world } = &config.backend
            && !world.exists()
        {
            SimulatedWorld::create(world, &config.environment)?;
        }
        let pool_guid = match &config.backend {
            BackendConfig::Managed { binding } => binding.pool_guid.clone(),
            BackendConfig::Lab { pool_guid, .. } => pool_guid.clone(),
            BackendConfig::Simulated { .. } => backend.pool_guid().await?,
            BackendConfig::PhysicalTrial { binding, .. } => binding.pool_guid.clone(),
        };
        let binding = HostBinding {
            environment: config.environment.clone(),
            backend: kind.into(),
            pool_guid,
            physical_trial: config.backend.physical_trial().cloned(),
            managed: config.backend.managed().cloned(),
        };
        check_identity(&backend, &binding).await?;
        managed::verify_initial_imports(&backend, &binding).await?;
        ensure!(
            !backend
                .zfs_names()
                .await?
                .contains(&backend.control_dataset()),
            "{CONTROL_DATASET} already exists but no helper journal does; refusing to adopt a prior initialization"
        );
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&journal_path)?;
        let database = Database::builder().create_file(file)?;
        let journal = Journal {
            imported_vms: binding
                .managed
                .as_ref()
                .map_or_else(Vec::new, |m| m.imported_vms.clone()),
            schema: if binding.managed.is_some() {
                7
            } else if binding
                .physical_trial
                .as_ref()
                .is_some_and(|b| !b.scope.features.is_empty())
            {
                6
            } else if binding.physical_trial.is_some() {
                4
            } else {
                1
            },
            binding: binding.clone(),
            sequence: 0,
            control: None,
            owners: BTreeMap::new(),
            machines: BTreeMap::new(),
            jobs: BTreeMap::new(),
            disk_shrinks: BTreeMap::new(),
            migrations: BTreeMap::new(),
        };
        write_journal(&database, &journal)?;
        sync_parent(&journal_path)?;
        let mut marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&sentinel)?;
        marker.write_all(format!("{}\n", binding.environment).as_bytes())?;
        marker.sync_all()?;
        sync_parent(&sentinel)?;
        Ok(Self {
            state_dir: config.state_dir.clone(),
            database,
            binding,
            backend,
            task_timeout: config.task_timeout,
            poll_interval: config.poll_interval,
            notify: Notify::new(),
            execution: tokio::sync::Mutex::new(()),
        })
    }

    pub fn environment(&self) -> &str {
        &self.binding.environment
    }

    pub fn journal(&self) -> Result<Journal> {
        let journal = read_journal(&self.database)?;
        validate_scope(&journal)?;
        Ok(journal)
    }

    fn change<T>(&self, action: impl FnOnce(&mut Journal) -> Result<T>) -> Result<T> {
        let mut transaction = self.database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        let result;
        {
            let mut table = transaction.open_table(JOURNAL)?;
            let mut journal: Journal = {
                let bytes = table
                    .get(0)?
                    .ok_or_else(|| anyhow!("helper journal missing"))?;
                serde_json::from_slice(bytes.value())?
            };
            result = action(&mut journal)?;
            let bytes = serde_json::to_vec(&journal)?;
            table.insert(0, bytes.as_slice())?;
        }
        transaction.commit()?;
        Ok(result)
    }

    pub async fn check_host(&self) -> Result<String> {
        check_identity(&self.backend, &self.binding).await
    }

    fn bound(&self, environment: &str) -> Result<()> {
        ensure!(
            environment == self.binding.environment,
            "request environment does not match this helper"
        );
        Ok(())
    }

    pub async fn handle(&self, request: HostRequest) -> Result<HostResponse> {
        match request {
            HostRequest::VmStates { binding } => {
                ensure!(
                    binding.as_ref() == &self.binding && self.binding.managed.is_some(),
                    "runtime inventory binding differs"
                );
                self.check_host().await?;
                let states = self
                    .backend
                    .api_get(&format!("/nodes/{}/qemu", self.backend.node()), &[])
                    .await?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "vm_states".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    migration: None,
                    enrollment: None,
                    inspection: Some(states),
                    progress: None,
                })
            }
            HostRequest::Inventory { binding } => {
                ensure!(
                    binding.as_ref() == &self.binding && self.binding.managed.is_some(),
                    "inventory binding differs"
                );
                self.check_host().await?;
                let Backend::Lab(lab) = &self.backend else {
                    bail!("physical inventory unavailable in simulation");
                };
                let mut inventory = Worker {
                    label: "host inventory",
                    script: include_str!("host/inventory.py"),
                    args: &[self.backend.node()],
                    input: None,
                    budget: lab.command_timeout,
                    grace: DEFAULT_GRACE,
                    max_output: 160 * 1024,
                    gid: Some(0),
                    timeout_hint: "no changes were requested",
                }
                .run_json()
                .await?;
                inventory["managed_resource_checks"] = self.inspect_adopted_resources().await?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "inventory".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    migration: None,
                    enrollment: None,
                    inspection: Some(inventory),
                    progress: None,
                })
            }
            HostRequest::ImportedVms { binding } => {
                ensure!(
                    binding.as_ref() == &self.binding && self.binding.managed.is_some(),
                    "managed helper binding required for imports"
                );
                self.check_host().await?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "read_only_imported_metadata".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    progress: None,
                    migration: None,
                    enrollment: None,
                    inspection: Some(json!(self.journal()?.imported_vms)),
                })
            }
            HostRequest::Migration {
                environment,
                request,
            } => {
                self.bound(&environment)?;
                ensure!(
                    self.binding.managed.is_none(),
                    "managed migration is disabled"
                );
                if let Some(binding) = &self.binding.physical_trial {
                    ensure!(
                        binding.scope.features.contains("migration"),
                        "physical scope does not authorize migration"
                    );
                }
                let value = self.handle_migration(*request).await?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "migration".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    progress: None,
                    migration: Some(value),
                    enrollment: None,
                    inspection: None,
                })
            }
            HostRequest::InspectOperation {
                environment,
                operation_id,
            } => {
                self.bound(&environment)?;
                let _execution = self
                    .execution
                    .try_lock()
                    .map_err(|_| anyhow!("helper is busy; retry inspection after current work"))?;
                if self.binding.managed.is_some() {
                    let journal = self.journal()?;
                    let job = journal
                        .jobs
                        .get(&operation_id)
                        .context("operation not found")?;
                    ensure!(
                        job.job.kind == JobKind::Start,
                        "managed inspection is limited to failed starts"
                    );
                    self.binding.validate_job(&job.job)?;
                }
                let value = self.inspect_operation(&operation_id).await?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "operation_inspection".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    progress: None,
                    migration: None,
                    enrollment: None,
                    inspection: Some(value),
                })
            }
            HostRequest::PlanEnrollment {
                environment,
                vm_id,
                owner,
            } => {
                self.bound(&environment)?;
                let _execution = self
                    .execution
                    .try_lock()
                    .map_err(|_| anyhow!("helper is busy; retry read-only inspection"))?;
                ensure!(
                    self.binding.managed.is_none(),
                    "managed enrollment is disabled"
                );
                let value = self.plan_enrollment(vm_id, &owner).await?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "enrollment_plan".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    progress: None,
                    migration: None,
                    enrollment: Some(json!(value)),
                    inspection: None,
                })
            }
            HostRequest::OwnedResources => {
                self.check_host().await?;
                let journal = self.journal()?;
                let unsettled_jobs = journal
                    .jobs
                    .iter()
                    .filter(|(_, job)| !job.status.is_terminal())
                    .map(|(id, job)| (id.clone(), job.status.clone()))
                    .collect();
                let mut datasets = BTreeMap::new();
                let mut storages = BTreeMap::new();
                if let Some(control) = &journal.control {
                    datasets.insert(self.backend.control_dataset(), control.clone());
                }
                for (id, owner) in &journal.owners {
                    if let Some(dataset) = owner.identities.get("dataset") {
                        datasets.insert(
                            format!("{}/{id}", self.backend.control_dataset()),
                            dataset.clone(),
                        );
                    }
                    if let Some(storage) = owner.identities.get("storage") {
                        let prefix = self
                            .backend
                            .scope()
                            .map_or("ctl-", |scope| scope.storage_prefix.as_str());
                        storages.insert(format!("{prefix}{id}"), storage.clone());
                    }
                }
                let owners = journal
                    .owners
                    .into_iter()
                    .map(|(id, owner)| {
                        (
                            id,
                            OwnedOwnerResources {
                                state: owner.state,
                                ready: owner.ready,
                                dataset: owner.identities.get("dataset").cloned(),
                                storage: owner.identities.get("storage").cloned(),
                            },
                        )
                    })
                    .collect();
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "owned_resources".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    progress: None,
                    migration: None,
                    enrollment: None,
                    inspection: None,
                    resources: Some(OwnedResources {
                        binding: self.binding.clone(),
                        datasets,
                        storages,
                        unsettled_jobs,
                        machines: journal.machines,
                        control: journal.control,
                        owners,
                    }),
                })
            }
            HostRequest::Health => {
                self.check_host().await?;
                self.journal()?;
                Ok(HostResponse {
                    environment: self.binding.environment.clone(),
                    status: OperationStatus::Verified,
                    step: "healthy".into(),
                    error: None,
                    binding: Some(self.binding.clone()),
                    resources: None,
                    progress: None,
                    migration: None,
                    enrollment: None,
                    inspection: None,
                })
            }
            HostRequest::Submit { environment, job } => {
                self.bound(&environment)?;
                ensure!(
                    self.binding.managed.is_none(),
                    "managed jobs require full helper binding"
                );
                let result = self.submit(job)?;
                self.notify.notify_one();
                Ok(result)
            }
            HostRequest::SubmitBound { binding, job } => {
                ensure!(
                    binding.as_ref() == &self.binding,
                    "helper backend/scope binding does not match the submitted request"
                );
                self.check_host().await?;
                let result = self.submit(job)?;
                self.notify.notify_one();
                Ok(result)
            }
            HostRequest::Status {
                environment,
                operation_id,
            } => {
                self.bound(&environment)?;
                let journal = self.journal()?;
                let job = journal
                    .jobs
                    .get(&operation_id)
                    .ok_or_else(|| anyhow!("unknown helper operation {operation_id}"))?;
                Ok(response(&self.binding, job))
            }
        }
    }

    pub fn submit(&self, job: Job) -> Result<HostResponse> {
        self.binding.validate_job(&job)?;
        let digest = fingerprint(&job)?;

        self.change(|journal| {
            if let Some(existing) = journal.jobs.get(&job.operation_id) {
                ensure!(
                    existing.fingerprint == digest,
                    "operation {} already exists with a different payload",
                    job.operation_id
                );
                return Ok(response(&self.binding, existing));
            }
            ensure!(
                valid_operation_id(&job.operation_id),
                "invalid operation identifier"
            );
            journal.sequence += 1;
            let (status, step, error, steps) = match self.binding.job_plan(&job) {
                Ok(plan) => (
                    OperationStatus::Queued,
                    "queued".to_string(),
                    None,
                    plan.iter()
                        .map(|step| StepRecord {
                            step: *step,
                            phase: Phase::Planned,
                            boot_id: None,
                            upid: None,
                            identity: None,
                            witness: None,
                            started_at: None,
                            finished_at: None,
                        })
                        .collect(),
                ),
                Err(error) => (
                    OperationStatus::Blocked,
                    "rejected".to_string(),
                    Some(format!("{error:#}")),
                    Vec::new(),
                ),
            };
            let record = HostJob {
                sequence: journal.sequence,
                fingerprint: digest,
                job: job.clone(),
                status,
                step,
                error,
                attempts: 0,
                steps,
                started_at: None,
                updated_at: Some(super::store::now()),
                finished_at: None,
            };
            let result = response(&self.binding, &record);
            if job.extension.is_some() {
                journal.schema = journal.schema.max(6);
            }
            if job.disk_shrink.is_some() || job.kind == JobKind::Shutdown {
                journal.schema = journal.schema.max(5);
            }
            if record.status == OperationStatus::Queued && job.kind == JobKind::ShrinkVm {
                let vm = job.vm.as_ref().context("shrink VM missing")?;
                ensure!(
                    !journal.disk_shrinks.contains_key(&vm.vm_id),
                    "VM already retains shrink recovery"
                );
                journal.disk_shrinks.insert(
                    vm.vm_id,
                    super::shrink::PendingShrink {
                        operation_id: job.operation_id.clone(),
                        vm_id: vm.vm_id,
                        owner: job.owner.user_id.clone(),
                        spec: *job
                            .disk_shrink
                            .clone()
                            .context("shrink specification missing")?,
                        prepared: false,
                    },
                );
            }
            if job.kind == JobKind::UpdateUserPolicy {
                journal.schema = journal.schema.max(2);
            }
            if job.kind == JobKind::DeleteUser {
                journal.schema = journal.schema.max(3);
            }
            journal.jobs.insert(job.operation_id.clone(), record);
            Ok(result)
        })
    }

    fn next_pending(&self) -> Result<Option<String>> {
        Ok(self
            .journal()?
            .jobs
            .values()
            .filter(|job| {
                matches!(
                    job.status,
                    OperationStatus::Queued | OperationStatus::Running
                )
            })
            .min_by_key(|job| job.sequence)
            .map(|job| job.job.operation_id.clone()))
    }

    pub async fn run_pending(&self) -> Result<()> {
        // Database transactions serialize journal changes, but effects span
        // awaits. Never let two executors act on the same accepted operation.
        let _execution = self.execution.lock().await;
        while let Some(operation_id) = self.next_pending()? {
            let boot_id = self
                .check_host()
                .await
                .context("host identity check failed; leaving operations queued")?;
            match self.run_job(&operation_id, &boot_id).await {
                Ok(()) => {}
                Err(error) if error.chain().any(|cause| cause.is::<Crash>()) => return Err(error),
                Err(error) => self.block(&operation_id, &format!("{error:#}"))?,
            }
        }
        Ok(())
    }

    pub async fn worker(&self) {
        loop {
            if let Err(error) = self.run_pending().await {
                eprintln!("pve-dev-host worker paused: {error:#}");
                if error.chain().any(|cause| cause.is::<Crash>()) {
                    return;
                }
            }
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = sleep(Duration::from_secs(30)) => {}
            }
        }
    }

    fn block(&self, operation_id: &str, error: &str) -> Result<()> {
        self.change(|journal| {
            let job = journal
                .jobs
                .get_mut(operation_id)
                .ok_or_else(|| anyhow!("operation missing"))?;
            if !matches!(
                job.status,
                OperationStatus::Verified | OperationStatus::Blocked | OperationStatus::Cancelled
            ) {
                job.status = OperationStatus::Blocked;
                job.error = Some(error.into());
                job.updated_at = Some(super::store::now());
                job.finished_at = job.updated_at;
            }
            Ok(())
        })
    }

    async fn run_job(&self, operation_id: &str, boot_id: &str) -> Result<()> {
        let record = self.change(|journal| {
            let job = journal
                .jobs
                .get_mut(operation_id)
                .ok_or_else(|| anyhow!("operation missing"))?;
            job.status = OperationStatus::Running;
            job.started_at.get_or_insert_with(super::store::now);
            job.updated_at = Some(super::store::now());
            job.attempts += 1;
            Ok(job.clone())
        })?;
        let context = Context {
            environment: self.binding.environment.clone(),
            operation_id: operation_id.into(),
            kind: record.job.kind,
            owner: record.job.owner.user_id.clone(),
            budget_mib: record.job.owner.budget_mib,
            vm: record.job.vm.clone(),
            boot_id: boot_id.into(),
            policy_revision: record.job.owner.policy_revision,
            owner_policy: record.job.owner.policy(),
            policy_change: record.job.policy_change.clone(),
            disk_shrink: record.job.disk_shrink.clone(),
            extension: record.job.extension.clone(),
            scope: self.binding.namespace(),
            managed: self.binding.managed.clone(),
        };
        self.binding.job_plan(&record.job)?;
        self.binding.validate_job(&record.job)?;
        self.admit(&context)?;
        for step in record.steps.iter().map(|record| record.step) {
            self.change(|journal| {
                let job = journal.jobs.get_mut(operation_id).expect("running job");
                job.step = step.name().into();
                job.updated_at = Some(super::store::now());
                let record = job
                    .steps
                    .iter_mut()
                    .find(|r| r.step == step)
                    .expect("planned step");
                if record.phase != Phase::Verified {
                    record.started_at.get_or_insert_with(super::store::now);
                }
                Ok(())
            })?;
            self.run_step(&context, step)
                .await
                .with_context(|| format!("{} step {}", context.kind, step.name()))?;
        }
        self.change(|journal| {
            match context.kind.as_str() {
                "ensure_user" => {
                    journal
                        .owners
                        .get_mut(&context.owner)
                        .expect("admitted owner")
                        .ready = true
                }
                "create" | "create_guest" | "enroll_vm" => {
                    journal
                        .machines
                        .get_mut(&context.vm_id())
                        .expect("preflighted machine")
                        .state = MachineState::Present
                }
                _ => {}
            }
            let job = journal.jobs.get_mut(operation_id).expect("running job");
            job.status = OperationStatus::Verified;
            job.step = "complete".into();
            job.error = None;
            job.updated_at = Some(super::store::now());
            job.finished_at = job.updated_at;
            Ok(())
        })
    }

    fn owner_policy(
        journal: &Journal,
        owner: &OwnerRecord,
    ) -> Result<super::protocol::ResourcePolicy> {
        if let Some(policy) = &owner.policy {
            return Ok(policy.clone());
        }
        ensure!(
            owner.policy_revision == 0,
            "current helper policy missing after revision update"
        );
        let initial = journal
            .jobs
            .get(&owner.operation_id)
            .ok_or_else(|| anyhow!("legacy initial owner policy proof missing"))?;
        ensure!(
            initial.job.kind == JobKind::EnsureUser
                && initial.job.owner.user_id == owner.user_id
                && initial.job.owner.policy_revision == 0
                && initial.job.owner.budget_mib == owner.budget_mib,
            "legacy initial owner policy proof mismatches"
        );
        Ok(initial.job.owner.policy())
    }

    fn admit(&self, context: &Context) -> Result<()> {
        self.change(|journal| {
            managed::admit_scope(journal, context)?;
            ensure!(!journal.migrations.values().any(|r| {
                let owner=if r.role==super::migration::Role::Source {&r.plan.source_owner}else{&r.plan.target_owner};
                !r.phase.terminal() && owner==&context.owner
            }), "owner has an active migration or retained recovery; use its explicit handoff/abort/finish workflow");
            if context.kind==JobKind::RecoverOperation {
                let Some(super::vm_change::JobExtension::Recover(spec))=context.extension.as_deref() else {bail!("recovery specification missing")};
                let original=journal.jobs.get(&spec.operation_id).context("original operation missing")?;
                let proved=journal.jobs[&context.operation_id].steps.iter().any(|s|s.step==Step::Recover && s.phase==Phase::Verified);
                ensure!((original.status==OperationStatus::Blocked || proved) && original.job.owner==journal.jobs[&context.operation_id].job.owner && original.job.vm.as_ref().map(|v|v.vm_id)==context.vm.as_ref().map(|v|v.vm_id),"recovery does not match the blocked operation or its verified decision");
                return Ok(());
            }
            if context.kind == JobKind::EnsureUser {
                ensure!(!journal.jobs.values().any(|job| job.job.owner.user_id == context.owner && job.job.kind == JobKind::DeleteUser
                    && !job.status.is_terminal()), "owner deletion is pending or blocked");
                let owner = journal
                    .owners
                    .entry(context.owner.clone())
                    .or_insert_with(|| OwnerRecord {
                        user_id: context.owner.clone(),
                        budget_mib: context.budget_mib,
                        operation_id: context.operation_id.clone(),
                        ready: false,
                        identities: BTreeMap::new(),
                        policy_revision: context.policy_revision,
                        policy: Some(context.owner_policy.clone()),
                        state: AccountState::Active,
                        delete_operation: None,
                    });
                ensure!(
                    owner.budget_mib == context.budget_mib,
                    "owner budget differs from the provisioned dataset; resizing is unsupported"
                );
                let owner = owner.clone();
                ensure!(owner.state == AccountState::Active, "owner identity has a deletion tombstone; never recreate it");
                ensure!(
                    owner.policy_revision == context.policy_revision
                        && Self::owner_policy(journal, &owner)? == context.owner_policy,
                    "ensure_user cannot alter an existing owner policy"
                );
                return Ok(());
            }
            let owner = journal
                .owners
                .get(&context.owner)
                .ok_or_else(|| anyhow!("owner storage has no helper history"))?;
            if context.kind == JobKind::DeleteUser {
                let initial = journal.jobs.get(&owner.operation_id).ok_or_else(|| anyhow!("initial account role proof missing"))?;
                ensure!(initial.job.kind == JobKind::EnsureUser && initial.job.owner.user_id == context.owner
                    && initial.job.owner.role == Role::Member, "helper refuses deletion of an account originally provisioned as administrator");
                ensure!(owner.policy_revision == context.policy_revision && owner.budget_mib == context.budget_mib
                    && Self::owner_policy(journal, owner)? == context.owner_policy, "delete_user current policy differs from frozen account");
                ensure!(owner.state == AccountState::Active || owner.delete_operation.as_deref() == Some(&context.operation_id),
                    "owner deletion belongs to another operation");
                ensure!(!journal.jobs.values().any(|job| job.job.owner.user_id == context.owner
                    && job.job.operation_id != context.operation_id && !job.status.is_terminal()),
                    "owner has unfinished helper operations");
                ensure!(!journal.machines.values().any(|machine| machine.owner == context.owner && machine.state != MachineState::Deleted),
                    "owner still has managed machines; explicit VM deletion must precede account deletion");
                return Ok(());
            }
            ensure!(owner.state == AccountState::Active, "owner account is being deleted or has a retained tombstone");
            ensure!(!journal.jobs.values().any(|j| j.job.owner.user_id == context.owner && j.job.operation_id != context.operation_id
                && j.job.extension.is_some() && !j.status.is_terminal()), "owner has an unsettled disk growth or enrollment");

            if matches!(context.kind, JobKind::ShrinkVm | JobKind::FinishVmShrink) {
                ensure!(!journal.jobs.values().any(|j| j.job.owner.user_id == context.owner
                    && j.job.operation_id != context.operation_id && !j.status.is_terminal()), "shrink owner has unfinished helper work");
                if context.kind == JobKind::FinishVmShrink {
                    if let Some(hold) = journal.disk_shrinks.get(&context.vm_id()) {
                        let mut expected = hold.spec.clone(); expected.preparation_id = Some(hold.operation_id.clone());
                        ensure!(context.disk_shrink.as_deref() == Some(&expected), "finish specification differs from retained preparation");
                    } else {
                        ensure!(journal.jobs[&context.operation_id].steps.iter().any(|r| r.step == Step::ShrinkFinish && r.phase == Phase::Verified),
                            "retained shrink hold missing without this finish's durable deletion proof");
                    }
                }
            }
            if let Some(hold) = journal.disk_shrinks.values().find(|s| s.owner == context.owner) {
                ensure!((context.kind == JobKind::ShrinkVm && hold.operation_id == context.operation_id)
                    || (context.kind == JobKind::FinishVmShrink && hold.prepared && context.disk_shrink.as_ref().and_then(|s| s.preparation_id.as_ref()) == Some(&hold.operation_id))
                    || (matches!(context.kind, JobKind::Start | JobKind::Stop | JobKind::Shutdown) && hold.prepared && hold.vm_id == context.vm_id()),
                    "owner retains disk shrink recovery; only target lifecycle and explicit finish are allowed");
            }
            ensure!(
                owner.ready,
                "owner storage is not verified ready on the host"
            );
            ensure!(
                context.kind == JobKind::UpdateUserPolicy || owner.budget_mib == context.budget_mib,
                "owner budget differs from the provisioned dataset"
            );
            ensure!(
                !journal
                    .jobs
                    .values()
                    .any(|job| job.job.owner.user_id == context.owner
                        && job.job.operation_id != context.operation_id
                        && matches!(job.job.kind, JobKind::UpdateUserPolicy | JobKind::DeleteUser)
                        && !job.status.is_terminal()),
                "owner policy is pending or blocked"
            );
            if context.kind == JobKind::UpdateUserPolicy {
                let change = context
                    .policy_change
                    .as_ref()
                    .ok_or_else(|| anyhow!("policy change missing"))?;
                ensure!(
                    !journal
                        .jobs
                        .values()
                        .any(|job| job.job.owner.user_id == context.owner
                            && job.job.operation_id != context.operation_id
                            && !job.status.is_terminal()),
                    "owner has unfinished helper work"
                );
                let committed = journal.jobs[&context.operation_id]
                    .steps
                    .iter()
                    .any(|step| step.step == Step::PolicyCommit && step.phase == Phase::Verified);
                ensure!(
                    (owner.budget_mib == change.previous.budget_mib
                        && owner.policy_revision == change.previous.policy_revision)
                        || (committed
                            && owner.budget_mib == change.target.budget_mib
                            && owner.policy_revision == change.target.policy_revision),
                    "helper current policy revision differs from frozen previous/committed target"
                );
                let expected_policy = if committed {
                    change.target.policy()
                } else {
                    change.previous.policy()
                };
                ensure!(
                    Self::owner_policy(journal, owner)? == expected_policy,
                    "helper previous resource policy differs from frozen payload"
                );
                return Ok(());
            }
            ensure!(
                Self::owner_policy(journal, owner)? == context.owner_policy,
                "job resource policy differs from helper current policy"
            );
            ensure!(
                owner.policy_revision == context.policy_revision,
                "owner policy revision differs from helper history"
            );
            let vm = context.vm()?;
            match journal.machines.get(&vm.vm_id) {
                None => ensure!(
                    matches!(context.kind, JobKind::Create | JobKind::CreateGuest | JobKind::EnrollVm),
                    "VM {} has no helper creation history; refusing",
                    vm.vm_id
                ),
                Some(machine) => {
                    ensure!(
                        machine.owner == context.owner && (machine.profile == profile(vm)
                            || (context.kind == JobKind::ShrinkVm && journal.jobs[&context.operation_id].steps.iter().any(|r| r.step == Step::ShrinkSwap && r.phase == Phase::Verified)
                                && context.disk_shrink.as_ref().is_some_and(|s| { let mut p = profile(vm); p.disk_mib = s.target_disk_mib; machine.profile == p }))
                            || (context.kind == JobKind::GrowVm && journal.jobs[&context.operation_id].steps.iter().any(|r| r.step == Step::GrowFilesystem && r.phase == Phase::Verified)
                                && matches!(context.extension.as_deref(), Some(super::vm_change::JobExtension::Grow(s)) if {let mut p=profile(vm);p.disk_mib=s.target_disk_mib;machine.profile==p}))),
                        "VM {} ownership or profile differs from helper history",
                        vm.vm_id
                    );
                    let allowed = match context.kind.as_str() {
                        "create" | "create_guest" | "enroll_vm" => {
                            machine.create_operation == context.operation_id
                                && machine.state == MachineState::Creating
                        }
                        "start" | "stop" | "shutdown" | "shrink_vm" | "finish_vm_shrink" | "grow_vm" => machine.state == MachineState::Present,
                        "delete" => {
                            machine.state == MachineState::Present
                                || (matches!(
                                    machine.state,
                                    MachineState::Deleting | MachineState::Deleted
                                ) && machine.delete_operation.as_deref()
                                    == Some(&context.operation_id))
                        }
                        _ => false,
                    };
                    ensure!(
                        allowed,
                        "VM {} is {:?} in helper history (VMIDs are never reused); refusing {}",
                        vm.vm_id,
                        machine.state,
                        context.kind
                    );
                }
            }
            Ok(())
        })
    }

    fn step_record(&self, context: &Context, step: Step) -> Result<StepRecord> {
        let journal = self.journal()?;
        journal.jobs[&context.operation_id]
            .steps
            .iter()
            .find(|record| record.step == step)
            .cloned()
            .ok_or_else(|| anyhow!("step missing from plan"))
    }

    fn update_step<T>(
        &self,
        context: &Context,
        step: Step,
        action: impl FnOnce(&mut Journal, &mut StepRecord) -> Result<T>,
    ) -> Result<T> {
        self.change(|journal| {
            let mut record = journal.jobs[&context.operation_id]
                .steps
                .iter()
                .find(|record| record.step == step)
                .cloned()
                .ok_or_else(|| anyhow!("step missing"))?;
            let previous_phase = record.phase;
            let result = action(journal, &mut record)?;
            if record.phase == Phase::Verified && previous_phase != Phase::Verified {
                record.finished_at = Some(super::store::now());
            }
            let job = journal
                .jobs
                .get_mut(&context.operation_id)
                .expect("job exists");
            job.updated_at = Some(super::store::now());
            *job.steps
                .iter_mut()
                .find(|candidate| candidate.step == step)
                .expect("step exists") = record;
            Ok(result)
        })
    }

    fn intent(&self, context: &Context, step: Step) -> Result<()> {
        self.update_step(context, step, |_, record| {
            record.phase = Phase::Intent;
            record.boot_id = Some(context.boot_id.clone());
            record.upid = None;
            Ok(())
        })
    }

    fn receipt(&self, context: &Context, step: Step, upid: &str) -> Result<()> {
        self.update_step(context, step, |_, record| {
            record.upid = Some(upid.into());
            Ok(())
        })
    }

    fn verified(
        &self,
        context: &Context,
        step: Step,
        identity: Value,
        witness: Option<Value>,
    ) -> Result<()> {
        self.update_step(context, step, |journal, record| {
            remember(journal, context, step, &identity)?;
            if step == Step::Verify && context.kind == JobKind::Delete {
                let machine = journal
                    .machines
                    .get_mut(&context.vm_id())
                    .expect("admitted machine");
                machine.state = MachineState::Deleting;
                machine.delete_operation = Some(context.operation_id.clone());
            }
            if step == Step::Release {
                journal
                    .machines
                    .get_mut(&context.vm_id())
                    .expect("admitted machine")
                    .state = MachineState::Deleted;
            }
            if step == Step::Preflight {
                let vm = context.vm()?;
                if let Some(existing) = journal.machines.get(&vm.vm_id) {
                    ensure!(
                        existing.create_operation == context.operation_id,
                        "VMID {} already has helper history",
                        vm.vm_id
                    );
                } else {
                    journal.machines.insert(
                        vm.vm_id,
                        MachineRecord {
                            vm_id: vm.vm_id,
                            owner: context.owner.clone(),
                            profile: profile(vm),
                            create_operation: context.operation_id.clone(),
                            delete_operation: None,
                            state: MachineState::Creating,
                            identities: BTreeMap::new(),
                            pinned_config: None,
                        },
                    );
                }
            }
            record.phase = Phase::Verified;
            record.boot_id = Some(context.boot_id.clone());
            record.identity = Some(identity);
            if witness.is_some() {
                record.witness = witness;
            }
            Ok(())
        })
    }

    async fn run_step(&self, context: &Context, step: Step) -> Result<()> {
        if step == Step::Recover {
            let record = self.step_record(context, step)?;
            return self.run_recovery_step(context, step, &record).await;
        }
        if step == Step::Enroll {
            let record = self.step_record(context, step)?;
            return self.run_enroll_step(context, step, &record).await;
        }
        if matches!(
            step,
            Step::GrowPrepare
                | Step::GrowVolume
                | Step::GrowPartition
                | Step::GrowFilesystem
                | Step::GrowUnlock
        ) {
            let record = self.step_record(context, step)?;
            return self.run_grow_step(context, step, &record).await;
        }

        if self.binding.physical_trial.is_some() || self.binding.managed.is_some() {
            self.check_host().await?;
        }
        let record = self.step_record(context, step)?;
        if matches!(
            step,
            Step::ShrinkStage | Step::ShrinkSwap | Step::ShrinkUnlock | Step::ShrinkFinish
        ) {
            return self.run_shrink_step(context, step, &record).await;
        }
        if matches!(
            step,
            Step::OwnerPreflight | Step::OwnerStorage | Step::OwnerDataset | Step::OwnerRelease
        ) {
            return self.run_owner_deletion_step(context, step, &record).await;
        }
        if matches!(
            step,
            Step::PolicyPreflight | Step::PolicyFirst | Step::PolicySecond | Step::PolicyCommit
        ) {
            return self.run_policy_step(context, step, &record).await;
        }
        match step {
            Step::Preflight => return self.preflight(context, &record).await,
            Step::Verify => {
                if record.phase != Phase::Verified {
                    self.verify_machine(context).await?;
                    self.verified(context, step, json!({"verified": true}), None)?;
                }
                return Ok(());
            }
            Step::Release => {
                if record.phase != Phase::Verified {
                    ensure!(
                        self.observe(context, Step::Destroy).await?.is_some(),
                        "VM or managed volumes reappeared before capacity release; investigate"
                    );
                    self.cleanup_migration_metadata(context).await?;
                    self.verified(context, step, json!({"released": true}), None)?;
                }
                return Ok(());
            }
            _ => {}
        }
        let mut reexecute = false;
        match record.phase {
            Phase::Verified => {
                if step.persistent() {
                    let current = self.observe(context, step).await?;
                    ensure!(
                        current == record.identity,
                        "verified {} disappeared or drifted; refusing to recreate or overwrite it",
                        step.name()
                    );
                }
                return Ok(());
            }
            Phase::Planned => {
                self.verify_effect_target(context, step).await?;
                if let Some(identity) = self.observe(context, step).await? {
                    ensure!(
                        self.preexisting(context, step, &identity)?,
                        "{} already exists without durable helper provenance; refusing adoption",
                        step.name()
                    );
                    return self.verified(context, step, identity, None);
                }
                self.prepare(context, step).await?;
                self.intent(context, step)?;
            }
            Phase::Intent => {
                if let Some(upid) = &record.upid {
                    self.wait(upid).await?;
                    self.verify_effect_target(context, step).await?;
                    let identity = self.observe(context, step).await?.ok_or_else(|| {
                        anyhow!(
                            "{} postcondition missing after task {upid} completed",
                            step.name()
                        )
                    })?;
                    return self.verified(context, step, identity, None);
                }
                self.verify_effect_target(context, step).await?;
                if let Some(identity) = self.observe(context, step).await? {
                    return self.verified(context, step, identity, None);
                }
                ensure!(
                    record.boot_id.as_deref() != Some(context.boot_id.as_str()),
                    "{} intent was recorded during this host boot without an authoritative task receipt and its effect is absent; blocked pending investigation",
                    step.name()
                );
                reexecute = true;
                self.prepare(context, step).await?;
                self.intent(context, step)?;
            }
        }
        self.backend.trip(step, FaultPoint::BeforeEffect)?;
        self.verify_effect_target(context, step).await?;
        let upid = self.effect(context, step, reexecute).await?;
        self.backend.trip(step, FaultPoint::AfterEffect)?;
        if let Some(upid) = &upid {
            self.receipt(context, step, upid)?;
            self.backend.trip(step, FaultPoint::AfterReceipt)?;
            self.wait(upid).await?;
        }
        self.verify_effect_target(context, step).await?;
        let identity = self
            .observe(context, step)
            .await?
            .ok_or_else(|| anyhow!("{} postcondition missing after its effect", step.name()))?;
        self.verified(context, step, identity, None)
    }

    fn preexisting(&self, context: &Context, step: Step, identity: &Value) -> Result<bool> {
        if step.runtime() {
            return Ok(true);
        }
        let journal = self.journal()?;
        Ok(match step {
            Step::Control => journal.control.as_ref() == Some(identity),
            Step::Dataset | Step::Storage => {
                journal
                    .owners
                    .get(&context.owner)
                    .and_then(|owner| owner.identities.get(step.name()))
                    == Some(identity)
            }
            _ => false,
        })
    }

    async fn prepare(&self, context: &Context, step: Step) -> Result<()> {
        if step == Step::Dataset {
            let available: u64 = self
                .backend
                .available(true)
                .await?
                .parse()
                .context("pool available space")?;
            let needed = (context.budget_mib + SAFETY_FLOOR_MIB) * MIB;
            ensure!(
                available >= needed,
                "pool has {available} bytes available; owner reservation plus the {SAFETY_FLOOR_MIB} MiB safety floor needs {needed}"
            );
        }
        Ok(())
    }

    async fn wait(&self, upid: &str) -> Result<()> {
        ensure!(
            valid_upid_on(upid, self.backend.node()),
            "invalid task receipt"
        );
        let deadline = Instant::now() + self.task_timeout;
        loop {
            let status = self
                .backend
                .api_get(
                    &format!("/nodes/{}/tasks/{upid}/status", self.backend.node()),
                    &[],
                )
                .await
                .with_context(|| format!("task {upid} status is unknown"))?;
            match status["status"].as_str() {
                Some("stopped") => {
                    let exit = text(&status["exitstatus"]);
                    ensure!(exit == "OK", "Proxmox task {upid} failed: {exit}");
                    return Ok(());
                }
                Some("running") => {}
                _ => bail!("Proxmox task {upid} returned an unrecognized status"),
            }
            ensure!(
                Instant::now() < deadline,
                "Proxmox task {upid} was still running at the timeout; outcome uncertain"
            );
            sleep(self.poll_interval).await;
        }
    }

    async fn zfs_identity(
        &self,
        name: &str,
        kind: &str,
        expected: &[(&str, String)],
        extra: &[&str],
    ) -> Result<Option<Value>> {
        if !self.backend.zfs_names().await?.contains(name) {
            return Ok(None);
        }
        let mut keys = vec!["type", "guid", "createtxg"];
        keys.extend(expected.iter().map(|(key, _)| *key));
        keys.extend(extra);
        let values = self.backend.zfs_get(name, &keys).await?;
        ensure!(values["type"] == kind, "{name} is not a {kind}");
        for (key, value) in expected {
            ensure!(
                &values[*key] == value,
                "{name} {key} is {:?}, expected {value:?}; refusing to adopt or overwrite drift",
                values[*key]
            );
        }
        let identity: serde_json::Map<String, Value> = keys
            .iter()
            .filter(|key| **key != "type")
            .map(|key| (key.to_string(), Value::String(values[*key].clone())))
            .collect();
        Ok(Some(Value::Object(identity)))
    }

    async fn vm_present(&self, context: &Context) -> Result<bool> {
        let resources = self
            .backend
            .api_get("/cluster/resources", &[("type", "vm".into())])
            .await?;
        let resources = resources
            .as_array()
            .ok_or_else(|| anyhow!("unexpected cluster resource listing"))?;
        let mut present = false;
        for resource in resources {
            if resource["vmid"].as_u64() == Some(u64::from(context.vm_id())) {
                ensure!(
                    resource["type"] == "qemu" && resource["node"] == context.node(),
                    "VMID {} is not a QEMU guest on {NODE}",
                    context.vm_id()
                );
                present = true;
            }
        }
        Ok(present)
    }

    async fn vm_config(
        &self,
        context: &Context,
        require_protection: bool,
    ) -> Result<Option<Value>> {
        if !self.vm_present(context).await? {
            return Ok(None);
        }
        let config = self
            .backend
            .api_get(&format!("{}/config", context.vm_path()), &[])
            .await?;
        let map = config
            .as_object()
            .ok_or_else(|| anyhow!("VM configuration is not an object"))?;
        let pinned = self
            .journal()?
            .machines
            .get(&context.vm_id())
            .and_then(|m| m.pinned_config.clone());
        if let Some(pinned) = pinned {
            ensure!(
                normalized_pinned_config(&config)? == normalized_pinned_config(&pinned)?,
                "enrolled VM configuration drifted"
            );
            ensure!(
                attachment_bytes(&text(&config["scsi0"]))? == context.vm()?.disk_mib * MIB,
                "enrolled root attachment size differs from its admitted profile"
            );
            if config.get("lock").is_some_and(|v| !v.is_null()) {
                let migration_owned = context.kind == JobKind::Migration
                    && self
                        .journal()?
                        .migrations
                        .get(&context.operation_id)
                        .is_some_and(|r| {
                            let id = if r.role == super::migration::Role::Source {
                                r.plan.source_vm_id
                            } else {
                                r.plan.target_vm_id
                            };
                            id == context.vm_id() && !r.phase.terminal()
                        });
                ensure!(
                    text(&config["lock"]) == "migrate"
                        && (matches!(context.kind, JobKind::ShrinkVm | JobKind::GrowVm)
                            || migration_owned),
                    "enrolled VM has an unowned lock"
                );
            }
            if require_protection {
                let expected = context.adopted().map_or("1".into(), |a| {
                    let value = text(&a.configuration["protection"]);
                    if value.is_empty() { "0".into() } else { value }
                });
                let actual = text(&config["protection"]);
                ensure!(
                    (if actual.is_empty() { "0" } else { &actual }) == expected,
                    "enrolled VM protection is disabled"
                );
            }
            ensure!(
                text(&config["smbios1"]).starts_with("uuid="),
                "enrolled VM lacks its SMBIOS identity"
            );
            return Ok(Some(config));
        }
        let expected_config = context.expected_config()?;
        for key in map.keys() {
            if context.vm()?.guest.is_some()
                && (key.starts_with("ci")
                    || key.starts_with("ipconfig")
                    || matches!(key.as_str(), "sshkeys" | "nameserver" | "searchdomain"))
            {
                ensure!(
                    expected_config.iter().any(|(expected, _)| *expected == key),
                    "VM {} has an unpinned cloud-init setting {key}; refusing",
                    context.vm_id()
                );
            }
            ensure!(
                !FORBIDDEN_KEYS.contains(&key.as_str())
                    || (key == "lock"
                        && config[key] == "migrate"
                        && (matches!(context.kind, JobKind::ShrinkVm | JobKind::GrowVm)
                            || (context.kind == JobKind::Migration
                                && self
                                    .journal()?
                                    .migrations
                                    .get(&context.operation_id)
                                    .is_some_and(|r| {
                                        (if r.role == super::migration::Role::Source {
                                            r.plan.source_vm_id
                                        } else {
                                            r.plan.target_vm_id
                                        }) == context.vm_id()
                                            && !r.phase.terminal()
                                    })))),
                "VM {} has forbidden or locked configuration key {key}; investigate",
                context.vm_id()
            );
            ensure!(
                !is_device(key) || context.managed_devices().contains(key),
                "VM {} has unmanaged device {key}; refusing",
                context.vm_id()
            );
        }
        for (key, expected) in expected_config {
            let mut actual = text(&config[key]);
            if key == "memory" {
                actual = actual.trim_start_matches("current=").to_string();
            }
            if key == "description" {
                actual = actual.trim().to_string();
            }
            ensure!(
                actual == expected,
                "VM {} {key} is {actual:?}, expected {expected:?}; refusing ownership or profile drift",
                context.vm_id()
            );
        }
        if require_protection {
            ensure!(
                text(&config["protection"]) == "1",
                "VM {} protection is disabled",
                context.vm_id()
            );
        }
        ensure!(
            text(&config["smbios1"]).starts_with("uuid="),
            "VM {} lacks an SMBIOS UUID identity",
            context.vm_id()
        );
        Ok(Some(config))
    }

    async fn managed_config(&self, context: &Context, require_protection: bool) -> Result<Value> {
        self.vm_config(context, require_protection)
            .await?
            .ok_or_else(|| anyhow!("managed VM {} disappeared", context.vm_id()))
    }

    fn recorded_uuid(&self, context: &Context) -> Result<String> {
        let journal = self.journal()?;
        let machine = journal
            .machines
            .get(&context.vm_id())
            .ok_or_else(|| anyhow!("machine history missing"))?;
        Ok(text(
            &machine
                .identities
                .get(Step::Vm.name())
                .ok_or_else(|| anyhow!("VM identity missing"))?["smbios1"],
        ))
    }

    async fn status(&self, context: &Context) -> Result<String> {
        let status = self
            .backend
            .api_get(&format!("{}/status/current", context.vm_path()), &[])
            .await?;
        ensure!(
            status.get("lock").is_none_or(Value::is_null),
            "VM {} has an outstanding Proxmox lock; investigate",
            context.vm_id()
        );
        Ok(text(&status["status"]))
    }

    async fn observe(&self, context: &Context, step: Step) -> Result<Option<Value>> {
        self.observe_with_protection(context, step, true).await
    }

    async fn observe_with_protection(
        &self,
        context: &Context,
        step: Step,
        require_protection: bool,
    ) -> Result<Option<Value>> {
        match step {
            Step::Control => {
                self.zfs_identity(
                    &context.control_dataset(),
                    "filesystem",
                    &[("org.pve-dev:control", context.control_marker())],
                    &[],
                )
                .await
            }
            Step::Dataset => {
                let bytes = (context.budget_mib * MIB).to_string();
                self.zfs_identity(
                    &context.dataset(),
                    "filesystem",
                    &[
                        ("org.pve-dev:owner", context.owner_marker()),
                        ("quota", bytes.clone()),
                        ("reservation", bytes),
                    ],
                    &[],
                )
                .await
            }
            Step::Storage => {
                let storages = self.backend.api_get("/storage", &[]).await?;
                let listed = storages
                    .as_array()
                    .ok_or_else(|| anyhow!("unexpected storage listing"))?
                    .iter()
                    .any(|entry| entry["storage"] == context.storage());
                if !listed {
                    return Ok(None);
                }
                let entry = self
                    .backend
                    .api_get(&format!("/storage/{}", context.storage()), &[])
                    .await?;
                let sparse = match text(&entry["sparse"]) {
                    value if value.is_empty() => "0".to_string(),
                    value => value,
                };
                ensure!(
                    text(&entry["type"]) == "zfspool"
                        && text(&entry["pool"]) == context.dataset()
                        && text(&entry["content"]) == "images"
                        && sparse == "0",
                    "storage {} does not target exactly {} as thick image storage; refusing",
                    context.storage(),
                    context.dataset()
                );
                let nodes = text(&entry["nodes"]);
                ensure!(
                    nodes.is_empty() || nodes == context.node(),
                    "storage {} node restriction drifted",
                    context.storage()
                );
                ensure!(
                    text(&entry["disable"]).is_empty() || text(&entry["disable"]) == "0",
                    "storage {} is disabled",
                    context.storage()
                );
                Ok(Some(
                    json!({"storage": context.storage(), "type": "zfspool", "pool": context.dataset(), "content": "images", "sparse": sparse}),
                ))
            }
            Step::Vm => {
                let Some(config) = self.vm_config(context, require_protection).await? else {
                    return Ok(None);
                };
                if self
                    .journal()?
                    .machines
                    .get(&context.vm_id())
                    .is_some_and(|m| m.pinned_config.is_some())
                {
                    return Ok(Some(
                        json!({"smbios1":text(&config["smbios1"]),"vmgenid":text(&config["vmgenid"]),"configuration":normalized_pinned_config(&config)?}),
                    ));
                }
                let mut identity = serde_json::Map::new();
                for key in ["smbios1", "vmgenid"] {
                    identity.insert(key.into(), Value::String(text(&config[key])));
                }
                for (key, expected) in context.expected_config()? {
                    identity.insert(key.into(), Value::String(expected));
                }
                Ok(Some(Value::Object(identity)))
            }
            Step::Volume => {
                let size = context.vm()?.disk_mib * MIB;
                let identity = self
                    .zfs_identity(
                        &context.root_volume(),
                        "volume",
                        &[
                            ("org.pve-dev:owner", context.vm_marker()),
                            ("volsize", size.to_string()),
                        ],
                        &[],
                    )
                    .await?;
                if identity.is_some() {
                    let reservation: u64 = self
                        .backend
                        .zfs_get(&context.root_volume(), &["refreservation"])
                        .await?["refreservation"]
                        .parse()
                        .unwrap_or_default();
                    ensure!(
                        reservation >= size,
                        "{} is not thick-provisioned",
                        context.root_volume()
                    );
                }
                Ok(identity)
            }
            Step::Attach => {
                let config = self.managed_config(context, require_protection).await?;
                let Some(attachment) = config.get("scsi0").map(text) else {
                    return Ok(None);
                };
                let reference = format!("{}:{}", context.storage(), context.root_name());
                ensure!(
                    attachment.split(',').next() == Some(reference.as_str()),
                    "scsi0 is attached to an unexpected volume; refusing"
                );
                ensure!(text(&config["boot"]) == "order=scsi0", "boot order drifted");
                Ok(Some(json!({"scsi0": attachment, "boot": "order=scsi0"})))
            }
            Step::Image => {
                let vm = context.vm()?;
                let guest = vm
                    .guest
                    .as_ref()
                    .ok_or_else(|| anyhow!("guest specification missing"))?;
                let properties = self
                    .backend
                    .zfs_get(
                        &context.root_volume(),
                        &["org.pve-dev:image", "guid", "createtxg"],
                    )
                    .await?;
                let marker = &properties["org.pve-dev:image"];
                if marker == "-" {
                    return Ok(None);
                }
                ensure!(
                    marker == &guest.image_sha256,
                    "root disk image provenance drifted"
                );
                Ok(Some(
                    json!({"image_sha256": marker, "guid": properties["guid"], "createtxg": properties["createtxg"]}),
                ))
            }
            Step::CloudInit => {
                let config = self.managed_config(context, require_protection).await?;
                let Some(attachment) = config.get("ide2").map(text) else {
                    return Ok(None);
                };
                let expected = format!(
                    "{}:{},media=cdrom",
                    context.storage(),
                    context.cloud_init_name()
                );
                ensure!(
                    attachment == expected
                        || (attachment.strip_suffix(",size=4M") == Some(expected.as_str())),
                    "ide2 attachment {attachment:?} is not the managed cloud-init drive"
                );
                let volume = self
                    .zfs_identity(&context.cloud_init_volume(), "volume", &[], &["volsize"])
                    .await?
                    .ok_or_else(|| anyhow!("attached cloud-init volume is missing"))?;
                Ok(Some(
                    json!({"ide2": expected, "guid": volume["guid"], "createtxg": volume["createtxg"], "volsize": volume["volsize"]}),
                ))
            }
            Step::Start | Step::Stop | Step::Shutdown => {
                let state = self.status(context).await?;
                ensure!(
                    state == "running" || state == "stopped",
                    "VM {} has unexpected status {state:?}",
                    context.vm_id()
                );
                let wanted = if step == Step::Start {
                    "running"
                } else {
                    "stopped"
                };
                Ok((state == wanted).then(|| json!({"status": state})))
            }
            Step::Unprotect => {
                let config = self.managed_config(context, false).await?;
                ensure!(
                    text(&config["smbios1"]) == self.recorded_uuid(context)?,
                    "VM {} identity changed; refusing",
                    context.vm_id()
                );
                Ok((text(&config["protection"]) != "1").then(|| json!({"protection": "0"})))
            }
            Step::Destroy => {
                if let Some(config) = self.vm_config(context, false).await? {
                    ensure!(
                        text(&config["smbios1"]) == self.recorded_uuid(context)?,
                        "VM {} identity changed; refusing",
                        context.vm_id()
                    );
                    return Ok(None);
                }
                let names = self.backend.zfs_names().await?;
                ensure!(
                    !names.contains(&context.root_volume())
                        && !names.contains(&context.cloud_init_volume()),
                    "VM {} is absent but its managed volumes remain; investigate",
                    context.vm_id()
                );
                Ok(Some(json!({"absent": true})))
            }
            Step::Recover
            | Step::Enroll
            | Step::GrowPrepare
            | Step::GrowVolume
            | Step::GrowPartition
            | Step::GrowFilesystem
            | Step::GrowUnlock
            | Step::ShrinkStage
            | Step::ShrinkSwap
            | Step::ShrinkUnlock
            | Step::ShrinkFinish
            | Step::Preflight
            | Step::Verify
            | Step::Release
            | Step::PolicyPreflight
            | Step::PolicyFirst
            | Step::PolicySecond
            | Step::PolicyCommit
            | Step::OwnerPreflight
            | Step::OwnerStorage
            | Step::OwnerDataset
            | Step::OwnerRelease => {
                bail!("journal-only step has no observation")
            }
        }
    }

    async fn effect(
        &self,
        context: &Context,
        step: Step,
        reexecute: bool,
    ) -> Result<Option<String>> {
        match step {
            Step::Control => {
                self.backend
                    .zfs_create(
                        &context.control_dataset(),
                        None,
                        &[
                            ("org.pve-dev:control", context.control_marker()),
                            ("mountpoint", "none".into()),
                        ],
                    )
                    .await?;
                Ok(None)
            }
            Step::Dataset => {
                let bytes = (context.budget_mib * MIB).to_string();
                self.backend
                    .zfs_create(
                        &context.dataset(),
                        None,
                        &[
                            ("org.pve-dev:owner", context.owner_marker()),
                            ("quota", bytes.clone()),
                            ("reservation", bytes),
                            ("mountpoint", "none".into()),
                        ],
                    )
                    .await?;
                Ok(None)
            }
            Step::Storage => {
                self.backend
                    .api_create(
                        "/storage",
                        &[
                            ("storage", context.storage()),
                            ("type", "zfspool".into()),
                            ("pool", context.dataset()),
                            ("content", "images".into()),
                            ("sparse", "0".into()),
                            ("nodes", context.node().into()),
                        ],
                    )
                    .await?;
                Ok(None)
            }
            Step::Vm => {
                let mut arguments = vec![("vmid", context.vm_id().to_string())];
                arguments.extend(context.expected_config()?);
                arguments.push(("protection", "1".into()));
                Ok(Some(
                    self.backend
                        .api_task(
                            "create",
                            &format!("/nodes/{}/qemu", context.node()),
                            &arguments,
                        )
                        .await?,
                ))
            }
            Step::Volume => {
                self.backend
                    .zfs_create(
                        &context.root_volume(),
                        Some(context.vm()?.disk_mib),
                        &[("org.pve-dev:owner", context.vm_marker())],
                    )
                    .await?;
                Ok(None)
            }
            Step::Image => {
                ensure!(
                    self.status(context).await? == "stopped",
                    "image import requires a stopped VM"
                );
                let config = self.managed_config(context, true).await?;
                ensure!(
                    !config.as_object().unwrap().contains_key("scsi0")
                        && !config.as_object().unwrap().contains_key("ide2"),
                    "image import requires an unattached, never-booted root disk"
                );
                let journal = self.journal()?;
                let recorded = journal.machines[&context.vm_id()]
                    .identities
                    .get(Step::Volume.name())
                    .ok_or_else(|| anyhow!("root disk has no durable helper provenance"))?;
                ensure!(
                    self.observe(context, Step::Volume).await?.as_ref() == Some(recorded),
                    "root disk identity changed before image import"
                );
                self.backend.import_image(context).await?;
                Ok(None)
            }
            Step::Attach => {
                self.backend
                    .api_set(
                        &format!("{}/config", context.vm_path()),
                        &[
                            (
                                "scsi0",
                                format!("{}:{}", context.storage(), context.root_name()),
                            ),
                            ("boot", "order=scsi0".into()),
                        ],
                    )
                    .await?;
                Ok(None)
            }
            Step::CloudInit => {
                let exists = self
                    .backend
                    .zfs_names()
                    .await?
                    .contains(&context.cloud_init_volume());
                let attachment = if exists {
                    ensure!(
                        reexecute,
                        "cloud-init volume appeared without helper provenance; refusing adoption"
                    );
                    format!(
                        "{}:{},media=cdrom",
                        context.storage(),
                        context.cloud_init_name()
                    )
                } else {
                    format!("{}:cloudinit", context.storage())
                };
                self.backend
                    .api_set(
                        &format!("{}/config", context.vm_path()),
                        &[("ide2", attachment)],
                    )
                    .await?;
                Ok(None)
            }
            Step::Start => Ok(Some(
                self.backend
                    .api_task(
                        "create",
                        &format!("{}/status/start", context.vm_path()),
                        &[],
                    )
                    .await?,
            )),
            Step::Stop => Ok(Some(
                self.backend
                    .api_task("create", &format!("{}/status/stop", context.vm_path()), &[])
                    .await?,
            )),
            Step::Shutdown => Ok(Some(
                self.backend
                    .api_task(
                        "create",
                        &format!("{}/status/shutdown", context.vm_path()),
                        &[("timeout", "600".into())],
                    )
                    .await?,
            )),
            Step::Unprotect => {
                self.backend
                    .api_set(
                        &format!("{}/config", context.vm_path()),
                        &[("protection", "0".into())],
                    )
                    .await?;
                Ok(None)
            }
            Step::Destroy => Ok(Some(
                self.backend
                    .api_task("delete", &context.vm_path(), &context.delete_arguments())
                    .await?,
            )),
            Step::Recover
            | Step::Enroll
            | Step::GrowPrepare
            | Step::GrowVolume
            | Step::GrowPartition
            | Step::GrowFilesystem
            | Step::GrowUnlock
            | Step::ShrinkStage
            | Step::ShrinkSwap
            | Step::ShrinkUnlock
            | Step::ShrinkFinish
            | Step::Preflight
            | Step::Verify
            | Step::Release
            | Step::PolicyPreflight
            | Step::PolicyFirst
            | Step::PolicySecond
            | Step::PolicyCommit
            | Step::OwnerPreflight
            | Step::OwnerStorage
            | Step::OwnerDataset
            | Step::OwnerRelease => {
                bail!("journal-only step has no effect")
            }
        }
    }

    async fn verify_owner(&self, context: &Context) -> Result<()> {
        if context.adopted().is_some() {
            return self.verify_adopted_allocation(context).await;
        }
        if context.kind == JobKind::UpdateUserPolicy {
            return self.verify_policy_owner(context).await;
        }
        let journal = self.journal()?;
        let owner = journal
            .owners
            .get(&context.owner)
            .filter(|owner| owner.ready)
            .ok_or_else(|| anyhow!("owner storage is not verified ready"))?;
        ensure!(
            journal.control.is_some()
                && self.observe(context, Step::Control).await? == journal.control,
            "control dataset disappeared or drifted"
        );
        for step in [Step::Dataset, Step::Storage] {
            ensure!(
                self.observe(context, step).await?.as_ref() == owner.identities.get(step.name()),
                "owner {} disappeared or drifted; refusing",
                step.name()
            );
        }
        Ok(())
    }

    async fn verify_machine(&self, context: &Context) -> Result<()> {
        self.verify_machine_with_protection(context, true).await
    }

    async fn verify_machine_with_protection(
        &self,
        context: &Context,
        require_protection: bool,
    ) -> Result<()> {
        self.verify_owner(context).await?;
        if context.adopted().is_some() {
            self.verify_adopted_volumes(context).await?;
            self.managed_config(context, require_protection).await?;
            if context.kind == JobKind::Delete {
                self.verify_adopted_deletion(context).await?;
            }
            return Ok(());
        }
        let journal = self.journal()?;
        let machine = journal
            .machines
            .get(&context.vm_id())
            .ok_or_else(|| anyhow!("machine history missing"))?;
        let mut steps = vec![Step::Vm, Step::Volume, Step::Attach, Step::CloudInit];
        if context.vm()?.guest.is_some() {
            steps.push(Step::Image);
        }
        for step in steps {
            let recorded = machine
                .identities
                .get(step.name())
                .ok_or_else(|| anyhow!("managed {} identity was never recorded", step.name()))?;
            ensure!(
                self.observe_with_protection(context, step, require_protection)
                    .await?
                    .as_ref()
                    == Some(recorded),
                "managed {} disappeared or drifted; refusing",
                step.name()
            );
        }
        let config = self.managed_config(context, require_protection).await?;
        let expected = machine
            .pinned_config
            .as_ref()
            .map_or_else(|| context.managed_devices(), devices);
        ensure!(
            devices(&config) == expected,
            "VM {} device set differs from the managed set",
            context.vm_id()
        );
        Ok(())
    }

    // A previously verified step is historical evidence, not authority to act
    // on whatever now occupies a VMID. Recheck every identity at reconciliation
    // and effect boundaries, including deletion after protection was removed.
    async fn verify_effect_target(&self, context: &Context, step: Step) -> Result<()> {
        if self.binding.physical_trial.is_some() || self.binding.managed.is_some() {
            self.check_host().await?;
        }
        match step {
            Step::Start | Step::Stop | Step::Shutdown => self.verify_machine(context).await,
            Step::Unprotect | Step::Destroy => {
                self.verify_owner(context).await?;
                if step == Step::Destroy && !self.vm_present(context).await? {
                    // observe(Destroy) still requires both managed volumes to
                    // be absent; only an existing durable intent can accept it.
                    return Ok(());
                }
                if context.adopted().is_some() {
                    self.verify_adopted_deletion(context).await?;
                }
                self.verify_machine_with_protection(context, false).await?;
                ensure!(
                    self.status(context).await? == "stopped",
                    "VM restarted during deletion; refusing to unprotect or destroy it"
                );
                if step == Step::Destroy {
                    let config = self.managed_config(context, false).await?;
                    ensure!(
                        matches!(text(&config["protection"]).as_str(), "0" | ""),
                        "VM protection changed before destruction; refusing"
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn verify_empty_owner_state(
        &self,
        context: &Context,
        inspect: bool,
    ) -> Result<(bool, bool)> {
        let journal = self.journal()?;
        let owner = journal
            .owners
            .get(&context.owner)
            .ok_or_else(|| anyhow!("owner history missing"))?;
        ensure!(
            journal.control.is_some()
                && self.observe(context, Step::Control).await? == journal.control,
            "shared control dataset drifted; refusing owner cleanup"
        );
        ensure!(
            !journal
                .machines
                .values()
                .any(|machine| machine.owner == context.owner
                    && machine.state != MachineState::Deleted),
            "owner still has a managed VM; account deletion never cascades"
        );
        let job = &journal.jobs[&context.operation_id];
        let phase = |step| {
            job.steps
                .iter()
                .find(|record| record.step == step)
                .map(|record| record.phase)
                .ok_or_else(|| anyhow!("owner cleanup plan missing"))
        };
        let names = self.backend.zfs_names().await?;
        let dataset_present = names.contains(&context.dataset());
        let prefix = format!("{}/", context.dataset());
        ensure!(
            !names.iter().any(|name| name.starts_with(&prefix)),
            "owner has remaining descendants; explicit cleanup required"
        );
        match phase(Step::OwnerDataset)? {
            Phase::Planned => ensure!(
                dataset_present,
                "owner dataset disappeared before its deletion intent"
            ),
            Phase::Verified => ensure!(
                !dataset_present,
                "owner dataset reappeared after verified deletion"
            ),
            Phase::Intent => {}
        }
        if dataset_present {
            ensure!(
                self.observe(context, Step::Dataset).await?.as_ref()
                    == owner.identities.get("dataset"),
                "owner dataset identity/current policy drifted"
            );
            ensure!(
                self.backend
                    .owner_snapshots(&context.dataset())
                    .await?
                    .is_empty(),
                "owner snapshots/bookmarks remain; remove them explicitly before account cleanup"
            );
            let properties = self
                .backend
                .zfs_get(&context.dataset(), &["mountpoint", "mounted"])
                .await?;
            ensure!(
                properties["mountpoint"] == "none" && properties["mounted"] == "no",
                "owner dataset is mounted or has an unexpected mountpoint; preserve it"
            );
        }
        let storage = self.observe(context, Step::Storage).await?;
        let storage_present = storage.is_some();
        match phase(Step::OwnerStorage)? {
            Phase::Planned => ensure!(
                storage.as_ref() == owner.identities.get("storage"),
                "owner storage disappeared or drifted before its deletion intent"
            ),
            Phase::Verified => ensure!(
                !storage_present,
                "owner storage reappeared after verified deletion"
            ),
            Phase::Intent => {
                if storage_present {
                    ensure!(
                        storage.as_ref() == owner.identities.get("storage"),
                        "owner storage binding drifted"
                    );
                }
            }
        }
        if !dataset_present {
            ensure!(
                !storage_present,
                "storage still exists while owner dataset is absent"
            );
        }
        // Proxmox permits multiple storage IDs for the same ZFS pool path.
        // Never strand an unrelated alias when deleting our empty scaffold,
        // including recovery after our own storage entry has been removed.
        let storages = self.backend.api_get("/storage", &[]).await?;
        for storage in storages
            .as_array()
            .ok_or_else(|| anyhow!("storage inventory missing"))?
        {
            let kind = storage["type"]
                .as_str()
                .ok_or_else(|| anyhow!("storage type missing"))?;
            if kind != "zfspool" {
                continue;
            }
            let id = storage["storage"]
                .as_str()
                .ok_or_else(|| anyhow!("storage ID missing"))?;
            if id == context.storage() {
                continue;
            }
            ensure!(
                !id.is_empty()
                    && id.len() <= 128
                    && !id.contains("..")
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "unsupported storage ID; inspect before owner cleanup"
            );
            let config = self.backend.api_get(&format!("/storage/{id}"), &[]).await?;
            ensure!(
                config["type"] == "zfspool",
                "storage type changed during alias inspection"
            );
            let pool = config["pool"]
                .as_str()
                .ok_or_else(|| anyhow!("ZFS storage pool binding missing"))?;
            ensure!(
                pool != context.dataset() && !pool.starts_with(&prefix),
                "another storage ID aliases this owner dataset or a descendant; preserve it"
            );
        }
        let resources = self
            .backend
            .api_get("/cluster/resources", &[("type", "vm".into())])
            .await?;
        let resources = resources
            .as_array()
            .ok_or_else(|| anyhow!("VM inventory missing"))?;
        for resource in resources {
            let vm_id = resource["vmid"]
                .as_u64()
                .ok_or_else(|| anyhow!("invalid VM identity in inventory"))?;
            ensure!(
                resource["node"] == context.node(),
                "nonlocal VM inventory is unsupported for owner cleanup"
            );
            ensure!(
                !journal
                    .machines
                    .values()
                    .any(|machine| machine.owner == context.owner
                        && u64::from(machine.vm_id) == vm_id),
                "a historical owner VMID reappeared; investigate before account cleanup"
            );
            let kind = text(&resource["type"]);
            ensure!(
                matches!(kind.as_str(), "qemu" | "lxc"),
                "unsupported guest type during empty-owner verification"
            );
            for current in ["1", "0"] {
                let config = self
                    .backend
                    .api_get(
                        &format!("/nodes/{}/{kind}/{vm_id}/config", context.node()),
                        &[("current", current.into())],
                    )
                    .await?;
                let config = config
                    .as_object()
                    .ok_or_else(|| anyhow!("guest configuration missing"))?;
                let reference = format!("{}:", context.storage());
                let marker = format!("pve-dev-control:{}:{}:", context.environment, context.owner);
                ensure!(
                    !config.values().any(|value| value
                        .as_str()
                        .is_some_and(|value| value.contains(&reference)))
                        && !config
                            .get("description")
                            .and_then(Value::as_str)
                            .is_some_and(|description| description.contains(&marker)),
                    "guest configuration still references this owner allocation"
                );
            }
        }
        let pools = self.backend.api_get("/pools", &[]).await?;
        for pool in pools
            .as_array()
            .ok_or_else(|| anyhow!("resource pool inventory missing"))?
        {
            let id = pool["poolid"]
                .as_str()
                .ok_or_else(|| anyhow!("resource pool ID missing"))?;
            ensure!(
                valid_label(id, 64),
                "unsupported resource pool identity; inspect before owner cleanup"
            );
            let config = self.backend.api_get(&format!("/pools/{id}"), &[]).await?;
            let members = config["members"]
                .as_array()
                .ok_or_else(|| anyhow!("pool members missing"))?;
            ensure!(
                !members
                    .iter()
                    .any(|member| member["storage"] == context.storage()
                        || member["id"] == format!("storage/{}", context.storage())),
                "an unmanaged resource pool still references owner storage; detach it explicitly"
            );
        }
        if storage_present {
            let contents = self
                .backend
                .api_get(
                    &format!(
                        "/nodes/{}/storage/{}/content",
                        context.node(),
                        context.storage()
                    ),
                    &[],
                )
                .await?;
            ensure!(
                contents.as_array().is_some_and(Vec::is_empty),
                "owner storage content is not verified empty"
            );
        }
        if inspect && dataset_present {
            self.backend
                .inspect_empty_owner(
                    &context.dataset(),
                    &self
                        .state_dir
                        .join(format!("empty-owner-{}", context.operation_id)),
                )
                .await?;
            ensure!(
                self.backend
                    .zfs_get(&context.dataset(), &["mounted"])
                    .await?["mounted"]
                    == "no",
                "owner inspection did not leave dataset unmounted"
            );
        }
        Ok((storage_present, dataset_present))
    }

    async fn run_owner_deletion_step(
        &self,
        context: &Context,
        step: Step,
        record: &StepRecord,
    ) -> Result<()> {
        let state = self.verify_empty_owner_state(context, false).await?;
        if step == Step::OwnerPreflight {
            if record.phase != Phase::Verified {
                if record.phase == Phase::Planned {
                    self.intent(context, step)?;
                }
                self.backend.trip(step, FaultPoint::BeforeEffect)?;
                self.verify_empty_owner_state(context, true).await?;
                self.backend.trip(step, FaultPoint::AfterEffect)?;
                self.update_step(context, step, |journal, record| {
                    let owner = journal
                        .owners
                        .get_mut(&context.owner)
                        .ok_or_else(|| anyhow!("owner history missing"))?;
                    owner.state = AccountState::Deleting;
                    owner.delete_operation = Some(context.operation_id.clone());
                    record.phase = Phase::Verified;
                    record.boot_id = Some(context.boot_id.clone());
                    record.identity = Some(json!({"empty":true}));
                    Ok(())
                })?;
            }
            return Ok(());
        }
        if step == Step::OwnerRelease {
            ensure!(
                !state.0 && !state.1,
                "owner scaffolding is not absent; retain admission claims"
            );
            if record.phase != Phase::Verified {
                self.backend.trip(step, FaultPoint::BeforeEffect)?;
                self.update_step(context, step, |journal, record| {
                    let owner = journal
                        .owners
                        .get_mut(&context.owner)
                        .ok_or_else(|| anyhow!("owner history missing"))?;
                    owner.state = AccountState::Deleted;
                    owner.ready = false;
                    record.phase = Phase::Verified;
                    record.boot_id = Some(context.boot_id.clone());
                    record.identity = Some(json!({"absent":true}));
                    Ok(())
                })?;
                self.backend.trip(step, FaultPoint::AfterEffect)?;
            }
            return Ok(());
        }
        let present = if step == Step::OwnerStorage {
            state.0
        } else {
            state.1
        };
        if record.phase == Phase::Verified {
            return Ok(());
        }
        if record.phase == Phase::Intent && !present {
            return self.verified(context, step, json!({"absent":true}), None);
        }
        if record.phase == Phase::Intent {
            ensure!(
                record.boot_id.as_deref() != Some(context.boot_id.as_str()),
                "same-boot owner removal intent without completion proof is ambiguous; retain claims"
            );
        } else {
            self.intent(context, step)?;
        }
        self.backend.trip(step, FaultPoint::BeforeEffect)?;
        self.verify_empty_owner_state(context, true).await?;
        if step == Step::OwnerStorage {
            self.backend
                .remove_owner_storage(&context.storage())
                .await?;
        } else {
            self.backend
                .remove_empty_owner_dataset(&context.dataset())
                .await?;
        }
        self.backend.trip(step, FaultPoint::AfterEffect)?;
        let after = self.verify_empty_owner_state(context, false).await?;
        ensure!(
            if step == Step::OwnerStorage {
                !after.0
            } else {
                !after.1
            },
            "owner removal postcondition missing"
        );
        self.verified(context, step, json!({"absent":true}), None)
    }

    async fn verify_policy_owner(&self, context: &Context) -> Result<()> {
        let journal = self.journal()?;
        let owner = journal
            .owners
            .get(&context.owner)
            .filter(|owner| owner.ready)
            .ok_or_else(|| anyhow!("owner storage is not ready"))?;
        ensure!(
            journal.control.is_some()
                && self.observe(context, Step::Control).await? == journal.control,
            "control identity drifted during policy change"
        );
        ensure!(
            self.observe(context, Step::Storage).await?.as_ref() == owner.identities.get("storage"),
            "owner storage binding drifted during policy change"
        );
        let actual = self
            .zfs_identity(
                &context.dataset(),
                "filesystem",
                &[("org.pve-dev:owner", context.owner_marker())],
                &[],
            )
            .await?
            .ok_or_else(|| anyhow!("owner dataset missing"))?;
        let mut recorded = owner
            .identities
            .get("dataset")
            .cloned()
            .ok_or_else(|| anyhow!("owner dataset provenance missing"))?;
        recorded
            .as_object_mut()
            .ok_or_else(|| anyhow!("invalid dataset identity"))?
            .remove("quota");
        recorded.as_object_mut().unwrap().remove("reservation");
        ensure!(
            actual == recorded,
            "owner dataset GUID/creation identity changed; refusing policy update"
        );
        Ok(())
    }

    fn policy_property(context: &Context, step: Step) -> Result<&'static str> {
        let change = context
            .policy_change
            .as_ref()
            .ok_or_else(|| anyhow!("policy change missing"))?;
        let increase = change.target.budget_mib >= change.previous.budget_mib;
        Ok(match (step, increase) {
            (Step::PolicyFirst, true) | (Step::PolicySecond, false) => "quota",
            (Step::PolicySecond, true) | (Step::PolicyFirst, false) => "reservation",
            _ => bail!("not a policy property step"),
        })
    }

    async fn verify_policy_state(&self, context: &Context) -> Result<BTreeMap<String, String>> {
        self.verify_policy_owner(context).await?;
        let change = context
            .policy_change
            .as_ref()
            .ok_or_else(|| anyhow!("policy change missing"))?;
        let journal = self.journal()?;
        let job = &journal.jobs[&context.operation_id];
        let properties = self
            .backend
            .zfs_get(&context.dataset(), &["quota", "reservation", "used"])
            .await?;
        for step in [Step::PolicyFirst, Step::PolicySecond] {
            let property = Self::policy_property(context, step)?;
            let phase = job
                .steps
                .iter()
                .find(|record| record.step == step)
                .ok_or_else(|| anyhow!("policy step absent"))?
                .phase;
            let value = properties[property]
                .parse::<u64>()
                .context("invalid ZFS policy property")?;
            let old = change.previous.budget_mib * MIB;
            let target = change.target.budget_mib * MIB;
            ensure!(
                match phase {
                    Phase::Planned => value == old,
                    Phase::Intent => value == old || value == target,
                    Phase::Verified => value == target,
                },
                "unexpected {property} drift during frozen policy plan"
            );
        }
        let target_bytes = change.target.budget_mib * MIB;
        let used = properties["used"]
            .parse::<u64>()
            .context("invalid ZFS used accounting")?;
        ensure!(
            used <= target_bytes,
            "target disk budget below actual ZFS used bytes (including descendants, snapshots and thick reservations)"
        );
        let reservation = properties["reservation"].parse::<u64>()?;
        let available = self
            .backend
            .available(change.target.budget_mib > change.previous.budget_mib)
            .await?
            .parse::<u64>()?;
        if change.target.budget_mib > change.previous.budget_mib {
            ensure!(
                available >= target_bytes.saturating_sub(reservation) + SAFETY_FLOOR_MIB * MIB,
                "pool lacks target reservation plus required safety floor"
            );
        }
        let mut expected_names = BTreeSet::new();
        let mut disk = 0_u64;
        let mut unbacked_margin = 0_u64;
        let mut min_memory = 0_u64;
        let mut max_memory = 0_u64;
        let mut cores = 0_u64;
        let mut count = 0_u64;
        for machine in journal.machines.values().filter(|machine| {
            machine.owner == context.owner && machine.state != MachineState::Deleted
        }) {
            ensure!(
                machine.state == MachineState::Present,
                "owner has unsettled helper machine"
            );
            let mut vm_context = context.clone();
            vm_context.vm = Some(Vm {
                vm_id: machine.vm_id,
                owner: machine.owner.clone(),
                name: machine.profile.name.clone(),
                disk_mib: machine.profile.disk_mib,
                memory_mib: machine.profile.memory_mib,
                min_memory_mib: machine.profile.min_memory_mib,
                cores: machine.profile.cores,
                state: super::protocol::VmState::Stopped,
                guest: machine.profile.guest.clone(),
            });
            self.verify_machine(&vm_context).await?;
            expected_names.insert(vm_context.root_volume());
            expected_names.insert(vm_context.cloud_init_volume());
            let mut vm_backing = 0_u64;
            for volume in [vm_context.root_volume(), vm_context.cloud_init_volume()] {
                let backing = self
                    .backend
                    .zfs_get(&volume, &["refreservation", "volsize"])
                    .await?;
                let reservation = backing["refreservation"]
                    .parse::<u64>()
                    .context("invalid thick backing reservation")?;
                ensure!(
                    reservation >= backing["volsize"].parse::<u64>()?,
                    "managed volume is not fully reserved"
                );
                vm_backing = vm_backing
                    .checked_add(reservation)
                    .ok_or_else(|| anyhow!("backing reservation accounting overflow"))?;
            }
            let vm_charge = super::store::disk_charge(vm_context.vm()?);
            unbacked_margin += (vm_charge * MIB).saturating_sub(vm_backing);
            disk += vm_charge;
            min_memory += machine.profile.min_memory_mib;
            max_memory += machine.profile.memory_mib;
            cores += u64::from(machine.profile.cores);
            count += 1;
        }
        let prefix = format!("{}/", context.dataset());
        let actual_names: BTreeSet<_> = self
            .backend
            .zfs_names()
            .await?
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .collect();
        ensure!(
            actual_names == expected_names,
            "unmanaged or missing descendants in owner allocation; refusing policy change"
        );
        // `used` includes child reservations plus snapshot/metadata growth.
        // Add the controller allowance not already backed by those reservations
        // instead of merely taking max(used, disk_charge), which loses headroom.
        // An unchanged disk budget is not a new storage admission: memory/CPU
        // edits must not freeze a valid owner merely to impose extra disk margin.
        // Actual used, thick backing and logical disk commitments remain checked.
        let additional_margin = if change.target.budget_mib == change.previous.budget_mib {
            0
        } else {
            unbacked_margin
        };
        let conservative_used = used
            .checked_add(additional_margin)
            .ok_or_else(|| anyhow!("policy usage accounting overflow"))?;
        ensure!(
            target_bytes >= conservative_used,
            "target below actual ZFS usage plus committed metadata allowance"
        );
        let target = &change.target;
        ensure!(
            target.budget_mib >= disk
                && target.min_memory_budget_mib >= min_memory
                && target.max_memory_budget_mib >= max_memory
                && u64::from(target.max_cores) >= cores
                && u64::from(target.max_vms) >= count,
            "target below helper's actual committed VM profiles"
        );
        Ok(properties)
    }

    async fn run_policy_step(
        &self,
        context: &Context,
        step: Step,
        record: &StepRecord,
    ) -> Result<()> {
        if self.binding.physical_trial.is_some() || self.binding.managed.is_some() {
            self.check_host().await?;
        }
        // Even a remembered preflight is historical; every resumed boundary
        // rechecks exact provenance, usage, topology and allowed old/target values.
        let properties = self.verify_policy_state(context).await?;
        let change = context
            .policy_change
            .as_ref()
            .ok_or_else(|| anyhow!("policy change missing"))?;
        if step == Step::PolicyPreflight {
            if record.phase != Phase::Verified {
                self.verified(
                    context,
                    step,
                    json!({"checked":true}),
                    Some(json!({"dataset":context.dataset(),"properties":properties})),
                )?;
            }
            return Ok(());
        }
        if step == Step::PolicyCommit {
            if record.phase != Phase::Verified {
                let target = (change.target.budget_mib * MIB).to_string();
                ensure!(
                    properties["quota"] == target && properties["reservation"] == target,
                    "target storage policy not verified"
                );
                self.backend.trip(step, FaultPoint::BeforeEffect)?;
                self.update_step(context, step, |journal, record| {
                    let owner = journal
                        .owners
                        .get_mut(&context.owner)
                        .ok_or_else(|| anyhow!("owner missing"))?;
                    owner.budget_mib = change.target.budget_mib;
                    owner.policy_revision = change.target.policy_revision;
                    owner.policy = Some(change.target.policy());
                    let identity = owner
                        .identities
                        .get_mut("dataset")
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| anyhow!("dataset identity missing"))?;
                    identity.insert("quota".into(), json!(target));
                    identity.insert("reservation".into(), json!(target));
                    record.phase = Phase::Verified;
                    record.boot_id = Some(context.boot_id.clone());
                    record.identity =
                        Some(json!({"policy_revision":change.target.policy_revision}));
                    Ok(())
                })?;
                self.backend.trip(step, FaultPoint::AfterEffect)?;
            }
            return Ok(());
        }
        let property = Self::policy_property(context, step)?;
        let target = change.target.budget_mib * MIB;
        let current = properties[property].parse::<u64>()?;
        match record.phase {
            Phase::Verified => return Ok(()),
            Phase::Planned => {
                self.intent(context, step)?;
            }
            Phase::Intent if current != target => ensure!(
                record.boot_id.as_deref() != Some(context.boot_id.as_str()),
                "same-boot policy intent without a verified effect is ambiguous; blocked pending investigation"
            ),
            Phase::Intent => {}
        }
        if current != target {
            self.backend.trip(step, FaultPoint::BeforeEffect)?;
            self.verify_policy_state(context).await?;
            self.backend
                .set_policy_property(&context.dataset(), property, target)
                .await?;
            self.backend.trip(step, FaultPoint::AfterEffect)?;
        }
        let actual = self.verify_policy_state(context).await?;
        ensure!(
            actual[property] == target.to_string(),
            "policy property effect not verified"
        );
        self.verified(context, step, json!({property:target.to_string()}), None)
    }

    async fn preflight(&self, context: &Context, record: &StepRecord) -> Result<()> {
        if record.phase == Phase::Verified {
            return Ok(());
        }
        self.verify_owner(context).await?;
        if context.vm()?.guest.is_some() {
            self.backend.check_image(context.vm()?).await?;
        }
        ensure!(
            !self.vm_present(context).await?,
            "VMID {} already exists on the host without helper provenance; refusing adoption",
            context.vm_id()
        );
        let prefix = format!("vm-{}-", context.vm_id());
        let collisions: Vec<String> = self
            .backend
            .zfs_names()
            .await?
            .into_iter()
            .filter(|name| {
                name.rsplit('/')
                    .next()
                    .is_some_and(|base| base.starts_with(&prefix))
            })
            .collect();
        ensure!(
            collisions.is_empty(),
            "volumes for VMID {} already exist ({}); refusing",
            context.vm_id(),
            collisions.join(", ")
        );
        self.verified(
            context,
            Step::Preflight,
            json!({"vm_absent": true}),
            Some(json!({"vm_absent": true, "volumes_absent": true, "boot_id": context.boot_id})),
        )
    }
}

fn remember(journal: &mut Journal, context: &Context, step: Step, identity: &Value) -> Result<()> {
    let slot = match step {
        Step::Control => {
            match &journal.control {
                Some(existing) => ensure!(existing == identity, "control dataset identity changed"),
                None => journal.control = Some(identity.clone()),
            }
            return Ok(());
        }
        Step::Dataset | Step::Storage => {
            &mut journal
                .owners
                .get_mut(&context.owner)
                .ok_or_else(|| anyhow!("owner missing"))?
                .identities
        }
        Step::Vm | Step::Volume | Step::Image | Step::Attach | Step::CloudInit => {
            &mut journal
                .machines
                .get_mut(&context.vm_id())
                .ok_or_else(|| anyhow!("machine missing"))?
                .identities
        }
        _ => return Ok(()),
    };
    match slot.get(step.name()) {
        Some(existing) => ensure!(existing == identity, "{} identity changed", step.name()),
        None => {
            slot.insert(step.name().into(), identity.clone());
        }
    }
    Ok(())
}

fn read_journal(database: &Database) -> Result<Journal> {
    let transaction = database.begin_read()?;
    let table = transaction.open_table(JOURNAL)?;
    let bytes = table
        .get(0)?
        .ok_or_else(|| anyhow!("helper journal missing; refusing initialization"))?;
    Ok(serde_json::from_slice(bytes.value())?)
}

fn write_journal(database: &Database, journal: &Journal) -> Result<()> {
    let mut transaction = database.begin_write()?;
    transaction.set_durability(Durability::Immediate)?;
    {
        let mut table = transaction.open_table(JOURNAL)?;
        let bytes = serde_json::to_vec(journal)?;
        table.insert(0, bytes.as_slice())?;
    }
    transaction.commit()?;
    Ok(())
}

fn envelope(result: Result<HostResponse>) -> Envelope {
    match result {
        Ok(response) => Envelope {
            ok: true,
            response: Some(response),
            error: None,
        },
        Err(error) => Envelope {
            ok: false,
            response: None,
            error: Some(format!("{error:#}")),
        },
    }
}

async fn connection(host: Arc<Host>, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buffer = Vec::new();
    let read = timeout(
        EXCHANGE_TIMEOUT,
        reader
            .take(MAX_HOST_MESSAGE as u64 + 1)
            .read_to_end(&mut buffer),
    )
    .await;
    let result = match read {
        Err(_) => Err(anyhow!("request timed out")),
        Ok(Err(error)) => Err(error.into()),
        Ok(Ok(_)) if buffer.len() > MAX_HOST_MESSAGE => Err(anyhow!("request too large")),
        Ok(Ok(_)) => match serde_json::from_slice::<HostRequest>(&buffer) {
            Ok(request) => host.handle(request).await,
            Err(error) => Err(anyhow!("invalid request: {error}")),
        },
    };
    let bytes = serde_json::to_vec(&envelope(result))?;
    timeout(EXCHANGE_TIMEOUT, async {
        writer.write_all(&bytes).await?;
        writer.shutdown().await
    })
    .await??;
    Ok(())
}

pub fn bind(socket: &Path) -> Result<UnixListener> {
    let runtime = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("socket path needs a runtime directory"))?;
    prepare_private_dir(runtime, 0o750)?;
    match fs::symlink_metadata(socket) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket(),
                "refusing to replace non-socket {}",
                socket.display()
            );
            fs::remove_file(socket)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660))?;
    let owner = fs::symlink_metadata(socket)?.uid();
    if fs::metadata(runtime)?.uid() != owner {
        let _ = fs::remove_file(socket);
        bail!(
            "runtime directory {} is not owned by the helper user",
            runtime.display()
        );
    }
    Ok(listener)
}

pub async fn serve(host: Arc<Host>, socket: &Path) -> Result<()> {
    let listener = bind(socket)?;
    let worker = host.clone();
    let accept = async {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => return anyhow::Error::from(error),
            };
            let host = host.clone();
            tokio::spawn(async move {
                if let Err(error) = connection(host, stream).await {
                    eprintln!("pve-dev-host connection error: {error:#}");
                }
            });
        }
    };
    tokio::select! {
        error = accept => Err(error.context("accepting helper connections")),
        _ = worker.worker() => bail!("helper worker stopped"),
    }
}

fn helper_exchange_timeout(request: &HostRequest) -> Duration {
    use super::migration::HostMigrationRequest;
    match request {
        HostRequest::Migration { request, .. }
            if matches!(
                request.as_ref(),
                HostMigrationRequest::Prepare { .. }
                    | HostMigrationRequest::Fence { .. }
                    | HostMigrationRequest::Seal { .. }
                    | HostMigrationRequest::Cleanup { .. }
                    | HostMigrationRequest::Abort { .. }
            ) =>
        {
            Duration::from_secs(72 * 3600)
        }
        _ => EXCHANGE_TIMEOUT,
    }
}

pub async fn rpc(socket: &Path, request: &HostRequest) -> Result<HostResponse> {
    let body = serde_json::to_vec(request)?;
    ensure!(body.len() <= MAX_HOST_MESSAGE, "helper request too large");
    let mut stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| anyhow!("timed out connecting to the host helper"))?
        .with_context(|| format!("connecting to host helper {}", socket.display()))?;
    let mut buffer = Vec::new();
    timeout(helper_exchange_timeout(request), async {
        stream.write_all(&body).await?;
        stream.shutdown().await?;
        (&mut stream)
            .take(MAX_HOST_MESSAGE as u64 + 1)
            .read_to_end(&mut buffer)
            .await
    })
    .await
    .map_err(|_| anyhow!("host helper exchange timed out"))??;
    ensure!(
        buffer.len() <= MAX_HOST_MESSAGE,
        "host helper response too large"
    );
    let envelope: Envelope =
        serde_json::from_slice(&buffer).context("invalid host helper response")?;
    if !envelope.ok {
        bail!(
            "host helper error: {}",
            envelope.error.unwrap_or_else(|| "unspecified".into())
        );
    }
    let response = envelope
        .response
        .ok_or_else(|| anyhow!("host helper response missing"))?;
    match request {
        HostRequest::Submit { environment, .. }
        | HostRequest::Status { environment, .. }
        | HostRequest::Migration { environment, .. }
        | HostRequest::PlanEnrollment { environment, .. }
        | HostRequest::InspectOperation { environment, .. } => {
            ensure!(
                &response.environment == environment,
                "host helper answered for a different environment"
            )
        }
        HostRequest::SubmitBound { binding, .. }
        | HostRequest::ImportedVms { binding }
        | HostRequest::Inventory { binding }
        | HostRequest::VmStates { binding } => ensure!(
            response.binding.as_ref() == Some(binding.as_ref())
                && response.environment == binding.environment,
            "helper answered for a different backend/scope binding"
        ),
        HostRequest::Health | HostRequest::OwnedResources => {}
    }
    Ok(response)
}

#[cfg(test)]
mod physical_scope_tests {
    use super::*;

    #[test]
    fn reserved_parent_is_not_double_charged_to_global_pool_and_reductions_keep_working() {
        assert_eq!(
            allocation_space(1024 * MIB, Some(8192 * MIB), true).unwrap(),
            8192 * MIB
        );
        assert!(allocation_space(256 * MIB, Some(8192 * MIB), true).is_err());
        assert_eq!(
            allocation_space(256 * MIB, Some(8192 * MIB), false).unwrap(),
            8192 * MIB
        );
        assert_eq!(
            allocation_space(1024 * MIB, None, true).unwrap(),
            1024 * MIB
        );
    }

    fn scope() -> super::super::physical_trial::TrialScope {
        super::super::physical_trial::TrialScope {
            node: "pve2".into(),
            pool: "rpool".into(),
            root_dataset: "rpool/pve-dev-trial-0123456789abcdef0123456789abcdef".into(),
            root_guid: "222".into(),
            machine_id_sha256: "a".repeat(64),
            storage_prefix: "trial-01234567-".into(),
            owners: BTreeSet::from(["trial-admin".into(), "trial-user".into()]),
            vm_ids: BTreeSet::from([9051]),
            capacity_mib: 12288,
            features: Default::default(),
            limits: None,
        }
    }

    fn context(scope: Option<super::super::physical_trial::TrialScope>) -> Context {
        Context {
            environment: "0123456789abcdef0123456789abcdef".into(),
            operation_id: "test".into(),
            kind: JobKind::Create,
            owner: "trial-user".into(),
            budget_mib: 128,
            vm: Some(
                serde_json::from_value(json!({"vm_id":9051,"owner":"trial-user","name":"trial",
                "disk_mib":32,"memory_mib":128,"min_memory_mib":128,"cores":1,"state":"creating"}))
                .unwrap(),
            ),
            boot_id: "test-boot".into(),
            policy_revision: 0,
            owner_policy: super::super::protocol::ResourcePolicy {
                budget_mib: 128,
                min_memory_budget_mib: 256,
                max_memory_budget_mib: 1024,
                max_cores: 8,
                max_vms: 4,
            },
            policy_change: None,
            disk_shrink: None,
            extension: None,
            scope: scope.as_ref().map(Namespace::from),
            managed: None,
        }
    }

    #[test]
    fn physical_paths_and_task_receipts_use_the_exact_pinned_scope() {
        let scope = scope();
        let context = context(Some(scope.clone()));
        assert_eq!(context.vm_path(), "/nodes/pve2/qemu/9051");
        assert_eq!(
            context.dataset(),
            format!("{}/control/trial-user", scope.root_dataset)
        );
        assert_eq!(context.storage(), "trial-01234567-trial-user");
        assert_eq!(
            context.delete_arguments(),
            [
                ("purge", "0".into()),
                ("destroy-unreferenced-disks", "0".into())
            ]
        );
        assert_eq!(
            context.root_volume(),
            format!("{}/control/trial-user/vm-9051-disk-0", scope.root_dataset)
        );
        let backend = Backend::Lab(Box::new(LabBackend {
            command_timeout: Duration::from_secs(1),
            images: vec![],
            scope: Some(Namespace::from(&scope)),
        }));
        assert_eq!(backend.node(), "pve2");
        assert_eq!(backend.pool(), "rpool");
        assert_eq!(backend.control_dataset(), context.control_dataset());
        let receipt = "\"UPID:pve2:00000001:00000001:00000001:qmcreate:9051:root@pam:\"";
        assert!(parse_task_on(receipt, "pve2").is_ok());
        assert!(parse_task_on(receipt, NODE).is_err());
    }

    #[test]
    fn absent_scope_keeps_original_lab_paths_and_task_parser() {
        let context = context(None);
        assert_eq!(context.vm_path(), "/nodes/pve-lab/qemu/9051");
        assert_eq!(context.dataset(), "labpool/control/trial-user");
        assert_eq!(context.storage(), "ctl-trial-user");
        assert_eq!(
            context.delete_arguments(),
            [
                ("purge", "1".into()),
                ("destroy-unreferenced-disks", "0".into())
            ]
        );
        let receipt = "\"UPID:pve-lab:00000001:00000001:00000001:qmcreate:9051:root@pam:\"";
        assert!(parse_task(receipt).is_ok());
    }

    #[test]
    fn disk_maintenance_timeout_scales_from_admitted_size_and_is_bounded() {
        assert_eq!(disk_maintenance_timeout(8192).as_secs(), 3600);
        assert_eq!(disk_maintenance_timeout(750 * 1024).as_secs(), 97800);
        assert_eq!(disk_maintenance_timeout(0).as_secs(), 3600);
        assert_eq!(disk_maintenance_timeout(u64::MAX).as_secs(), 72 * 3600);
    }
}
