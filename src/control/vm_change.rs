//! Immutable payloads for reviewed VM changes. Root-side observations remain
//! authoritative; client plans cannot turn observed resources into ownership.
use super::protocol::{Operation, Vm};
use crate::control::JobKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_DISK_MIB: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrowthSpec {
    pub previous_disk_mib: u64,
    pub target_disk_mib: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_partition_uuid: Option<String>,
}
impl GrowthSpec {
    pub fn valid(&self) -> bool {
        self.previous_disk_mib >= 2048
            && self.previous_disk_mib < self.target_disk_mib
            && self.target_disk_mib <= MAX_DISK_MIB
            && self
                .root_partition_uuid
                .as_deref()
                .is_none_or(valid_partition_uuid)
    }
}
pub fn valid_partition_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GrowthPlan {
    pub vm_id: u16,
    pub expected_disk_mib: u64,
    pub target_disk_mib: u64,
    pub additional_charge_mib: u64,
    pub requires_stopped_vm: bool,
    pub filesystem_checked_during_preparation: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EnrollmentPlan {
    pub plan_id: String,
    pub vm: Vm,
    pub configuration_sha256: String,
    pub root_guid: String,
    pub cloud_init_guid: String,
    pub observed_at: u64,
    pub eligible: bool,
    pub blocked_reasons: Vec<String>,
    pub evidence: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentSpec {
    pub plan: EnrollmentPlan,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    ResumeVerifiedEffect,
    AbortPreparation,
    /// Cancel only a root-proven, failed managed qmstart attempt. This neither
    /// starts the VM nor releases its allocation; a fresh start is separate.
    AcknowledgeFailedStart,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecoveryEvidence {
    pub name: String,
    pub status: String,
    pub detail: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationInspection {
    pub operation: Operation,
    pub inspection_id: String,
    pub observed_at: u64,
    pub classification: String,
    pub evidence: Vec<RecoveryEvidence>,
    pub allowed_actions: Vec<RecoveryKind>,
    pub proof: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "spec",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum JobExtension {
    Grow(GrowthSpec),
    Enroll(EnrollmentSpec),
    Recover(RecoverySpec),
    IncomingMigration(super::migration::Plan),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingGrowth {
    pub operation_id: String,
    pub vm_id: u16,
    pub owner: String,
    pub spec: GrowthSpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySpec {
    pub operation_id: String,
    pub inspection_id: String,
    pub kind: RecoveryKind,
}

pub fn validate_catalog(c: &super::store::Catalog) -> anyhow::Result<()> {
    use super::protocol::OperationStatus;
    use anyhow::{Context, ensure};
    let mut owners = std::collections::BTreeSet::new();
    for (id, hold) in &c.disk_growths {
        let r = c
            .operations
            .get(&hold.operation_id)
            .context("growth hold lacks operation")?;
        let vm = c.vms.get(id).context("growth hold lacks VM")?;
        ensure!(
            c.schema >= 6
                && id == &hold.vm_id
                && hold.spec.valid()
                && !r.public.status.is_terminal()
                && owners.insert(&hold.owner)
                && !c.disk_shrinks.values().any(|s| s.owner == hold.owner)
                && r.public.owner == hold.owner
                && vm.owner == hold.owner
                && vm.disk_mib == hold.spec.previous_disk_mib
                && matches!(r.job.extension.as_deref(),Some(JobExtension::Grow(spec)) if spec==&hold.spec),
            "invalid growth admission hold"
        );
    }
    for r in c.operations.values() {
        if let Some(extension) = r.job.extension.as_deref() {
            ensure!(c.schema >= 6, "extended operation requires schema6");
            match extension {
                JobExtension::Grow(spec) => {
                    ensure!(
                        r.job.kind == JobKind::GrowVm && spec.valid(),
                        "invalid growth operation"
                    );
                    if !r.public.status.is_terminal() {
                        ensure!(
                            c.disk_growths
                                .values()
                                .any(|h| h.operation_id == r.public.operation_id),
                            "growth admission claim vanished before verified completion/cancellation"
                        );
                    }
                    if r.public.status == OperationStatus::Cancelled {
                        ensure!(c.operations.values().any(|p|p.public.status==OperationStatus::Verified && matches!(p.job.extension.as_deref(),Some(JobExtension::Recover(s)) if s.operation_id==r.public.operation_id && s.kind==RecoveryKind::AbortPreparation)),"growth cancelled without a verified abort decision");
                    }
                }
                JobExtension::Enroll(spec) => {
                    ensure!(
                        r.job.kind == JobKind::EnrollVm
                            && spec.plan.eligible
                            && r.public.vm_id == Some(spec.plan.vm.vm_id)
                            && r.public.owner == spec.plan.vm.owner
                            && spec.plan.vm.owner == r.job.owner.user_id,
                        "invalid enrollment binding"
                    );
                }
                JobExtension::IncomingMigration(plan) => {
                    plan.validate()?;
                    ensure!(
                        r.job.kind == JobKind::Create
                            && plan.destination.environment == c.binding.environment
                            && r.job.operation_id
                                == super::migration_transport::target_create_id(&plan.migration_id)
                            && c.migrations
                                .get(&plan.migration_id)
                                .is_some_and(|transfer| transfer.role
                                    == super::migration::Role::Destination
                                    && transfer.plan == *plan)
                            && r.job
                                .vm
                                .as_ref()
                                .is_some_and(|vm| vm.vm_id == plan.target_vm_id
                                    && vm.owner == plan.target_owner
                                    && vm.guest.is_none()),
                        "invalid incoming staging create"
                    );
                }
                JobExtension::Recover(spec) => {
                    let original = c
                        .operations
                        .get(&spec.operation_id)
                        .context("recovery target missing")?;
                    ensure!(
                        r.job.kind == JobKind::RecoverOperation
                            && r.public.operation_id != spec.operation_id
                            && original.public.owner == r.public.owner
                            && original.public.vm_id == r.public.vm_id,
                        "invalid recovery binding"
                    );
                    if spec.kind == RecoveryKind::AcknowledgeFailedStart {
                        ensure!(
                            c.binding.managed.is_some()
                                && original.public.kind == JobKind::Start
                                && original.job.owner == r.job.owner
                                && original.job.vm == r.job.vm,
                            "failed-start recovery differs from the exact managed operation"
                        );
                    }
                }
            }
        }
        if c.binding.managed.is_some()
            && r.public.kind == JobKind::Start
            && r.public.status == OperationStatus::Cancelled
        {
            ensure!(c.operations.values().any(|recovery| recovery.public.status == OperationStatus::Verified
                && matches!(recovery.job.extension.as_deref(), Some(JobExtension::Recover(spec))
                    if spec.operation_id == r.public.operation_id && spec.kind == RecoveryKind::AcknowledgeFailedStart)),
                "managed start cancellation lacks a verified acknowledgment");
        }
    }
    for (id, spec) in &c.enrollments {
        ensure!(c.vms.get(id).is_some_and(|vm|vm.owner==spec.plan.vm.owner)
            && c.operations.values().any(|r|r.public.kind==JobKind::EnrollVm && r.public.vm_id==Some(*id) && r.public.status==OperationStatus::Verified
                && matches!(r.job.extension.as_deref(),Some(JobExtension::Enroll(s)) if s.plan.plan_id==spec.plan.plan_id)),"enrollment ownership proof missing");
    }
    if !c.migrations.is_empty() {
        ensure!(c.schema >= 6, "migration records require schema6");
    }
    super::migration::validate_records(&c.migrations, &c.binding.environment)?;
    Ok(())
}

pub fn validate_journal(j: &super::host::Journal) -> anyhow::Result<()> {
    use super::host::{Phase, Step};
    use anyhow::{Context, ensure};
    for r in j.jobs.values() {
        if let Some(extension) = r.job.extension.as_deref() {
            ensure!(j.schema >= 6, "extended helper operation requires schema6");
            match extension {
                JobExtension::Grow(spec) => ensure!(
                    r.job.kind == JobKind::GrowVm && spec.valid(),
                    "invalid helper growth payload"
                ),
                JobExtension::Enroll(spec) => ensure!(
                    r.job.kind == JobKind::EnrollVm
                        && r.job
                            .vm
                            .as_ref()
                            .is_some_and(|vm| vm.vm_id == spec.plan.vm.vm_id
                                && vm.owner == spec.plan.vm.owner),
                    "invalid helper enrollment payload"
                ),
                JobExtension::IncomingMigration(plan) => {
                    plan.validate()?;
                    ensure!(
                        r.job.kind == JobKind::Create
                            && plan.destination.environment == j.binding.environment
                            && r.job.operation_id
                                == super::migration_transport::target_create_id(&plan.migration_id)
                            && r.job
                                .vm
                                .as_ref()
                                .is_some_and(|vm| vm.vm_id == plan.target_vm_id
                                    && vm.owner == plan.target_owner
                                    && vm.guest.is_none()),
                        "invalid helper incoming staging create"
                    );
                }
                JobExtension::Recover(spec) => {
                    let original = j
                        .jobs
                        .get(&spec.operation_id)
                        .context("helper recovery target missing")?;
                    ensure!(
                        r.job.kind == JobKind::RecoverOperation
                            && r.job.operation_id != spec.operation_id
                            && original.job.owner == r.job.owner,
                        "invalid helper recovery binding"
                    );
                    if spec.kind == RecoveryKind::AcknowledgeFailedStart {
                        ensure!(
                            j.binding.managed.is_some()
                                && original.job.kind == JobKind::Start
                                && original.job.vm == r.job.vm,
                            "helper failed-start recovery differs from the exact managed operation"
                        );
                    }
                }
            }
            if r.job.kind == JobKind::GrowVm
                && r.status == super::protocol::OperationStatus::Cancelled
            {
                ensure!(j.jobs.values().any(|p|p.steps.iter().any(|s|s.step==Step::Recover && s.phase==Phase::Verified)
                    && matches!(p.job.extension.as_deref(),Some(JobExtension::Recover(s)) if s.operation_id==r.job.operation_id && s.kind==RecoveryKind::AbortPreparation)),"growth cancellation lacks verified abort receipt");
            }
        }
        if j.binding.managed.is_some()
            && r.job.kind == JobKind::Start
            && r.status == super::protocol::OperationStatus::Cancelled
        {
            ensure!(j.jobs.values().any(|recovery| recovery.steps.iter().any(|step| step.step == Step::Recover && step.phase == Phase::Verified)
                && matches!(recovery.job.extension.as_deref(), Some(JobExtension::Recover(spec))
                    if spec.operation_id == r.job.operation_id && spec.kind == RecoveryKind::AcknowledgeFailedStart)),
                "helper managed start cancellation lacks a verified acknowledgment");
        }
    }
    for (id, r) in &j.migrations {
        r.plan.validate()?;
        let (vm, owner) = if r.role == super::migration::Role::Source {
            (r.plan.source_vm_id, &r.plan.source_owner)
        } else {
            (r.plan.target_vm_id, &r.plan.target_owner)
        };
        ensure!(
            j.schema >= 6
                && id == &r.plan.migration_id
                && r.plan.peer(r.role).environment == j.binding.environment
                && r.machine_before.vm_id == vm
                && &r.machine_before.owner == owner,
            "helper migration binding changed"
        );
        if let Some(binding) = &j.binding.physical_trial {
            ensure!(
                binding.scope.features.contains("migration")
                    && binding.scope.permits(owner, Some(vm)),
                "helper migration escapes immutable physical scope"
            );
        }
    }
    Ok(())
}
