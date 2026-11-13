use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"pve-dev/control/1";
pub const MAX_REQUEST: usize = 65536;
pub const MAX_RESPONSE: usize = 2 * 1024 * 1024;

pub fn valid_image_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Member,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    #[default]
    Active,
    Deleting,
    Deleted,
}
impl AccountState {
    pub fn is_active(&self) -> bool {
        *self == Self::Active
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    pub user_id: String,
    pub role: Role,
    pub budget_mib: u64,
    pub min_memory_budget_mib: u64,
    pub max_memory_budget_mib: u64,
    pub max_cores: u16,
    pub max_vms: u16,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub policy_revision: u64,
    #[serde(default, skip_serializing_if = "AccountState::is_active")]
    pub state: AccountState,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourcePolicy {
    pub budget_mib: u64,
    pub min_memory_budget_mib: u64,
    pub max_memory_budget_mib: u64,
    pub max_cores: u16,
    pub max_vms: u16,
}

/// Explicit root-configured ceilings. Legacy lab environments retain their
/// original small limits; a broader physical test scope must persist its caps.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub disk_mib: u64,
    pub budget_mib: u64,
    pub min_memory_mib: u64,
    pub memory_mib: u64,
    pub cores: u16,
}
impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            disk_mib: 8192,
            budget_mib: 8192,
            min_memory_mib: 1024,
            memory_mib: 2048,
            cores: 16,
        }
    }
}
impl ResourceLimits {
    pub fn valid(&self) -> bool {
        (16..=super::vm_change::MAX_DISK_MIB).contains(&self.disk_mib)
            && self.disk_mib <= self.budget_mib
            && self.budget_mib <= 2 * super::vm_change::MAX_DISK_MIB
            && (128..=1024 * 1024).contains(&self.memory_mib)
            && (128..=self.memory_mib).contains(&self.min_memory_mib)
            && (1..=256).contains(&self.cores)
    }
}

impl ResourcePolicy {
    pub fn valid(&self) -> bool {
        self.valid_with_limits(ResourceLimits::default())
    }
    pub fn valid_with_limits(&self, limits: ResourceLimits) -> bool {
        limits.valid()
            && (64..=limits.budget_mib).contains(&self.budget_mib)
            && (128..=limits.min_memory_mib).contains(&self.min_memory_budget_mib)
            && self.max_memory_budget_mib >= self.min_memory_budget_mib
            && self.max_memory_budget_mib <= limits.memory_mib
            && (1..=limits.cores).contains(&self.max_cores)
            && (1..=8).contains(&self.max_vms)
    }
}

