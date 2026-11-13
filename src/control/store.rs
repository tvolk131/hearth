use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::protocol::*;
use super::shrink::{PendingShrink, ShrinkPlan, ShrinkSpec};

const CATALOG: TableDefinition<u8, &[u8]> = TableDefinition::new("controller_catalog_v1");

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Binding {
    pub environment: String,
    pub backend: String,
    pub disk_capacity_mib: u64,
    pub min_memory_capacity_mib: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guest_images: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_trial: Option<super::physical_trial::TrialBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<super::managed::ManagedBinding>,
}

impl Binding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.managed.is_none() || self.physical_trial.is_none(),
            "managed and trial bindings cannot be combined"
        );
        if let Some(managed) = &self.managed {
            managed.validate(&self.environment)?;
            ensure!(
                self.backend == "managed"
                    && self.disk_capacity_mib == managed.scope.capacity_mib
                    && self.min_memory_capacity_mib == managed.scope.min_memory_capacity_mib
                    && self.guest_images
                        == managed
                            .images
                            .iter()
                            .map(|i| i.image_sha256.clone())
                            .collect::<Vec<_>>(),
                "controller differs from root managed policy"
            );
            return Ok(());
        }
        match (&self.physical_trial, self.backend.as_str()) {
            (Some(trial), "physical_trial") => {
                trial.validate(&self.environment)?;
                ensure!(
                    self.disk_capacity_mib == trial.scope.capacity_mib,
                    "controller capacity differs from the immutable physical trial cap"
                );
            }
            (None, "lab" | "simulated") => {}
            _ => anyhow::bail!("unsupported backend or mismatched physical trial binding"),
        }
        Ok(())
    }

    pub fn limits(&self) -> ResourceLimits {
        if let Some(m) = &self.managed {
            return m.scope.limits;
        }
        self.physical_trial
            .as_ref()
            .and_then(|b| b.scope.limits)
            .unwrap_or_default()
    }
    pub fn feature(&self, name: &str) -> bool {
        if self.managed.is_some() {
            return name == "recovery";
        }
        self.physical_trial
            .as_ref()
            .is_none_or(|b| b.scope.features.contains(name))
    }

    pub fn attested_by(&self, host: &super::host::HostBinding) -> bool {
        self.environment == host.environment
            && self.backend == host.backend
            && self.physical_trial == host.physical_trial
            && self.managed == host.managed
            && self
                .managed
                .as_ref()
                .is_none_or(|m| m.pool_guid == host.pool_guid)
            && self
                .physical_trial
                .as_ref()
                .is_none_or(|trial| trial.pool_guid == host.pool_guid)
    }
}

