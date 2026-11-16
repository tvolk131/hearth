//! Settling operations that the helper permanently rejected.
//!
//! The helper returns [`HelperErrorCode::Rejected`](super::host::HelperErrorCode)
//! only when `submit` refused a job before journaling it. Its deduplication
//! runs first, so a job the helper ever accepted is never rejected. Nothing
//! can have happened on the host, so the controller may end the operation and
//! release what it held, instead of resubmitting it forever.

use serde_json::json;

use super::{
    JobKind,
    host::{HelperError, HelperErrorCode},
    protocol::{AccountState, ApiError, OperationStatus, VmState},
    store::{Store, failure, now},
};

impl Store {
    /// Handle a failed submission of `operation_id`. `Ok(true)` means a typed
    /// helper rejection ended the queued operation. Any other error, including
    /// every transport failure, leaves it queued for resubmission, and a
    /// rejection of an operation that is no longer queued changes nothing;
    /// both return `Ok(false)`.
    pub fn settle_submit_error(
        &self,
        operation_id: &str,
        error: &anyhow::Error,
    ) -> Result<bool, ApiError> {
        match HelperError::find(error) {
            Some(typed) if typed.code == HelperErrorCode::Rejected => {
                self.reject(operation_id, &typed.message)
            }
            _ => Ok(false),
        }
    }

    /// Mark a queued operation `Failed` with the helper's reason and release
    /// its admission claims. Returns whether it did; operations that are no
    /// longer queued are left unchanged.
    pub fn reject(&self, operation_id: &str, reason: &str) -> Result<bool, ApiError> {
        self.change(|catalog| {
            let record = catalog
                .operations
                .get_mut(operation_id)
                .ok_or_else(|| failure("internal", "operation missing"))?;
            // Only work the helper has never reported on can be rejected.
            if record.public.status != OperationStatus::Queued {
                return Ok(false);
            }
            record.public.status = OperationStatus::Failed;
            record.public.step = "rejected_by_helper".into();
            record.public.error = Some(reason.into());
            if let Some(progress) = &mut record.public.progress {
                progress.updated_at = progress.updated_at.max(now());
                progress.finished_at = Some(progress.updated_at);
                progress.detail = Some("Rejected by the host helper".into());
            }
            let kind = record.public.kind;
            let owner = record.public.owner.clone();
            let vm_id = record.public.vm_id;
            let previous_vm_state = record.job.vm.as_ref().map(|vm| vm.state.clone());
            let frozen_owner_ready = record.job.owner.ready;
            match (kind, vm_id) {
                // A rejected create never reached the host. Keep the VMID as a
                // tombstone (VMIDs are never reused); it holds no claims.
                (JobKind::Create | JobKind::CreateGuest, Some(vm_id)) => {
                    if let Some(vm) = catalog.vms.get_mut(&vm_id) {
                        vm.state = VmState::Deleted;
                    }
                }
                // Enrollment targets an existing unmanaged VM that the helper
                // never recorded; return it to unmanaged.
                (JobKind::EnrollVm, Some(vm_id)) => {
                    catalog.vms.remove(&vm_id);
                }
                (JobKind::Delete, Some(vm_id)) => {
                    if let (Some(vm), Some(state)) =
                        (catalog.vms.get_mut(&vm_id), previous_vm_state)
                    {
                        vm.state = state;
                    }
                }
                (JobKind::GrowVm, Some(vm_id)) => {
                    if catalog
                        .disk_growths
                        .get(&vm_id)
                        .is_some_and(|hold| hold.operation_id == operation_id)
                    {
                        catalog.disk_growths.remove(&vm_id);
                    }
                }
                (JobKind::ShrinkVm, Some(vm_id)) => {
                    if catalog
                        .disk_shrinks
                        .get(&vm_id)
                        .is_some_and(|hold| hold.operation_id == operation_id)
                    {
                        catalog.disk_shrinks.remove(&vm_id);
                    }
                }
                // Accepting the deletion froze the account and revoked its
                // devices. Unfreeze it, but leave revocation in place: undoing
                // a revocation is never automatic.
                (JobKind::DeleteUser, None) => {
                    if let Some(user) = catalog.users.get_mut(&owner)
                        && user.state == AccountState::Deleting
                    {
                        user.state = AccountState::Active;
                        user.ready = frozen_owner_ready;
                    }
                }
                // Lifecycle actions, policy edits (whose capacity hold ends with
                // the operation), shrink finishes and recoveries change no other
                // catalog state at admission.
                _ => {}
            }
            catalog.audit.push(json!({
                "at": now(),
                "event": "operation_rejected_by_helper",
                "operation_id": operation_id,
                "reason": reason,
            }));
            Ok(true)
        })
    }
}