impl User {
    pub fn policy(&self) -> ResourcePolicy {
        ResourcePolicy {
            budget_mib: self.budget_mib,
            min_memory_budget_mib: self.min_memory_budget_mib,
            max_memory_budget_mib: self.max_memory_budget_mib,
            max_cores: self.max_cores,
            max_vms: self.max_vms,
        }
    }
    pub fn with_policy(&self, policy: &ResourcePolicy) -> Option<Self> {
        let mut next = self.clone();
        next.budget_mib = policy.budget_mib;
        next.min_memory_budget_mib = policy.min_memory_budget_mib;
        next.max_memory_budget_mib = policy.max_memory_budget_mib;
        next.max_cores = policy.max_cores;
        next.max_vms = policy.max_vms;
        next.policy_revision = self.policy_revision.checked_add(1)?;
        Some(next)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Creating,
    Stopped,
    Running,
    Deleting,
    Deleted,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Guest {
    pub image_sha256: String,
    pub ssh_public_key: String,
}

impl Guest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_image_digest(&self.image_sha256),
            "image_sha256 must be 64 lowercase hexadecimal characters"
        );
        anyhow::ensure!(
            crate::ssh_key::key_identity(&self.ssh_public_key)? == self.ssh_public_key,
            "guest SSH key must be a canonical Ed25519 public key without a comment"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Vm {
    pub vm_id: u16,
    pub owner: String,
    pub name: String,
    pub disk_mib: u64,
    pub memory_mib: u64,
    pub min_memory_mib: u64,
    pub cores: u16,
    pub state: VmState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<Box<Guest>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmAction {
    Start,
    Shutdown,
    Stop,
    Delete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    HostInventory {},
    InspectOperation {
        operation_id: String,
    },
    RecoverOperation {
        request_id: String,
        operation_id: String,
        inspection_id: String,
        kind: super::vm_change::RecoveryKind,
        confirm_recovery: bool,
    },
    PlanVmGrowth {
        vm_id: u16,
        target_disk_mib: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_partition_uuid: Option<String>,
    },
    GrowVm {
        request_id: String,
        vm_id: u16,
        expected_disk_mib: u64,
        target_disk_mib: u64,
        confirm_downtime: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_partition_uuid: Option<String>,
    },
    PlanVmEnrollment {
        vm_id: u16,
        owner: String,
    },
    EnrollVm {
        request_id: String,
        plan_id: String,
        vm_id: u16,
        owner: String,
        confirm_enrollment: bool,
    },
    Migration {
        request: Box<super::migration::AdminRequest>,
    },
    PlanVmShrink {
        vm_id: u16,
        target_disk_mib: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_partition_uuid: Option<String>,
    },
    ShrinkVm {
        request_id: String,
        vm_id: u16,
        expected_disk_mib: u64,
        target_disk_mib: u64,
        confirm_downtime: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_partition_uuid: Option<String>,
    },
    FinishVmShrink {
        request_id: String,
        vm_id: u16,
        preparation_id: String,
        confirm_guest_verified: bool,
        confirm_delete_recovery: bool,
    },
    Dashboard {},
    Whoami {},
    Budget {},
    ListImages {},
    ListImageCatalog {},
    ListVms {},
    ListOperations {},
    GetOperation {
        operation_id: String,
    },
    CreateUser {
        request_id: String,
        user_id: String,
        budget_mib: u64,
        min_memory_budget_mib: u64,
        max_memory_budget_mib: u64,
        max_cores: u16,
        max_vms: u16,
    },
    UpdateUserPolicy {
        request_id: String,
        user_id: String,
        expected_revision: u64,
        policy: ResourcePolicy,
    },
    DeleteUser {
        request_id: String,
        user_id: String,
        expected_revision: u64,
    },
    EnrollDevice {
        user_id: String,
        endpoint_id: String,
        label: String,
    },
    RevokeDevice {
        endpoint_id: String,
    },
    CreateVm {
        request_id: String,
        name: String,
        disk_mib: u64,
        memory_mib: u64,
        min_memory_mib: u64,
        cores: u16,
        owner: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        guest: Option<Guest>,
    },
    VmAction {
        request_id: String,
        vm_id: u16,
        kind: VmAction,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Budget {
    pub budget_mib: u64,
    pub allocated_mib: u64,
    pub remaining_mib: u64,
    pub min_memory_allocated_mib: u64,
    pub memory_allocated_mib: u64,
    pub cores_allocated: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Queued,
    Running,
    Verified,
    Blocked,
    Cancelled,
}
impl OperationStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Verified | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Operation {
    pub operation_id: String,
    pub owner: String,
    pub request_id: String,
    pub kind: super::JobKind,
    pub vm_id: Option<u16>,
    pub status: OperationStatus,
    pub step: String,
    pub error: Option<String>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<OperationProgress>,
}

/// Durable wall-clock milestones; step counts deliberately do not estimate
/// remaining time or imply that copying and metadata steps have equal cost.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationProgress {
    pub started_at: Option<u64>,
    pub updated_at: u64,
    pub finished_at: Option<u64>,
    pub completed_steps: u32,
    pub total_steps: u32,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Dashboard {
    pub identity: User,
    pub users: Vec<User>,
    pub budget: Budget,
    pub vms: Vec<Vm>,
    pub operations: Vec<Operation>,
    pub environment: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disk_shrinks: Vec<super::shrink::PendingShrink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imported_vms: Vec<ImportedVm>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage_allocations: Vec<super::adoption::StorageAllocation>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl Response {
    pub fn from_result(result: Result<serde_json::Value, ApiError>) -> Self {
        match result {
            Ok(result) => Self {
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => Self {
                ok: false,
                result: None,
                error: Some(error),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageCatalogEntry {
    pub image_sha256: String,
    pub label: String,
    pub os_version: String,
    pub default_user: String,
    pub min_disk_mib: u64,
}

/// Root-approved historical observation, never an owned VM or a create operation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportedVm {
    pub vm: Vm,
    pub label: String,
    pub observed_at: u64,
    pub configuration_sha256: String,
    pub reason: String,
}
impl ImportedVm {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.vm.vm_id > 0
                && super::store::valid_name(&self.vm.owner)
                && super::store::valid_name(&self.vm.name)
                && self.vm.guest.is_none()
                && matches!(self.vm.state, VmState::Running | VmState::Stopped),
            "invalid read-only imported VM metadata"
        );
        anyhow::ensure!(
            self.observed_at > 0
                && valid_image_digest(&self.configuration_sha256)
                && !self.label.is_empty()
                && self.label.len() <= 160
                && !self.label.chars().any(char::is_control)
                && !self.reason.is_empty()
                && self.reason.len() <= 512
                && !self.reason.chars().any(char::is_control),
            "invalid imported provenance"
        );
        anyhow::ensure!(
            self.vm.disk_mib > 0
                && self.vm.disk_mib <= u64::MAX / super::host::MIB
                && self.vm.memory_mib > 0
                && self.vm.min_memory_mib <= self.vm.memory_mib
                && self.vm.cores > 0,
            "invalid imported resource display metadata"
        );
        Ok(())
    }
}
