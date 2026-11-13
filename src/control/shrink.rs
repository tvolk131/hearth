//! The allocation contract for a two-stage, offline lab disk shrink.
use crate::control::JobKind;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShrinkSpec {
    pub previous_disk_mib: u64,
    pub target_disk_mib: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_partition_uuid: Option<String>,
}

impl ShrinkSpec {
    pub fn valid(&self) -> bool {
        (2048..super::vm_change::MAX_DISK_MIB).contains(&self.target_disk_mib)
            && self.target_disk_mib < self.previous_disk_mib
            && self.previous_disk_mib <= super::vm_change::MAX_DISK_MIB
            && self
                .root_partition_uuid
                .as_deref()
                .is_none_or(super::vm_change::valid_partition_uuid)
    }

    // Reserve the full source-sized work disk and the target-sized disk at
    // once, including thick-volume overhead and room for filesystem metadata.
    // This is deliberately conservative, independent of compression/utilization.
    pub fn workspace_mib(&self) -> u64 {
        self.previous_disk_mib
            + self.previous_disk_mib.div_ceil(20)
            + self.target_disk_mib
            + self.target_disk_mib.div_ceil(20)
            + 64
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingShrink {
    pub operation_id: String,
    pub vm_id: u16,
    pub owner: String,
    pub spec: ShrinkSpec,
    pub prepared: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShrinkPlan {
    pub vm_id: u16,
    pub expected_disk_mib: u64,
    pub target_disk_mib: u64,
    pub workspace_mib: u64,
    pub requires_stopped_vm: bool,
    pub recovery_copy_retained: bool,
    pub filesystem_checked_during_preparation: bool,
}

pub fn validate_catalog(c: &super::store::Catalog) -> anyhow::Result<()> {
    use super::protocol::OperationStatus;
    use anyhow::{Context, ensure};
    let mut owners = std::collections::BTreeSet::new();
    for (id, hold) in &c.disk_shrinks {
        ensure!(
            c.schema >= 5
                && hold.vm_id == *id
                && hold.spec.valid()
                && hold.spec.preparation_id.is_none()
                && owners.insert(&hold.owner),
            "invalid/duplicate shrink capacity hold"
        );
        let record = c
            .operations
            .get(&hold.operation_id)
            .context("shrink operation missing")?;
        ensure!(
            record.job.kind == JobKind::ShrinkVm
                && record.public.vm_id == Some(*id)
                && record.job.disk_shrink.as_deref() == Some(&hold.spec)
                && record.public.owner == hold.owner
                && hold.prepared == (record.public.status == OperationStatus::Verified),
            "shrink hold/proof mismatch"
        );
        let vm = c.vms.get(id).context("shrink VM missing")?;
        ensure!(
            vm.owner == hold.owner
                && vm.disk_mib
                    == if hold.prepared {
                        hold.spec.target_disk_mib
                    } else {
                        hold.spec.previous_disk_mib
                    },
            "shrink catalog profile mismatch"
        );
    }
    for record in c
        .operations
        .values()
        .filter(|r| r.job.kind == JobKind::ShrinkVm)
    {
        ensure!(
            c.schema >= 5 && record.job.disk_shrink.as_ref().is_some_and(|s| s.valid()),
            "shrink operation requires schema5 and a valid payload"
        );
        ensure!(
            c.disk_shrinks
                .values()
                .any(|s| s.operation_id == record.public.operation_id)
                || (record.public.status == OperationStatus::Cancelled && c.operations.values().any(|r| {
                    r.public.status == OperationStatus::Verified && matches!(r.job.extension.as_deref(),Some(super::vm_change::JobExtension::Recover(s)) if s.operation_id == record.public.operation_id && s.kind == super::vm_change::RecoveryKind::AbortPreparation)
                }))
                || c.operations
                    .values()
                    .any(|r| r.job.kind == JobKind::FinishVmShrink
                        && r.public.status == OperationStatus::Verified
                        && r.job
                            .disk_shrink
                            .as_ref()
                            .and_then(|s| s.preparation_id.as_deref())
                            == Some(&record.public.operation_id)),
            "shrink workspace claim vanished without verified finish"
        );
    }
    Ok(())
}

pub fn validate_journal(j: &super::host::Journal) -> anyhow::Result<()> {
    use super::host::{Phase, Step};
    use anyhow::{Context, ensure};
    let mut owners = std::collections::BTreeSet::new();
    for (id, hold) in &j.disk_shrinks {
        ensure!(
            j.schema >= 5
                && hold.vm_id == *id
                && hold.spec.valid()
                && hold.spec.preparation_id.is_none()
                && owners.insert(&hold.owner),
            "invalid/duplicate helper shrink hold"
        );
        let record = j
            .jobs
            .get(&hold.operation_id)
            .context("helper shrink operation missing")?;
        ensure!(
            record.job.kind == JobKind::ShrinkVm
                && record
                    .job
                    .vm
                    .as_ref()
                    .is_some_and(|v| v.vm_id == *id && v.owner == hold.owner)
                && record.job.disk_shrink.as_deref() == Some(&hold.spec)
                && hold.prepared
                    == record
                        .steps
                        .iter()
                        .any(|s| s.step == Step::ShrinkUnlock && s.phase == Phase::Verified),
            "helper shrink hold/proof mismatch"
        );
    }
    for record in j
        .jobs
        .values()
        .filter(|r| r.job.kind == JobKind::ShrinkVm && !r.steps.is_empty())
    {
        ensure!(
            j.schema >= 5 && record.job.disk_shrink.as_ref().is_some_and(|s| s.valid()),
            "helper shrink requires schema5"
        );
        ensure!(
            j.disk_shrinks
                .values()
                .any(|s| s.operation_id == record.job.operation_id)
                || (record.status == super::protocol::OperationStatus::Cancelled && j.jobs.values().any(|r| {
                    matches!(r.job.extension.as_deref(),Some(super::vm_change::JobExtension::Recover(s)) if s.operation_id == record.job.operation_id && s.kind == super::vm_change::RecoveryKind::AbortPreparation)
                    && r.steps.iter().any(|s|s.step==Step::Recover && s.phase==Phase::Verified)
                }))
                || j.jobs.values().any(|r| r.job.kind == JobKind::FinishVmShrink
                    && r.job
                        .disk_shrink
                        .as_ref()
                        .and_then(|s| s.preparation_id.as_deref())
                        == Some(&record.job.operation_id)
                    && r.steps
                        .iter()
                        .any(|s| s.step == Step::ShrinkFinish && s.phase == Phase::Verified)),
            "helper shrink hold lost without verified recovery removal"
        );
    }
    Ok(())
}
