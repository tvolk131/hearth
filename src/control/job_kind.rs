//! The kinds of operation the controller can queue and the helper can run.
//!
//! Serialized names are the historical wire strings, so JSON written by older
//! builds and read by the macOS app is unchanged.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Blank VM (lab and tests).
    Create,
    /// VM from an approved image with cloud-init.
    CreateGuest,
    Start,
    /// Graceful ACPI shutdown.
    Shutdown,
    /// Forced stop.
    Stop,
    Delete,
    EnsureUser,
    DeleteUser,
    UpdateUserPolicy,
    GrowVm,
    ShrinkVm,
    FinishVmShrink,
    EnrollVm,
    RecoverOperation,
    /// Helper-internal context for a migration step; never a queued job.
    Migration,
    /// Helper-internal context for read-only inspection; never a queued job.
    Inspect,
}

impl JobKind {
    pub const ALL: [JobKind; 16] = [
        JobKind::Create,
        JobKind::CreateGuest,
        JobKind::Start,
        JobKind::Shutdown,
        JobKind::Stop,
        JobKind::Delete,
        JobKind::EnsureUser,
        JobKind::DeleteUser,
        JobKind::UpdateUserPolicy,
        JobKind::GrowVm,
        JobKind::ShrinkVm,
        JobKind::FinishVmShrink,
        JobKind::EnrollVm,
        JobKind::RecoverOperation,
        JobKind::Migration,
        JobKind::Inspect,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            JobKind::Create => "create",
            JobKind::CreateGuest => "create_guest",
            JobKind::Start => "start",
            JobKind::Shutdown => "shutdown",
            JobKind::Stop => "stop",
            JobKind::Delete => "delete",
            JobKind::EnsureUser => "ensure_user",
            JobKind::DeleteUser => "delete_user",
            JobKind::UpdateUserPolicy => "update_user_policy",
            JobKind::GrowVm => "grow_vm",
            JobKind::ShrinkVm => "shrink_vm",
            JobKind::FinishVmShrink => "finish_vm_shrink",
            JobKind::EnrollVm => "enroll_vm",
            JobKind::RecoverOperation => "recover_operation",
            JobKind::Migration => "migration",
            JobKind::Inspect => "inspect",
        }
    }

    /// Power-state changes on an existing VM.
    pub const fn is_lifecycle(self) -> bool {
        matches!(self, JobKind::Start | JobKind::Shutdown | JobKind::Stop)
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        JobKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| anyhow::anyhow!("unknown operation kind {value:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_round_trip_and_match_serde() {
        for kind in JobKind::ALL {
            assert_eq!(kind.as_str().parse::<JobKind>().unwrap(), kind);
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.as_str().into())
            );
        }
        assert!("Create".parse::<JobKind>().is_err());
        assert!(serde_json::from_str::<JobKind>("\"reboot\"").is_err());
    }
}