pub fn validate_scope(catalog: &Catalog) -> Result<()> {
    catalog.binding.validate()?;
    super::shrink::validate_catalog(catalog)?;
    super::vm_change::validate_catalog(catalog)?;
    ensure!(
        matches!(catalog.schema, 1..=7)
            && if let Some(scope) = &catalog.binding.physical_trial {
                catalog.schema
                    >= if scope.scope.features.is_empty() {
                        4
                    } else {
                        6
                    }
            } else {
                catalog.schema != 4
            },
        "catalog schema/scope binding mismatch"
    );
    if let Some(trial) = &catalog.binding.physical_trial {
        for (id, user) in &catalog.users {
            ensure!(
                id == &user.user_id && trial.scope.permits(id, None),
                "catalog owner escapes physical trial scope"
            );
            ensure!(
                (id == "trial-admin") == (user.role == Role::Admin),
                "physical trial role binding changed"
            );
        }
        for (id, vm) in &catalog.vms {
            ensure!(
                *id == vm.vm_id && trial.scope.permits(&vm.owner, Some(*id)),
                "catalog VM escapes physical trial scope"
            );
        }
        for record in catalog.operations.values() {
            ensure!(
                trial.scope.permits(
                    &record.job.owner.user_id,
                    record.job.vm.as_ref().map(|vm| vm.vm_id)
                ),
                "catalog operation escapes physical trial scope"
            );
        }
    }
    if let Some(m) = &catalog.binding.managed {
        for adopted in &m.adopted_vms {
            let vm = catalog
                .vms
                .get(&adopted.vm.vm_id)
                .context("adopted VM is missing from the catalog")?;
            let mut expected = adopted.vm.clone();
            expected.state = vm.state.clone();
            ensure!(
                *vm == expected,
                "adopted catalog profile differs from root binding"
            );
        }
        ensure!(
            catalog.schema == 7 && catalog.imported_vms == m.imported_vms,
            "managed imported records or schema differ from root pin"
        );
        for (id, u) in &catalog.users {
            ensure!(
                id == &u.user_id
                    && m.scope.permits(id, None)
                    && ((id == &m.admin_user) == (u.role == Role::Admin)),
                "managed catalog owner/role escapes scope"
            );
        }
        for (id, v) in &catalog.vms {
            ensure!(
                *id == v.vm_id
                    && m.permits_vm(&v.owner, *id)
                    && (!catalog.imported_vms.iter().any(|r| r.vm.vm_id == *id)
                        || m.adopted(*id).is_some()),
                "managed catalog VM escapes scope"
            );
        }
        for r in catalog.operations.values() {
            m.validate_retained_job(&r.job, r.public.status.is_terminal())?;
        }
        ensure!(
            catalog.disk_shrinks.is_empty()
                && catalog.disk_growths.is_empty()
                && catalog.enrollments.is_empty()
                && catalog.enrollment_plans.is_empty()
                && catalog.migrations.is_empty(),
            "unqualified managed feature state"
        );
    } else {
        ensure!(
            catalog.imported_vms.is_empty(),
            "imported records require managed root binding"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Device {
    pub user_id: String,
    pub label: String,
    pub revoked: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub operation_id: String,
    pub kind: String,
    pub owner: User,
    pub vm: Option<Vm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_change: Option<Box<PolicyChange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_shrink: Option<Box<ShrinkSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<Box<super::vm_change::JobExtension>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyChange {
    pub previous: User,
    pub target: User,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    pub public: Operation,
    pub actor: String,
    pub fingerprint: String,
    pub job: Job,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Catalog {
    pub schema: u32,
    pub binding: Binding,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imported_vms: Vec<ImportedVm>,
    pub revision: u64,
    pub users: BTreeMap<String, User>,
    pub devices: BTreeMap<String, Device>,
    pub vms: BTreeMap<u16, Vm>,
    pub operations: BTreeMap<String, Record>,
    pub requests: BTreeMap<String, String>,
    pub audit: Vec<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub disk_shrinks: BTreeMap<u16, PendingShrink>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub enrollment_plans: BTreeMap<String, super::vm_change::EnrollmentPlan>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub disk_growths: BTreeMap<u16, super::vm_change::PendingGrowth>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub enrollments: BTreeMap<u16, super::vm_change::EnrollmentSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub migrations: BTreeMap<String, super::migration::TransferRecord>,
}

pub struct Store {
    database: Database,
}

pub fn failure(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("controller storage error: {error}");
    failure("internal", "controller storage unavailable")
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("serializable control value"))
    )
}

pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn disk_charge(vm: &Vm) -> u64 {
    vm.disk_mib + 4 + 8.max(vm.disk_mib.div_ceil(20))
}

fn uses_namespace_budget(catalog: &Catalog, vm: &Vm) -> bool {
    catalog
        .binding
        .managed
        .as_ref()
        .is_none_or(|m| m.adopted(vm.vm_id).is_none())
}

fn budget(catalog: &Catalog, user: &User) -> Budget {
    let machines: Vec<_> = catalog
        .vms
        .values()
        .filter(|vm| {
            vm.owner == user.user_id
                && vm.state != VmState::Deleted
                && uses_namespace_budget(catalog, vm)
        })
        .collect();
    let allocated_mib = machines.iter().map(|vm| disk_charge(vm)).sum::<u64>()
        + catalog
            .disk_growths
            .values()
            .filter(|g| g.owner == user.user_id)
            .map(|g| {
                let vm = &catalog.vms[&g.vm_id];
                let mut target = vm.clone();
                target.disk_mib = g.spec.target_disk_mib;
                disk_charge(&target).saturating_sub(disk_charge(vm))
            })
            .sum::<u64>();
    Budget {
        budget_mib: user.budget_mib,
        allocated_mib,
        remaining_mib: user.budget_mib.saturating_sub(allocated_mib),
        min_memory_allocated_mib: machines.iter().map(|vm| vm.min_memory_mib).sum(),
        memory_allocated_mib: machines.iter().map(|vm| vm.memory_mib).sum(),
        cores_allocated: machines.iter().map(|vm| u64::from(vm.cores)).sum(),
    }
}

fn owner_busy(catalog: &Catalog, owner: &str) -> bool {
    if catalog.migrations.values().any(|r| {
        !r.phase.terminal()
            && if r.role == super::migration::Role::Source {
                r.plan.source_owner == owner
            } else {
                r.plan.target_owner == owner
            }
    }) {
        return true;
    }
    catalog
        .operations
        .values()
        .any(|r| r.public.owner == owner && !r.public.status.is_terminal())
}

fn policy_busy(catalog: &Catalog, owner: &str) -> bool {
    if catalog.migrations.values().any(|r| {
        !r.phase.terminal()
            && if r.role == super::migration::Role::Source {
                r.plan.source_owner == owner
            } else {
                r.plan.target_owner == owner
            }
    }) {
        return true;
    }
    catalog.disk_growths.values().any(|s| s.owner == owner)
        || catalog.disk_shrinks.values().any(|s| s.owner == owner)
        || catalog.operations.values().any(|r| {
            r.public.owner == owner
                && r.job.policy_change.is_some()
                && !r.public.status.is_terminal()
        })
}

fn committed_capacity(catalog: &Catalog) -> (u64, u64) {
    catalog
        .users
        .values()
        .filter(|user| user.state != AccountState::Deleted)
        .map(|user| {
            let mut disk = user.budget_mib;
            let mut memory = user.min_memory_budget_mib;
            for record in catalog
                .operations
                .values()
                .filter(|r| r.public.owner == user.user_id && !r.public.status.is_terminal())
            {
                if let Some(change) = &record.job.policy_change {
                    disk = disk
                        .max(change.previous.budget_mib)
                        .max(change.target.budget_mib);
                    memory = memory
                        .max(change.previous.min_memory_budget_mib)
                        .max(change.target.min_memory_budget_mib);
                }
            }
            (disk, memory)
        })
        .fold(
            (
                catalog
                    .disk_shrinks
                    .values()
                    .map(|s| s.spec.workspace_mib())
                    .sum(),
                0,
            ),
            |(d, m), (next_d, next_m)| (d + next_d, m + next_m),
        )
}

fn authenticate(catalog: &Catalog, peer: &str) -> Result<User, ApiError> {
    let device = catalog
        .devices
        .get(peer)
        .filter(|device| !device.revoked)
        .ok_or_else(|| failure("forbidden", "device is not enrolled or has been revoked"))?;
    catalog
        .users
        .get(&device.user_id)
        .filter(|user| user.state == AccountState::Active)
        .cloned()
        .ok_or_else(|| failure("forbidden", "account unavailable"))
}

fn admin(user: &User) -> Result<(), ApiError> {
    if user.role != Role::Admin {
        return Err(failure("forbidden", "administrator permission required"));
    }
    Ok(())
}

fn visible(user: &User, owner: &str) -> bool {
    user.role == Role::Admin || user.user_id == owner
}

fn growth_plan(
    catalog: &Catalog,
    actor: &User,
    vm_id: u16,
    target_disk_mib: u64,
    root_partition_uuid: Option<String>,
) -> Result<super::vm_change::GrowthPlan, ApiError> {
    admin(actor)?;
    if !catalog.binding.feature("resize") {
        return Err(failure("forbidden", "scope does not authorize disk growth"));
    }
    let vm = catalog
        .vms
        .get(&vm_id)
        .ok_or_else(|| failure("invalid", "VM not found"))?;
    let owner = &catalog.users[&vm.owner];
    if vm.state != VmState::Stopped
        || !owner.ready
        || owner.state != AccountState::Active
        || owner_busy(catalog, &vm.owner)
        || policy_busy(catalog, &vm.owner)
    {
        return Err(failure(
            "conflict",
            "growth requires a stopped VM and a settled owner",
        ));
    }
    let spec = super::vm_change::GrowthSpec {
        previous_disk_mib: vm.disk_mib,
        target_disk_mib,
        root_partition_uuid: root_partition_uuid.clone(),
    };
    if !spec.valid() || target_disk_mib > catalog.binding.limits().disk_mib {
        return Err(failure(
            "invalid",
            "target must grow the root within the explicit scope limits",
        ));
    }
    let mut target = vm.clone();
    target.disk_mib = target_disk_mib;
    let additional_charge_mib = disk_charge(&target) - disk_charge(vm);
    if budget(catalog, owner).remaining_mib < additional_charge_mib {
        return Err(failure(
            "conflict",
            "owner budget cannot cover the larger disk and backing overhead",
        ));
    }
    Ok(super::vm_change::GrowthPlan {
        vm_id,
        expected_disk_mib: vm.disk_mib,
        target_disk_mib,
        additional_charge_mib,
        requires_stopped_vm: true,
        filesystem_checked_during_preparation: true,
    })
}

fn shrink_plan(
    catalog: &Catalog,
    actor: &User,
    vm_id: u16,
    target_disk_mib: u64,
    root_partition_uuid: Option<String>,
) -> Result<ShrinkPlan, ApiError> {
    admin(actor)?;
    if !catalog.binding.feature("resize") {
        return Err(failure(
            "forbidden",
            "disk shrink is restricted to controller-created disposable lab guests",
        ));
    }
    let vm = catalog
        .vms
        .get(&vm_id)
        .ok_or_else(|| failure("invalid", "VM not found"))?;
    let owner = &catalog.users[&vm.owner];
    if vm.state != VmState::Stopped
        || !owner.ready
        || owner.state != AccountState::Active
        || owner_busy(catalog, &vm.owner)
        || policy_busy(catalog, &vm.owner)
    {
        return Err(failure(
            "conflict",
            "shrink requires a stopped image guest and a settled owner without retained shrink recovery",
        ));
    }
    let spec = ShrinkSpec {
        previous_disk_mib: vm.disk_mib,
        target_disk_mib,
        preparation_id: None,
        root_partition_uuid,
    };
    if !spec.valid() || spec.previous_disk_mib > catalog.binding.limits().disk_mib {
        return Err(failure(
            "invalid",
            "target must be at least 2048 MiB and smaller than the current lab disk",
        ));
    }
    if committed_capacity(catalog).0 + spec.workspace_mib() > catalog.binding.disk_capacity_mib {
        return Err(failure(
            "conflict",
            "host admission budget cannot cover the retained source and full shrink workspace",
        ));
    }
    Ok(ShrinkPlan {
        vm_id,
        expected_disk_mib: vm.disk_mib,
        target_disk_mib,
        workspace_mib: spec.workspace_mib(),
        requires_stopped_vm: true,
        recovery_copy_retained: true,
        filesystem_checked_during_preparation: true,
    })
}

fn retry(
    catalog: &Catalog,
    actor: &User,
    request_id: &str,
    fingerprint: &str,
) -> Result<Option<Value>, ApiError> {
    if request_id.is_empty()
        || request_id.len() > 96
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(failure(
            "invalid",
            "request_id must be 1..96 ASCII letters, digits, hyphens or underscores",
        ));
    }
    if let Some(operation_id) = catalog
        .requests
        .get(&format!("{}/{request_id}", actor.user_id))
    {
        let record = &catalog.operations[operation_id];
        if record.fingerprint != fingerprint {
            return Err(failure(
                "conflict",
                "request_id already belongs to a different request",
            ));
        }
        return Ok(Some(json!(record.public)));
    }
    Ok(None)
}

fn queue(
    catalog: &mut Catalog,
    actor: &User,
    request_id: &str,
    fingerprint: &str,
    kind: &str,
    owner: User,
    vm: Option<Vm>,
) -> Value {
    let operation_id = digest(&(actor.user_id.as_str(), request_id));
    let public = Operation {
        operation_id: operation_id.clone(),
        owner: owner.user_id.clone(),
        request_id: request_id.into(),
        kind: kind.into(),
        vm_id: vm.as_ref().map(|vm| vm.vm_id),
        status: OperationStatus::Queued,
        step: "intent_committed".into(),
        error: None,
        created_at: now(),
        progress: Some(OperationProgress {
            started_at: None,
            updated_at: now(),
            finished_at: None,
            completed_steps: 0,
            total_steps: super::host::plan(kind).map_or(0, |steps| steps.len() as u32),
            detail: Some("Waiting for helper admission".into()),
        }),
    };
    let job = Job {
        operation_id: operation_id.clone(),
        kind: kind.into(),
        owner,
        vm,
        policy_change: None,
        disk_shrink: None,
        extension: None,
    };
    catalog.operations.insert(
        operation_id.clone(),
        Record {
            public: public.clone(),
            actor: actor.user_id.clone(),
            fingerprint: fingerprint.into(),
            job,
        },
    );
    catalog.requests.insert(
        format!("{}/{request_id}", actor.user_id),
        operation_id.clone(),
    );
    catalog.audit.push(json!({"at":now(),"actor":actor.user_id,"event":"intent_committed","operation_id":operation_id}));
    json!(public)
}

impl Store {
    pub fn create(path: &Path, binding: Binding, admin_peer: &str) -> Result<Self> {
        ensure!(
            !path.exists(),
            "refusing to overwrite an existing controller database"
        );
        let admin_peer = admin_peer
            .parse::<iroh::EndpointId>()
            .context("invalid administrator endpoint identity")?
            .to_string();
        binding.validate()?;
        ensure!(
            binding
                .managed
                .as_ref()
                .is_none_or(|m| m.adopted_vms.is_empty()),
            "existing-resource adoption requires the explicit paired-state preparation path"
        );
        let capacity_ceiling = binding
            .physical_trial
            .as_ref()
            .filter(|trial| !trial.scope.features.is_empty())
            .map_or_else(
                || {
                    binding
                        .managed
                        .as_ref()
                        .map_or(16384, |m| m.scope.capacity_mib)
                },
                |trial| trial.scope.capacity_mib,
            );
        ensure!(
            (128..=capacity_ceiling).contains(&binding.disk_capacity_mib),
            "controller capacity exceeds its validated backend scope"
        );
        ensure!(
            binding.min_memory_capacity_mib >= 256
                && binding.min_memory_capacity_mib
                    <= binding
                        .managed
                        .as_ref()
                        .map_or(binding.limits().memory_mib, |m| m.scope.memory_capacity_mib),
            "invalid lab minimum-memory capacity"
        );
        ensure!(
            binding.guest_images.len() <= 8
                && binding
                    .guest_images
                    .iter()
                    .all(|image| super::protocol::valid_image_digest(image)),
            "invalid lab image allowlist"
        );
        ensure!(
            binding
                .guest_images
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == binding.guest_images.len(),
            "duplicate image digests in lab allowlist"
        );
        let administrator = User {
            user_id: if let Some(m) = &binding.managed {
                m.admin_user.as_str()
            } else if binding.physical_trial.is_some() {
                "trial-admin"
            } else {
                "erin"
            }
            .into(),
            role: Role::Admin,
            budget_mib: 128.min(binding.limits().budget_mib),
            min_memory_budget_mib: 256.min(binding.limits().min_memory_mib),
            max_memory_budget_mib: 1024.min(binding.limits().memory_mib),
            max_cores: 8.min(binding.limits().cores),
            max_vms: 4,
            ready: false,
            policy_revision: 0,
            state: AccountState::Active,
        };
        let mut administrator = administrator;
        if let Some(m) = &binding.managed {
            let p = &m.admin_policy;
            administrator.budget_mib = p.budget_mib;
            administrator.min_memory_budget_mib = p.min_memory_budget_mib;
            administrator.max_memory_budget_mib = p.max_memory_budget_mib;
            administrator.max_cores = p.max_cores;
            administrator.max_vms = p.max_vms;
        }
        let mut catalog = Catalog {
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
            binding,
            revision: 0,
            users: BTreeMap::new(),
            devices: BTreeMap::new(),
            vms: BTreeMap::new(),
            operations: BTreeMap::new(),
            requests: BTreeMap::new(),
            audit: Vec::new(),
            disk_shrinks: BTreeMap::new(),
            enrollment_plans: BTreeMap::new(),
            disk_growths: BTreeMap::new(),
            enrollments: BTreeMap::new(),
            migrations: BTreeMap::new(),
        };
        catalog
            .users
            .insert(administrator.user_id.clone(), administrator.clone());
        catalog.devices.insert(
            admin_peer,
            Device {
                user_id: administrator.user_id.clone(),
                label: "bootstrap administrator".into(),
                revoked: false,
            },
        );
        queue(
            &mut catalog,
            &administrator,
            "bootstrap",
            "bootstrap",
            "ensure_user",
            administrator.clone(),
            None,
        );
        let store = Self {
            database: Database::create(path)?,
        };
        store.write_catalog(&catalog)?;
        std::fs::File::open(path.parent().context("database has no parent")?)?.sync_all()?;
        Ok(store)
    }

    pub fn open(path: &Path, binding: &Binding) -> Result<Self> {
        binding.validate()?;
        let store = Self {
            database: Database::open(path)?,
        };
        let catalog = store.read()?;
        validate_scope(&catalog)?;
        ensure!(
            matches!(catalog.schema, 1..=7) && &catalog.binding == binding,
            "database environment identity or policy changed; investigate rather than reset"
        );
        Ok(store)
    }

    fn write_catalog(&self, catalog: &Catalog) -> Result<()> {
        let mut transaction = self.database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        {
            let mut table = transaction.open_table(CATALOG)?;
            let bytes = serde_json::to_vec(catalog)?;
            table.insert(0, bytes.as_slice())?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn read(&self) -> Result<Catalog> {
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(CATALOG)?;
        let bytes = table
            .get(0)?
            .context("controller catalog missing; refusing initialization")?;
        Ok(serde_json::from_slice(bytes.value())?)
    }

    pub fn change<T>(
        &self,
        action: impl FnOnce(&mut Catalog) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut transaction = self.database.begin_write().map_err(internal)?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(internal)?;
        let result;
        {
            let mut table = transaction.open_table(CATALOG).map_err(internal)?;
            let mut catalog: Catalog = {
                let bytes = table
                    .get(0)
                    .map_err(internal)?
                    .ok_or_else(|| failure("internal", "catalog missing"))?;
                serde_json::from_slice(bytes.value()).map_err(internal)?
            };
            validate_scope(&catalog).map_err(internal)?;
            result = action(&mut catalog)?;
            validate_scope(&catalog).map_err(internal)?;
            catalog.revision = catalog
                .revision
                .checked_add(1)
                .ok_or_else(|| failure("internal", "catalog revision exhausted"))?;
            let bytes = serde_json::to_vec(&catalog).map_err(internal)?;
            table.insert(0, bytes.as_slice()).map_err(internal)?;
        }
        transaction.commit().map_err(internal)?;
        Ok(result)
    }

    pub fn remember_enrollment_plan(
        &self,
        peer: &str,
        plan: super::vm_change::EnrollmentPlan,
    ) -> Result<(), ApiError> {
        self.change(|catalog| {
            let actor = authenticate(catalog, peer)?;
            admin(&actor)?;
            if !catalog.binding.feature("enrollment") {
                return Err(failure("forbidden", "scope does not authorize enrollment"));
            }
            if !catalog.users.contains_key(&plan.vm.owner)
                || !super::host::valid_operation_id(&plan.plan_id)
            {
                return Err(failure("invalid", "invalid helper enrollment plan"));
            }
            catalog.enrollment_plans.insert(plan.plan_id.clone(), plan);
            Ok(())
        })
    }

    pub fn queue_reviewed_recovery(
        &self,
        peer: &str,
        request_id: &str,
        operation_id: &str,
        inspection_id: &str,
        kind: super::vm_change::RecoveryKind,
        confirm: bool,
    ) -> Result<Value, ApiError> {
        self.change(|catalog| {
            let actor=authenticate(catalog,peer)?;admin(&actor)?;
            let fingerprint=digest(&("recover_operation",operation_id,inspection_id,&kind,confirm));
            if let Some(result)=retry(catalog,&actor,request_id,&fingerprint)? {return Ok(result);}
            if !confirm || !catalog.binding.feature("recovery") || !super::host::valid_operation_id(inspection_id) {return Err(failure("invalid","an exact reviewed inspection and explicit recovery acknowledgment are required"));}
            let original=catalog.operations.get(operation_id).ok_or_else(||failure("invalid","original operation missing"))?.clone();
            if original.public.status!=OperationStatus::Blocked {return Err(failure("conflict","original operation is no longer blocked"));}
            if (catalog.binding.managed.is_some()
                && (kind != super::vm_change::RecoveryKind::AcknowledgeFailedStart || original.public.kind != "start"))
                || (catalog.binding.managed.is_none() && kind == super::vm_change::RecoveryKind::AcknowledgeFailedStart)
            {return Err(failure("forbidden","failed-start acknowledgment is limited to managed start operations"));}
            let accepted=queue(catalog,&actor,request_id,&fingerprint,"recover_operation",original.job.owner.clone(),original.job.vm.clone());
            let id=accepted["operation_id"].as_str().expect("operation ID");
            catalog.operations.get_mut(id).expect("queued recovery").job.extension=Some(Box::new(super::vm_change::JobExtension::Recover(super::vm_change::RecoverySpec{operation_id:operation_id.into(),inspection_id:inspection_id.into(),kind})));
            catalog.schema=catalog.schema.max(6);Ok(accepted)
        })
    }

    pub fn recovery_retry(
        &self,
        peer: &str,
        request_id: &str,
        operation_id: &str,
        inspection_id: &str,
        kind: &super::vm_change::RecoveryKind,
        confirm: bool,
    ) -> Result<Option<Value>, ApiError> {
        let catalog = self.read().map_err(internal)?;
        let actor = authenticate(&catalog, peer)?;
        admin(&actor)?;
        retry(
            &catalog,
            &actor,
            request_id,
            &digest(&(
                "recover_operation",
                operation_id,
                inspection_id,
                kind,
                confirm,
            )),
        )
    }

    pub fn capabilities_for(&self, binding: &Binding) -> Vec<String> {
        let mut values = Vec::new();
        for (feature, actions) in [
            (
                "resize",
                &["shutdown", "shrink_vm", "plan_vm_growth", "grow_vm"][..],
            ),
            ("recovery", &["inspect_operation", "recover_operation"][..]),
            ("enrollment", &["plan_vm_enrollment", "enroll_vm"][..]),
            ("migration", &["migration", "shutdown"][..]),
        ] {
            if binding.feature(feature) {
                values.extend(actions.iter().map(|s| (*s).to_string()));
            }
        }
        if binding
            .managed
            .as_ref()
            .is_some_and(|m| !m.adopted_vms.is_empty())
        {
            values.push("manage_existing_vm".into());
        }
        if binding.managed.is_some() {
            values.extend(
                [
                    "host_inventory",
                    "create_user",
                    "update_user_policy",
                    "delete_empty_user",
                    "create_guest",
                    "start_new_vm",
                    "shutdown_new_vm",
                    "stop_new_vm",
                    "delete_new_vm",
                    "list_image_catalog",
                ]
                .map(str::to_string),
            );
        }
        values.sort();
        values.dedup();
        values
    }

    pub fn actor(&self, peer: &str) -> Result<User, ApiError> {
        authenticate(&self.read().map_err(internal)?, peer)
    }

    pub fn authorized(&self, peer: &str) -> bool {
        self.read()
            .ok()
            .is_some_and(|catalog| authenticate(&catalog, peer).is_ok())
    }

    pub fn handle(&self, peer: &str, request: Request) -> Result<Value, ApiError> {
        let fingerprint = digest(&request);
        self.change(|catalog| {
            let actor = authenticate(catalog, peer)?;
            if catalog.binding.managed.as_ref().is_some_and(|m| m.admin_only_mutations) && !matches!(request, Request::Dashboard {} | Request::Whoami {} | Request::Budget {} | Request::ListImages {} | Request::ListImageCatalog {} | Request::ListVms {} | Request::ListOperations {} | Request::GetOperation { .. }) { admin(&actor)?; }
            match request {
                Request::HostInventory {} => Err(failure("unavailable", "host inventory requires a live bound helper")),
                Request::EnrollVm {request_id,plan_id,vm_id,owner,confirm_enrollment} => {
                    admin(&actor)?;
                    let fingerprint=digest(&("enroll_vm",&plan_id,vm_id,&owner,confirm_enrollment));
                    if let Some(result)=retry(catalog,&actor,&request_id,&fingerprint)? {return Ok(result);}
                    if !confirm_enrollment || !catalog.binding.feature("enrollment") {return Err(failure("forbidden","explicit enrollment approval and scope are required"));}
                    let plan=catalog.enrollment_plans.get(&plan_id).ok_or_else(||failure("invalid","review the live enrollment plan first"))?.clone();
                    if !plan.eligible || plan.vm.vm_id!=vm_id || plan.vm.owner!=owner || now().saturating_sub(plan.observed_at)>300 {return Err(failure("conflict","enrollment plan is stale or belongs to another VM/owner"));}
                    let user=catalog.users.get(&owner).filter(|u|u.ready && u.state==AccountState::Active).ok_or_else(||failure("invalid","owner is not ready/active"))?.clone();
                    if catalog.vms.contains_key(&vm_id) || owner_busy(catalog,&owner) || policy_busy(catalog,&owner) {return Err(failure("conflict","VM has prior history or owner has unsettled work"));}
                    let current=budget(catalog,&user);
                    if current.remaining_mib<disk_charge(&plan.vm) || current.min_memory_allocated_mib+plan.vm.min_memory_mib>user.min_memory_budget_mib || current.memory_allocated_mib+plan.vm.memory_mib>user.max_memory_budget_mib || current.cores_allocated+u64::from(plan.vm.cores)>u64::from(user.max_cores) || catalog.vms.values().filter(|v|v.owner==owner && v.state!=VmState::Deleted).count()>=usize::from(user.max_vms) {return Err(failure("conflict","owner policy cannot cover the existing VM"));}
                    if let Some(scope)=&catalog.binding.physical_trial && !scope.scope.permits(&owner,Some(vm_id)) {return Err(failure("forbidden","enrollment escapes physical scope"));}
                    let mut admitted=plan.vm.clone();admitted.state=VmState::Creating;catalog.vms.insert(vm_id,admitted);
                    let accepted=queue(catalog,&actor,&request_id,&fingerprint,"enroll_vm",user,Some(plan.vm.clone()));
                    let id=accepted["operation_id"].as_str().expect("operation ID");catalog.operations.get_mut(id).expect("queued enrollment").job.extension=Some(Box::new(super::vm_change::JobExtension::Enroll(super::vm_change::EnrollmentSpec{plan})));
                    catalog.schema=catalog.schema.max(6);Ok(accepted)
                }
                Request::PlanVmGrowth {vm_id,target_disk_mib,root_partition_uuid} => Ok(json!(growth_plan(catalog,&actor,vm_id,target_disk_mib,root_partition_uuid)?)),
                Request::GrowVm {request_id,vm_id,expected_disk_mib,target_disk_mib,confirm_downtime,root_partition_uuid} => {
                    admin(&actor)?;
                    let fingerprint=digest(&("grow_vm",vm_id,expected_disk_mib,target_disk_mib,confirm_downtime,&root_partition_uuid));
                    if let Some(result)=retry(catalog,&actor,&request_id,&fingerprint)? {return Ok(result);}
                    if !confirm_downtime {return Err(failure("invalid","explicit downtime acknowledgment is required"));}
                    let _plan=growth_plan(catalog,&actor,vm_id,target_disk_mib,root_partition_uuid.clone())?;
                    let vm=catalog.vms[&vm_id].clone();
                    if vm.disk_mib!=expected_disk_mib {return Err(failure("conflict","disk size changed since review"));}
                    let owner=catalog.users[&vm.owner].clone();
                    let spec=super::vm_change::GrowthSpec {previous_disk_mib:expected_disk_mib,target_disk_mib,root_partition_uuid};
                    let accepted=queue(catalog,&actor,&request_id,&fingerprint,"grow_vm",owner,Some(vm.clone()));
                    let id=accepted["operation_id"].as_str().expect("operation id").to_string();
                    catalog.operations.get_mut(&id).expect("queued operation").job.extension=Some(Box::new(super::vm_change::JobExtension::Grow(spec.clone())));
                    catalog.disk_growths.insert(vm_id,super::vm_change::PendingGrowth {operation_id:id,vm_id,owner:vm.owner,spec});
                    catalog.schema=catalog.schema.max(6);
                    Ok(accepted)
                }
                Request::InspectOperation {..} | Request::RecoverOperation {..} |
                Request::PlanVmEnrollment {..} |
                Request::Migration {..} =>
                    Err(failure("unavailable", "this action requires the live, bound helper service")),
                Request::PlanVmShrink { vm_id, target_disk_mib, root_partition_uuid } => Ok(json!(shrink_plan(catalog, &actor, vm_id, target_disk_mib, root_partition_uuid)?)),
                Request::ShrinkVm { request_id, vm_id, expected_disk_mib, target_disk_mib, confirm_downtime, root_partition_uuid } => {
                    admin(&actor)?;
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    if !confirm_downtime { return Err(failure("invalid", "explicit downtime acknowledgement required")); }
                    let plan = shrink_plan(catalog, &actor, vm_id, target_disk_mib, root_partition_uuid.clone())?;
                    if expected_disk_mib != plan.expected_disk_mib { return Err(failure("conflict", "disk size changed; refresh and review a new shrink plan")); }
                    let vm = catalog.vms[&vm_id].clone();
                    let owner = catalog.users[&vm.owner].clone();
                    let spec = ShrinkSpec { previous_disk_mib: expected_disk_mib, target_disk_mib, preparation_id: None, root_partition_uuid };
                    let value = queue(catalog, &actor, &request_id, &fingerprint, "shrink_vm", owner, Some(vm.clone()));
                    let id = value["operation_id"].as_str().expect("queued operation");
                    catalog.operations.get_mut(id).expect("queued operation").job.disk_shrink = Some(Box::new(spec.clone()));
                    catalog.disk_shrinks.insert(vm_id, PendingShrink { operation_id: id.into(), vm_id, owner: vm.owner, spec, prepared: false });
                    catalog.schema = catalog.schema.max(5);
                    Ok(value)
                }
                Request::FinishVmShrink { request_id, vm_id, preparation_id, confirm_guest_verified, confirm_delete_recovery } => {
                    admin(&actor)?;
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    if !confirm_guest_verified || !confirm_delete_recovery { return Err(failure("invalid", "explicit guest verification and recovery-copy deletion acknowledgements required")); }
                    if !catalog.binding.feature("resize") { return Err(failure("forbidden", "physical scope does not authorize finishing disk shrink")); }
                    let hold = catalog.disk_shrinks.get(&vm_id).cloned().ok_or_else(|| failure("conflict", "no retained shrink recovery for this VM"))?;
                    let vm = catalog.vms[&vm_id].clone();
                    if !hold.prepared || hold.operation_id != preparation_id || vm.state != VmState::Stopped
                        || vm.disk_mib != hold.spec.target_disk_mib || owner_busy(catalog, &vm.owner) {
                        return Err(failure("conflict", "finish requires the exact prepared shrink, a stopped verified guest and a settled owner"));
                    }
                    let owner = catalog.users[&vm.owner].clone();
                    let value = queue(catalog, &actor, &request_id, &fingerprint, "finish_vm_shrink", owner, Some(vm));
                    let id = value["operation_id"].as_str().expect("queued operation");
                    let mut spec = hold.spec; spec.preparation_id = Some(preparation_id);
                    catalog.operations.get_mut(id).expect("queued operation").job.disk_shrink = Some(Box::new(spec));
                    Ok(value)
                }
                Request::Whoami {} => Ok(json!(actor)),
                Request::Budget {} => Ok(json!(budget(catalog, &actor))),
                Request::ListImages {} => Ok(json!(catalog.binding.guest_images)),
                Request::ListImageCatalog {} => Ok(json!(catalog.binding.managed.as_ref().map_or_else(Vec::new, |m| m.images.iter().map(|i|i.catalog()).collect::<Vec<_>>()))),
                Request::ListVms {} => Ok(json!(catalog.vms.values().filter(|vm| visible(&actor, &vm.owner)).collect::<Vec<_>>())),
                Request::ListOperations {} => Ok(json!(catalog.operations.values().filter(|record| visible(&actor, &record.public.owner)).map(|record| &record.public).collect::<Vec<_>>())),
                Request::GetOperation { operation_id } => {
                    let record = catalog.operations.get(&operation_id).filter(|record| visible(&actor, &record.public.owner))
                        .ok_or_else(|| failure("forbidden", "operation not found or forbidden"))?;
                    Ok(json!(record.public))
                }
                Request::Dashboard {} => Ok(json!(Dashboard {
                    storage_allocations: super::adoption::allocations(catalog).into_iter().filter(|a| visible(&actor, &a.owner)).collect(),
                    identity: actor.clone(), users: catalog.users.values().filter(|user| visible(&actor, &user.user_id)).cloned().collect(),
                    budget: budget(catalog, &actor), vms: catalog.vms.values().filter(|vm| visible(&actor, &vm.owner)).cloned().collect(),
                    operations: catalog.operations.values().filter(|record| visible(&actor, &record.public.owner)).map(|record| record.public.clone()).collect(),
                    environment: catalog.binding.environment.clone(), backend: catalog.binding.backend.clone(),
                    disk_shrinks: catalog.disk_shrinks.values().filter(|s| visible(&actor, &s.owner)).cloned().collect(),
                    capabilities: self.capabilities_for(&catalog.binding),
                    imported_vms: if actor.role == Role::Admin { catalog.imported_vms.iter().filter(|r| catalog.binding.managed.as_ref().is_none_or(|m| m.adopted(r.vm.vm_id).is_none())).cloned().collect() } else { Vec::new() },
                })),
                Request::EnrollDevice { user_id, endpoint_id, label } => {
                    admin(&actor)?;
                    let endpoint_id = endpoint_id.parse::<iroh::EndpointId>().map_err(|_| failure("invalid", "invalid device endpoint identity"))?.to_string();
                    if !catalog.users.get(&user_id).is_some_and(|user| user.state == AccountState::Active) { return Err(failure("invalid", "active account does not exist")); }
                    if label.len() > 80 || label.chars().any(char::is_control) { return Err(failure("invalid", "invalid device label")); }
                    if let Some(device) = catalog.devices.get(&endpoint_id)
                        && (device.user_id != user_id || device.revoked) {
                        return Err(failure("conflict", "device identity was previously assigned or revoked; enroll a fresh key"));
                    }
                    catalog.devices.insert(endpoint_id.clone(), Device { user_id: user_id.clone(), label, revoked: false });
                    catalog.audit.push(json!({"at":now(),"actor":actor.user_id,"event":"device_enrolled","user_id":user_id,"endpoint_id":endpoint_id}));
                    Ok(json!({"updated":true}))
                }
                Request::RevokeDevice { endpoint_id } => {
                    admin(&actor)?;
                    let target = catalog.devices.get(&endpoint_id).ok_or_else(|| failure("invalid", "device not found"))?;
                    if catalog.users[&target.user_id].role == Role::Admin && !target.revoked
                        && catalog.devices.values().filter(|device| !device.revoked && catalog.users[&device.user_id].role == Role::Admin).count() <= 1 {
                        return Err(failure("conflict", "cannot revoke the last administrator device"));
                    }
                    catalog.devices.get_mut(&endpoint_id).expect("existing device").revoked = true;
                    catalog.audit.push(json!({"at":now(),"actor":actor.user_id,"event":"device_revoked","endpoint_id":endpoint_id}));
                    Ok(json!({"updated":true}))
                }
                Request::CreateUser { request_id, user_id, budget_mib, min_memory_budget_mib, max_memory_budget_mib, max_cores, max_vms } => {
                    admin(&actor)?;
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    if !valid_name(&user_id) || user_id.len() > 24 || !(ResourcePolicy { budget_mib,min_memory_budget_mib,max_memory_budget_mib,max_cores,max_vms }).valid_with_limits(catalog.binding.limits()) {
                        return Err(failure("invalid", "invalid or non-lab account resource policy"));
                    }
                    if catalog.binding.physical_trial.as_ref().is_some_and(|trial| !trial.scope.permits(&user_id, None)) { return Err(failure("forbidden", "account is outside the explicit physical trial scope")); }
                    if catalog.binding.managed.as_ref().is_some_and(|m| !m.scope.permits(&user_id,None)) { return Err(failure("forbidden", "account outside root managed allowlist")); }
                    if catalog.users.contains_key(&user_id) { return Err(failure("conflict", "account already exists")); }
                    if committed_capacity(catalog).0 + budget_mib > catalog.binding.disk_capacity_mib
                        || committed_capacity(catalog).1 + min_memory_budget_mib > catalog.binding.min_memory_capacity_mib {
                        return Err(failure("conflict", "host disk or minimum-memory admission budget exhausted"));
                    }
                    let user = User { user_id, role: Role::Member, budget_mib, min_memory_budget_mib, max_memory_budget_mib, max_cores, max_vms, ready: false, policy_revision: 0, state: AccountState::Active };
                    catalog.users.insert(user.user_id.clone(), user.clone());
                    Ok(queue(catalog, &actor, &request_id, &fingerprint, "ensure_user", user, None))
                }
                Request::UpdateUserPolicy { request_id, user_id, expected_revision, policy } => {
                    admin(&actor)?;
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    if !policy.valid_with_limits(catalog.binding.limits()) { return Err(failure("invalid", "positive lab policy required; zero, unlimited quota and allocation removal are unsupported")); }
                    let previous = catalog.users.get(&user_id).cloned().ok_or_else(|| failure("invalid", "account not found"))?;
                    if previous.policy_revision != expected_revision { return Err(failure("conflict", "policy changed; refresh and review the current policy revision")); }
                    if previous.state != AccountState::Active || !previous.ready || owner_busy(catalog, &user_id) || policy_busy(catalog, &user_id) { return Err(failure("conflict", "owner has pending work or retained shrink recovery; policy changes require a settled owner")); }
                    let usage = budget(catalog, &previous);
                    let count = catalog.vms.values().filter(|vm| vm.owner == user_id && vm.state != VmState::Deleted && uses_namespace_budget(catalog, vm)).count();
                    if policy.budget_mib < usage.allocated_mib || policy.min_memory_budget_mib < usage.min_memory_allocated_mib
                        || policy.max_memory_budget_mib < usage.memory_allocated_mib || u64::from(policy.max_cores) < usage.cores_allocated
                        || usize::from(policy.max_vms) < count {
                        return Err(failure("conflict", "target policy is below existing committed allocations; free allocations first (guest disk contents do not reduce a thick disk commitment)"));
                    }
                    if previous.policy() == policy { return Err(failure("conflict", "policy is unchanged")); }
                    let target = previous.with_policy(&policy).ok_or_else(|| failure("conflict", "policy revision exhausted"))?;
                    let capacity = committed_capacity(catalog);
                    if capacity.0 + target.budget_mib.saturating_sub(previous.budget_mib) > catalog.binding.disk_capacity_mib
                        || capacity.1 + target.min_memory_budget_mib.saturating_sub(previous.min_memory_budget_mib) > catalog.binding.min_memory_capacity_mib {
                        return Err(failure("conflict", "host budget exhausted, including pending policy reservations"));
                    }
                    let value = queue(catalog, &actor, &request_id, &fingerprint, "update_user_policy", previous.clone(), None);
                    let id = value["operation_id"].as_str().expect("queued operation");
                    catalog.operations.get_mut(id).expect("queued operation").job.policy_change = Some(Box::new(PolicyChange { previous, target }));
                    catalog.users.get_mut(&user_id).expect("existing owner").ready = false;
                    catalog.schema = catalog.schema.max(2);
                    Ok(value)
                }
                Request::DeleteUser { request_id, user_id, expected_revision } => {
                    admin(&actor)?;
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    let owner = catalog.users.get(&user_id).cloned().ok_or_else(|| failure("invalid", "account not found"))?;
                    if owner.user_id == actor.user_id || owner.role != Role::Member {
                        return Err(failure("forbidden", "self-deletion and administrator deletion are unsupported"));
                    }
                    if owner.policy_revision != expected_revision { return Err(failure("conflict", "policy changed; refresh before deleting the empty account")); }
                    if owner.state != AccountState::Active || !owner.ready || owner_busy(catalog, &user_id) {
                        return Err(failure("conflict", "account is inactive, not ready, or has pending/blocked work"));
                    }
                    if catalog.vms.values().any(|vm| vm.owner == user_id && vm.state != VmState::Deleted) {
                        return Err(failure("conflict", "delete every VM explicitly and wait for verified deletion before deleting this account; no cascade is performed"));
                    }
                    let result = queue(catalog, &actor, &request_id, &fingerprint, "delete_user", owner, None);
                    let user = catalog.users.get_mut(&user_id).expect("existing user");
                    user.state = AccountState::Deleting; user.ready = false;
                    for device in catalog.devices.values_mut().filter(|device| device.user_id == user_id) { device.revoked = true; }
                    catalog.schema = catalog.schema.max(3);
                    catalog.audit.push(json!({"at":now(),"event":"empty_user_deletion_accepted_devices_revoked","user_id":user_id}));
                    Ok(result)
                }
                Request::CreateVm { request_id, name, disk_mib, memory_mib, min_memory_mib, cores, owner, guest } => {
                    let owner_id = owner.as_deref().unwrap_or(&actor.user_id);
                    if !visible(&actor, owner_id) { return Err(failure("forbidden", "cannot allocate for another account")); }
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    let owner = catalog.users.get(owner_id).cloned().ok_or_else(|| failure("invalid", "account not found"))?;
                    if owner.state != AccountState::Active || !owner.ready || policy_busy(catalog, owner_id) { return Err(failure("conflict", "account storage is not verified ready or policy change is unfinished")); }
                    if let Some(guest) = &guest {
                        guest.validate().map_err(|error| failure("invalid", &error.to_string()))?;
                        if !catalog.binding.guest_images.contains(&guest.image_sha256) {
                            return Err(failure("invalid", "image digest is not in the administrator-approved lab allowlist"));
                        }
                    }
                    if let Some(m) = &catalog.binding.managed {
                        let g = guest.as_ref().ok_or_else(||failure("invalid","managed creation requires a bootable guest specification"))?;
                        let i = m.images.iter().find(|i|i.image_sha256==g.image_sha256).ok_or_else(||failure("invalid","image is not root-approved"))?;
                        if disk_mib<i.min_disk_mib { return Err(failure("invalid","disk is below approved image minimum")); }
                        let active=catalog.vms.values().filter(|v|v.state!=VmState::Deleted && m.adopted(v.vm_id).is_none()).collect::<Vec<_>>();
                        if active.iter().map(|v|v.memory_mib).sum::<u64>().saturating_add(memory_mib)>m.scope.memory_capacity_mib || active.iter().map(|v|u64::from(v.cores)).sum::<u64>()+u64::from(cores)>u64::from(m.scope.cores_capacity) { return Err(failure("conflict","managed scope compute capacity exhausted")); }
                    }
                    let limits = catalog.binding.limits();
                    let expanded = catalog.binding.managed.is_some() || catalog.binding.physical_trial.as_ref().is_some_and(|b| b.scope.limits.is_some());
                    let (max_disk, max_memory) = if guest.is_some() || expanded { (limits.disk_mib, limits.memory_mib) } else { (512, 512) };
                    if !valid_name(&name) || !(16..=max_disk).contains(&disk_mib) || !(128..=max_memory).contains(&memory_mib)
                        || min_memory_mib < 128 || min_memory_mib > memory_mib || !(1..=if expanded { limits.cores } else { 4 }).contains(&cores) {
                        return Err(failure("invalid", "invalid or non-lab VM profile"));
                    }
                    let machines: Vec<_> = catalog.vms.values().filter(|vm| vm.owner == owner_id && vm.state != VmState::Deleted && uses_namespace_budget(catalog, vm)).collect();
                    if machines.iter().any(|vm| vm.name == name) { return Err(failure("conflict", "VM name already allocated")); }
                    let vm_id = (9001..=9299).find(|id| !catalog.vms.contains_key(id) && !catalog.imported_vms.iter().any(|r| r.vm.vm_id==*id) && if let Some(m)=&catalog.binding.managed { m.scope.vm_ids.contains(id) } else { (9001..=9099).contains(id) && catalog.binding.physical_trial.as_ref().is_none_or(|trial| trial.scope.vm_ids.contains(id)) }).ok_or_else(|| failure("conflict", "lab VM identifiers exhausted"))?;
                    let kind = if guest.is_some() { "create_guest" } else { "create" };
                    let vm = Vm { vm_id, owner: owner.user_id.clone(), name, disk_mib, memory_mib, min_memory_mib, cores, state: VmState::Creating, guest: guest.map(Box::new) };
                    let usage = budget(catalog, &owner);
                    if machines.len() >= usize::from(owner.max_vms) || usage.allocated_mib + disk_charge(&vm) > owner.budget_mib
                        || usage.min_memory_allocated_mib + min_memory_mib > owner.min_memory_budget_mib
                        || usage.memory_allocated_mib + memory_mib > owner.max_memory_budget_mib
                        || usage.cores_allocated + u64::from(cores) > u64::from(owner.max_cores) {
                        return Err(failure("conflict", "account resource budget exhausted, including pending or blocked allocations"));
                    }
                    catalog.vms.insert(vm_id, vm.clone());
                    Ok(queue(catalog, &actor, &request_id, &fingerprint, kind, owner, Some(vm)))
                }
                Request::VmAction { request_id, vm_id, kind } => {
                    let vm = catalog.vms.get(&vm_id).filter(|vm| visible(&actor, &vm.owner)).cloned()
                        .ok_or_else(|| failure("forbidden", "VM not found or forbidden"))?;
                    if let Some(result) = retry(catalog, &actor, &request_id, &fingerprint)? { return Ok(result); }
                    if vm.state == VmState::Deleted || catalog.operations.values().any(|record| record.public.vm_id == Some(vm_id) && !record.public.status.is_terminal()) {
                        return Err(failure("conflict", "VM is deleted, busy, or blocked pending investigation"));
                    }
                    let kind = match kind { VmAction::Start => "start", VmAction::Shutdown => "shutdown", VmAction::Stop => "stop", VmAction::Delete => "delete" };
                    if kind == "shutdown" {
                        if catalog.binding.physical_trial.as_ref().is_some_and(|trial| {
                            !trial.scope.features.contains("resize") && !trial.scope.features.contains("migration")
                        }) { return Err(failure("forbidden", "graceful shutdown is currently lab-qualified only")); }
                        catalog.schema = catalog.schema.max(5);
                    }
                    if catalog.users[&vm.owner].state != AccountState::Active { return Err(failure("conflict", "owner is inactive or being deleted")); }
                    let shrink_lifecycle = catalog.disk_shrinks.get(&vm_id).is_some_and(|s| s.prepared)
                        && matches!(kind, "start" | "stop" | "shutdown");
                    if policy_busy(catalog, &vm.owner) && !shrink_lifecycle { return Err(failure("conflict", "owner policy change or disk shrink recovery is pending or blocked")); }
                    if kind == "delete" { catalog.vms.get_mut(&vm_id).expect("existing VM").state = VmState::Deleting; }
                    let owner = catalog.users[&vm.owner].clone();
                    Ok(queue(catalog, &actor, &request_id, &fingerprint, kind, owner, Some(vm)))
                }
            }
        })
    }

    pub fn pending(&self) -> Result<Vec<Record>> {
        Ok(self
            .read()?
            .operations
            .into_values()
            .filter(|record| {
                matches!(
                    record.public.status,
                    OperationStatus::Queued | OperationStatus::Running
                )
            })
            .collect())
    }

    pub fn progress(
        &self,
        operation_id: &str,
        status: OperationStatus,
        step: &str,
        error: Option<String>,
    ) -> Result<(), ApiError> {
        self.progress_detailed(operation_id, status, step, error, None)
    }

    pub fn progress_detailed(
        &self,
        operation_id: &str,
        status: OperationStatus,
        step: &str,
        error: Option<String>,
        progress: Option<OperationProgress>,
    ) -> Result<(), ApiError> {
        self.change(|catalog| {
            let record = catalog.operations.get_mut(operation_id).ok_or_else(|| failure("internal", "operation missing"))?;
            if matches!(record.public.status, OperationStatus::Verified | OperationStatus::Blocked | OperationStatus::Cancelled) { return Ok(()); }
            record.public.status = status.clone();
            record.public.step = step.into();
            record.public.error = error.clone();
            if let Some(progress) = &progress {
                if progress.completed_steps > progress.total_steps || progress.started_at.is_some_and(|v| v > progress.updated_at)
                    || progress.finished_at.is_some_and(|v| v > progress.updated_at) {
                    return Err(failure("internal", "invalid helper progress milestones"));
                }
                record.public.progress = Some(progress.clone());
            }
            if status==OperationStatus::Verified && record.public.kind=="recover_operation" {
                let Some(super::vm_change::JobExtension::Recover(spec))=record.job.extension.as_deref() else {return Err(failure("internal","recovery payload missing"));};
                let spec=spec.clone();
                let original=catalog.operations.get_mut(&spec.operation_id).ok_or_else(||failure("internal","original operation missing"))?;
                if original.public.status!=OperationStatus::Blocked {return Err(failure("internal","original recovery state changed"));}
                let kind=original.public.kind.clone();let vm_id=original.public.vm_id;
                if spec.kind != super::vm_change::RecoveryKind::AcknowledgeFailedStart {original.public.error=None;}
                match spec.kind {
                    super::vm_change::RecoveryKind::ResumeVerifiedEffect=>{original.public.status=OperationStatus::Queued;original.public.step="reconciled_waiting_for_helper".into();}
                    super::vm_change::RecoveryKind::AbortPreparation=>{original.public.status=OperationStatus::Cancelled;original.public.step="cancelled".into();}
                    super::vm_change::RecoveryKind::AcknowledgeFailedStart=>{original.public.status=OperationStatus::Cancelled;original.public.step="failed_start_acknowledged".into();}
                }
                if let Some(vm_id)=vm_id {
                    let vm=catalog.vms.get_mut(&vm_id).ok_or_else(||failure("internal","recovered VM missing"))?;
                    vm.state=if matches!(spec.kind,super::vm_change::RecoveryKind::AbortPreparation | super::vm_change::RecoveryKind::AcknowledgeFailedStart) {VmState::Stopped}else{VmState::Creating};
                    if spec.kind==super::vm_change::RecoveryKind::AbortPreparation {
                        if kind=="shrink_vm" {catalog.disk_shrinks.remove(&vm_id);}
                        if kind=="grow_vm" {catalog.disk_growths.remove(&vm_id);}
                    }
                }
                for (id,r) in &mut catalog.operations {
                    if id!=operation_id && !r.public.status.is_terminal() && matches!(r.job.extension.as_deref(),Some(super::vm_change::JobExtension::Recover(s)) if s.operation_id==spec.operation_id) {
                        r.public.status=OperationStatus::Cancelled;r.public.step="superseded_by_verified_recovery".into();r.public.error=None;
                    }
                }
                catalog.audit.push(json!({"at":now(),"event":"operation_reconciled","recovery_id":operation_id,"original_id":spec.operation_id,"kind":spec.kind}));
                return Ok(());
            }
            if let Some(vm_id) = record.public.vm_id {
                let machine = catalog.vms.get_mut(&vm_id).expect("operation VM exists");
                if status == OperationStatus::Blocked { machine.state = VmState::Blocked; }
                if status == OperationStatus::Verified {
                    machine.state = match record.public.kind.as_str() {
                        "create" | "create_guest" | "stop" | "shutdown" | "shrink_vm" | "finish_vm_shrink" | "grow_vm" | "enroll_vm" => VmState::Stopped,
                        "start" => VmState::Running,
                        "delete" => VmState::Deleted,
                        _ => return Err(failure("internal", "unknown VM operation")),
                    };
                    if record.public.kind == "shrink_vm" {
                        let hold = catalog.disk_shrinks.get_mut(&vm_id).ok_or_else(|| failure("internal", "shrink hold missing"))?;
                        machine.disk_mib = hold.spec.target_disk_mib;
                        hold.prepared = true;
                    }
                    if record.public.kind == "finish_vm_shrink" { catalog.disk_shrinks.remove(&vm_id); }
                    if record.public.kind == "enroll_vm" {
                        let Some(super::vm_change::JobExtension::Enroll(spec))=record.job.extension.as_deref() else {return Err(failure("internal","enrollment payload missing"));};
                        catalog.enrollments.insert(vm_id,spec.clone());
                    }
                    if record.public.kind == "grow_vm" {
                        let hold=catalog.disk_growths.remove(&vm_id).ok_or_else(||failure("internal","growth hold missing"))?;
                        machine.disk_mib=hold.spec.target_disk_mib;
                    }

                }
            } else if status == OperationStatus::Verified && record.public.kind == "delete_user" {
                let owner = catalog.users.get_mut(&record.public.owner).expect("operation account exists");
                if owner.state != AccountState::Deleting || owner.policy_revision != record.job.owner.policy_revision
                    || owner.policy() != record.job.owner.policy() {
                    return Err(failure("internal", "account deletion lost its frozen current identity/policy"));
                }
                owner.state = AccountState::Deleted; owner.ready = false;
            } else if status == OperationStatus::Verified {
                if let Some(change) = &record.job.policy_change {
                    let current = catalog.users.get(&record.public.owner).expect("operation account exists");
                    if current.policy_revision != change.previous.policy_revision || current.policy() != change.previous.policy() {
                        return Err(failure("internal", "policy commit no longer matches its frozen previous revision"));
                    }
                    catalog.users.insert(record.public.owner.clone(), change.target.clone());
                }
                catalog.users.get_mut(&record.public.owner).expect("operation account exists").ready = true;
            }
            if matches!(status, OperationStatus::Verified | OperationStatus::Blocked | OperationStatus::Cancelled) {
                catalog.audit.push(json!({"at":now(),"event":"operation_terminal","operation_id":operation_id,"status":status}));
            }
            Ok(())
        })
    }
}
