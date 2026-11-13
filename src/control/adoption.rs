//! Explicit root-approved bindings for existing allocations. Adoption changes
//! controller records only; it does not relocate disks or rewrite guest config.
use super::{
    host,
    managed::ManagedBinding,
    protocol::{Vm, VmState},
    store::Job,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdoptedVm {
    pub vm: Vm,
    pub configuration: Value,
    pub storage: String,
    pub storage_configuration: Value,
    pub dataset: String,
    pub dataset_properties: BTreeMap<String, String>,
    pub root_properties: BTreeMap<String, String>,
    pub cloud_init_properties: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub external_files_sha256: BTreeMap<String, String>,
}

pub const DATASET_PROPERTIES: &[&str] = &[
    "type",
    "guid",
    "quota",
    "reservation",
    "refreservation",
    "origin",
];
pub const VOLUME_PROPERTIES: &[&str] = &[
    "type",
    "guid",
    "volsize",
    "refreservation",
    "reservation",
    "origin",
    "readonly",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StorageAllocation {
    pub id: String,
    pub owner: String,
    pub dataset: String,
    pub storage: String,
    pub budget_mib: u64,
    pub vm_ids: Vec<u16>,
    pub accepts_new_vms: bool,
    pub min_memory_budget_mib: u64,
    pub max_memory_budget_mib: u64,
    pub max_cores: u16,
    pub max_vms: u16,
}

pub fn allocations(catalog: &super::store::Catalog) -> Vec<StorageAllocation> {
    let Some(binding) = &catalog.binding.managed else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for user in catalog
        .users
        .values()
        .filter(|u| u.state != super::protocol::AccountState::Deleted)
    {
        let storage = format!("{}{}", binding.scope.storage_prefix, user.user_id);
        result.push(StorageAllocation {
            id: storage.clone(),
            owner: user.user_id.clone(),
            dataset: format!("{}/{}", binding.scope.control_dataset(), user.user_id),
            storage,
            budget_mib: user.budget_mib,
            vm_ids: catalog
                .vms
                .values()
                .filter(|v| {
                    v.owner == user.user_id
                        && v.state != VmState::Deleted
                        && binding.adopted(v.vm_id).is_none()
                })
                .map(|v| v.vm_id)
                .collect(),
            accepts_new_vms: true,
            min_memory_budget_mib: user.min_memory_budget_mib,
            max_memory_budget_mib: user.max_memory_budget_mib,
            max_cores: user.max_cores,
            max_vms: user.max_vms,
        });
    }
    for row in &binding.adopted_vms {
        result.push(StorageAllocation {
            id: row.storage.clone(),
            owner: row.vm.owner.clone(),
            dataset: row.dataset.clone(),
            storage: row.storage.clone(),
            budget_mib: row.dataset_properties["quota"].parse::<u64>().unwrap_or(0) / host::MIB,
            vm_ids: vec![row.vm.vm_id],
            accepts_new_vms: false,
            min_memory_budget_mib: row.vm.min_memory_mib,
            max_memory_budget_mib: row.vm.memory_mib,
            max_cores: row.vm.cores,
            max_vms: 1,
        });
    }
    result
}

impl AdoptedVm {
    pub fn machine_record(&self, environment: &str) -> host::MachineRecord {
        host::MachineRecord {
            vm_id: self.vm.vm_id,
            owner: self.vm.owner.clone(),
            profile: host::Profile {
                name: self.vm.name.clone(),
                disk_mib: self.vm.disk_mib,
                memory_mib: self.vm.memory_mib,
                min_memory_mib: self.vm.min_memory_mib,
                cores: self.vm.cores,
                guest: None,
            },
            create_operation: super::store::digest(&("adopt-existing-vm/1", environment, self)),
            delete_operation: None,
            state: host::MachineState::Present,
            identities: BTreeMap::from([(
                "vm".into(),
                serde_json::json!({
                    "smbios1": self.configuration["smbios1"], "vmgenid": self.configuration["vmgenid"]
                }),
            )]),
            pinned_config: Some(self.configuration.clone()),
        }
    }
    pub fn root_volume(&self) -> String {
        format!("{}/vm-{}-disk-0", self.dataset, self.vm.vm_id)
    }
    pub fn cloud_init_volume(&self) -> String {
        format!("{}/vm-{}-cloudinit", self.dataset, self.vm.vm_id)
    }
    pub fn validate(&self, binding: &ManagedBinding) -> Result<()> {
        ensure!(
            self.vm.owner == binding.admin_user
                && self.vm.guest.is_none()
                && matches!(self.vm.state, VmState::Running | VmState::Stopped)
                && host::valid_label(&self.vm.name, 48)
                && self.vm.disk_mib > 0
                && self.vm.memory_mib >= self.vm.min_memory_mib
                && self.vm.min_memory_mib >= 128
                && self.vm.cores > 0,
            "invalid adopted VM identity/profile"
        );
        ensure!(
            self.vm.vm_id > 0
                && !binding.scope.vm_ids.contains(&self.vm.vm_id)
                && binding
                    .imported_vms
                    .iter()
                    .any(|r| r.vm.vm_id == self.vm.vm_id
                        && r.vm.name == self.vm.name
                        && r.vm.disk_mib == self.vm.disk_mib
                        && r.vm.memory_mib == self.vm.memory_mib
                        && r.vm.min_memory_mib == self.vm.min_memory_mib
                        && r.vm.cores == self.vm.cores),
            "adoption requires matching reviewed existing inventory"
        );
        ensure!(
            host::valid_label(&self.storage, 63)
                && self
                    .dataset
                    .starts_with(&(binding.scope.pool.clone() + "/"))
                && !self.dataset.starts_with(&binding.scope.root_dataset)
                && self.dataset.split('/').all(|p| host::valid_label(p, 128)),
            "invalid or overlapping adopted storage binding"
        );
        for (properties, keys, kind) in [
            (&self.dataset_properties, DATASET_PROPERTIES, "filesystem"),
            (&self.root_properties, VOLUME_PROPERTIES, "volume"),
            (&self.cloud_init_properties, VOLUME_PROPERTIES, "volume"),
        ] {
            ensure!(
                properties.len() == keys.len()
                    && keys.iter().all(|k| properties.contains_key(*k))
                    && properties["type"] == kind
                    && super::physical_trial::decimal_guid(&properties["guid"]),
                "incomplete adopted dataset identity"
            );
        }
        ensure!(
            self.root_properties["volsize"].parse::<u64>()? == self.vm.disk_mib * host::MIB
                && self.root_properties["refreservation"].parse::<u64>()?
                    >= self.vm.disk_mib * host::MIB
                && self.cloud_init_properties["volsize"].parse::<u64>()? == 4 * host::MIB
                && self.cloud_init_properties["refreservation"].parse::<u64>()? >= 4 * host::MIB
                && self.root_properties["readonly"] == "off"
                && self.cloud_init_properties["readonly"] == "off"
                && self.dataset_properties["quota"].parse::<u64>()? >= self.vm.disk_mib * host::MIB
                && self.dataset_properties["reservation"].parse::<u64>()?
                    >= self.vm.disk_mib * host::MIB,
            "adopted volume sizes differ from approved profile"
        );
        ensure!(
            self.storage_configuration["type"] == "zfspool"
                && self.storage_configuration["pool"] == self.dataset
                && self.storage_configuration["storage"] == self.storage,
            "adopted Proxmox storage differs from dataset"
        );
        let config = self
            .configuration
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("adopted configuration is not an object"))?;
        let snippets = config.get("cicustom").and_then(Value::as_str).unwrap_or("");
        let mut expected_files = std::collections::BTreeSet::new();
        if !snippets.is_empty() {
            for entry in snippets.split(',') {
                let (kind, reference) = entry
                    .split_once('=')
                    .context("invalid cloud-init snippet reference")?;
                ensure!(
                    matches!(kind, "user" | "network" | "meta" | "vendor"),
                    "unsupported snippet kind"
                );
                let name = reference
                    .strip_prefix("local:snippets/")
                    .context("only explicit local cloud-init snippets can be adopted")?;
                ensure!(
                    !name.is_empty()
                        && !name.contains('/')
                        && name != "."
                        && name != ".."
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                    "unsafe snippet name"
                );
                expected_files.insert(format!("/var/lib/vz/snippets/{name}"));
            }
        }
        ensure!(
            self.external_files_sha256
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                == expected_files
                && self
                    .external_files_sha256
                    .values()
                    .all(|s| super::protocol::valid_image_digest(s)),
            "cloud-init source files need exact hash pins"
        );
        for (key, expected) in [
            ("memory", self.vm.memory_mib),
            ("balloon", self.vm.min_memory_mib),
            ("cores", u64::from(self.vm.cores)),
        ] {
            let value = config
                .get(key)
                .context("adopted profile property missing")?;
            let text = value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
            ensure!(
                text.strip_prefix("current=")
                    .unwrap_or(&text)
                    .parse::<u64>()?
                    == expected,
                "adopted configuration profile differs: {key}"
            );
        }
        for key in config.keys() {
            ensure!(
                !["args", "hookscript", "hostpci", "usb", "virtiofs"]
                    .iter()
                    .any(|p| key.starts_with(p)),
                "adoption refuses custom execution/passthrough configuration"
            );
            if [
                "scsi", "ide", "sata", "virtio", "efidisk", "tpmstate", "unused",
            ]
            .iter()
            .any(|p| key.starts_with(p))
                && key != "scsihw"
            {
                ensure!(
                    matches!(key.as_str(), "scsi0" | "ide2"),
                    "adoption refuses unbound disk {key}"
                );
            }
        }
        for (key, name) in [
            (
                "scsi0",
                format!("{}:vm-{}-disk-0", self.storage, self.vm.vm_id),
            ),
            (
                "ide2",
                format!("{}:vm-{}-cloudinit", self.storage, self.vm.vm_id),
            ),
        ] {
            ensure!(
                config
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(|s| s.split(',').next())
                    == Some(name.as_str()),
                "adopted disk attachment differs from explicit storage pin"
            );
        }
        ensure!(
            !config.contains_key("lock")
                && config.get("name").and_then(Value::as_str) == Some(self.vm.name.as_str()),
            "locked or mismatched adopted VM"
        );
        Ok(())
    }

    pub fn validate_job(&self, job: &Job, binding: &ManagedBinding) -> Result<()> {
        let vm = job
            .vm
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("adopted job has no VM"))?;
        let mut expected = self.vm.clone();
        expected.state = vm.state.clone();
        let recovery = job.kind == "recover_operation"
            && matches!(job.extension.as_deref(), Some(super::vm_change::JobExtension::Recover(spec))
                if spec.kind == super::vm_change::RecoveryKind::AcknowledgeFailedStart
                    && host::valid_operation_id(&spec.operation_id) && host::valid_operation_id(&spec.inspection_id));
        ensure!(
            (matches!(job.kind.as_str(), "start" | "shutdown" | "stop" | "delete") || recovery)
                && *vm == expected
                && job.owner.user_id == binding.admin_user
                && job.owner.role == super::protocol::Role::Admin
                && job.owner.ready
                && job.owner.state == super::protocol::AccountState::Active
                && (job.extension.is_none() || recovery)
                && job.disk_shrink.is_none()
                && job.policy_change.is_none(),
            "adopted operation differs from its approved resource binding"
        );
        Ok(())
    }
}
