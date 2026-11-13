//! Root-pinned persistent management. Imported observations never confer ownership.
use super::{
    host::{LabImage, valid_environment, valid_label},
    physical_trial::{TrialScope, decimal_guid},
    protocol::{ImageCatalogEntry, ImportedVm, ResourceLimits, ResourcePolicy, Role},
    store::Job,
};
use crate::control::JobKind;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::Ipv4Addr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub const UBUNTU_2404_SHA256: &str =
    "04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedScope {
    pub node: String,
    pub pool: String,
    pub root_dataset: String,
    pub root_guid: String,
    pub machine_id_sha256: String,
    pub storage_prefix: String,
    pub owners: BTreeSet<String>,
    pub vm_ids: BTreeSet<u16>,
    pub capacity_mib: u64,
    pub min_memory_capacity_mib: u64,
    pub memory_capacity_mib: u64,
    pub cores_capacity: u16,
    pub limits: ResourceLimits,
}
impl ManagedScope {
    pub fn control_dataset(&self) -> String {
        format!("{}/control", self.root_dataset)
    }
    pub fn permits(&self, owner: &str, vm_id: Option<u16>) -> bool {
        self.owners.contains(owner) && vm_id.is_none_or(|id| self.vm_ids.contains(&id))
    }
    pub fn validate(&self, environment: &str) -> Result<()> {
        ensure!(
            valid_environment(environment),
            "invalid managed environment"
        );
        ensure!(
            valid_label(&self.node, 63) && valid_label(&self.pool, 32),
            "invalid managed node/pool"
        );
        ensure!(
            self.root_dataset == format!("{}/pve-dev-managed-{environment}", self.pool),
            "managed root must be a new environment-named namespace"
        );
        ensure!(
            decimal_guid(&self.root_guid)
                && super::protocol::valid_image_digest(&self.machine_id_sha256),
            "invalid managed root or machine identity"
        );
        ensure!(
            self.storage_prefix == format!("managed-{}-", &environment[..8]),
            "invalid managed storage prefix"
        );
        ensure!(
            !self.owners.is_empty()
                && self.owners.len() <= 64
                && self.owners.iter().all(|o| valid_label(o, 24)),
            "invalid managed account allowlist"
        );
        ensure!(
            !self.vm_ids.is_empty() && self.vm_ids.iter().all(|id| (9201..=9299).contains(id)),
            "managed VMIDs must be explicit within 9201..9299"
        );
        ensure!(
            (8192..=4 * 1024 * 1024).contains(&self.capacity_mib)
                && self.limits.valid()
                && self.limits.budget_mib <= self.capacity_mib,
            "invalid managed storage/profile ceilings"
        );
        ensure!(
            (256..=1024 * 1024).contains(&self.min_memory_capacity_mib)
                && self.min_memory_capacity_mib <= self.memory_capacity_mib
                && self.memory_capacity_mib <= 1024 * 1024
                && self.limits.memory_mib <= self.memory_capacity_mib
                && (1..=256).contains(&self.cores_capacity)
                && self.limits.cores <= self.cores_capacity,
            "invalid managed compute ceilings"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuestNetwork {
    pub bridge: String,
    pub subnet: String,
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
    #[serde(deserialize_with = "deserialize_addresses")]
    pub addresses: BTreeMap<u16, Ipv4Addr>,
}

// Internally tagged helper requests deserialize through serde's buffered
// content representation, which does not coerce JSON object keys to integers.
// Decode canonical VMID strings explicitly in both config and request frames.
fn deserialize_addresses<'de, D>(deserializer: D) -> Result<BTreeMap<u16, Ipv4Addr>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Addresses;
    impl<'de> serde::de::Visitor<'de> for Addresses {
        type Value = BTreeMap<u16, Ipv4Addr>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unique canonical VMID strings mapped to IPv4 addresses")
        }
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            use serde::de::Error;
            let mut result = BTreeMap::new();
            while let Some((key, address)) = map.next_entry::<String, Ipv4Addr>()? {
                let vmid = key.parse::<u16>().map_err(A::Error::custom)?;
                if key != vmid.to_string() || result.insert(vmid, address).is_some() {
                    return Err(A::Error::custom(
                        "noncanonical or duplicate VMID address key",
                    ));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Addresses)
}
impl GuestNetwork {
    pub fn validate(&self, vm_ids: &BTreeSet<u16>) -> Result<()> {
        ensure!(
            valid_label(&self.bridge, 15),
            "invalid existing bridge name"
        );
        let (base, bits) = self
            .subnet
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("IPv4 subnet CIDR required"))?;
        let base = base.parse::<Ipv4Addr>()?;
        let bits = bits.parse::<u8>()?;
        ensure!((8..=30).contains(&bits), "unsupported guest subnet prefix");
        let mask = u32::MAX << (32 - bits);
        let network = u32::from(base);
        let broadcast = network | !mask;
        ensure!(
            network & mask == network,
            "subnet must use its canonical network address"
        );
        let usable = |ip: Ipv4Addr| {
            let n = u32::from(ip);
            n & mask == network
                && n != network
                && n != broadcast
                && !ip.is_loopback()
                && !ip.is_multicast()
                && !ip.is_unspecified()
        };
        ensure!(usable(self.gateway), "gateway outside usable subnet");
        ensure!(
            !self.dns.is_unspecified() && !self.dns.is_multicast(),
            "invalid DNS address"
        );
        ensure!(
            self.addresses.keys().copied().collect::<BTreeSet<_>>() == *vm_ids
                && self
                    .addresses
                    .values()
                    .all(|ip| usable(*ip) && *ip != self.gateway)
                && self.addresses.values().collect::<BTreeSet<_>>().len() == self.addresses.len(),
            "VM address allocation must be unique, usable and exactly cover the allowlist"
        );
        Ok(())
    }
    pub fn ipconfig(&self, vm_id: u16) -> Result<String> {
        let ip = self
            .addresses
            .get(&vm_id)
            .ok_or_else(|| anyhow::anyhow!("VMID has no pinned managed address"))?;
        let bits = self
            .subnet
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("subnet missing prefix"))?
            .1;
        Ok(format!("ip={ip}/{bits},gw={}", self.gateway))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedImage {
    pub path: PathBuf,
    pub image_sha256: String,
    pub label: String,
    pub os_version: String,
    pub default_user: String,
    pub min_disk_mib: u64,
}
impl ManagedImage {
    pub fn catalog(&self) -> ImageCatalogEntry {
        ImageCatalogEntry {
            image_sha256: self.image_sha256.clone(),
            label: self.label.clone(),
            os_version: self.os_version.clone(),
            default_user: self.default_user.clone(),
            min_disk_mib: self.min_disk_mib,
        }
    }
    pub fn lab_image(&self) -> LabImage {
        LabImage {
            path: self.path.clone(),
            sha256: self.image_sha256.clone(),
        }
    }
}
fn admin_only_default() -> bool {
    true
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedBinding {
    pub pool_guid: String,
    pub scope: ManagedScope,
    pub admin_user: String,
    pub admin_policy: ResourcePolicy,
    #[serde(default = "admin_only_default")]
    pub admin_only_mutations: bool,
    pub guest_network: GuestNetwork,
    pub cpu: String,
    pub images: Vec<ManagedImage>,
    #[serde(default)]
    pub imported_vms: Vec<ImportedVm>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adopted_vms: Vec<super::adoption::AdoptedVm>,
    /// Offline operator pins for immutable terminal jobs whose adopted profile
    /// predates a reviewed resource change. Never authority for a new effect.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub settled_job_pins: BTreeSet<String>,
}
impl ManagedBinding {
    pub fn adopted(&self, vm_id: u16) -> Option<&super::adoption::AdoptedVm> {
        self.adopted_vms.iter().find(|v| v.vm.vm_id == vm_id)
    }
    pub fn permits_vm(&self, owner: &str, vm_id: u16) -> bool {
        self.scope.permits(owner, Some(vm_id))
            || self.adopted(vm_id).is_some_and(|v| v.vm.owner == owner)
    }
    pub fn validate(&self, environment: &str) -> Result<()> {
        self.scope.validate(environment)?;
        ensure!(
            decimal_guid(&self.pool_guid) && self.scope.owners.contains(&self.admin_user),
            "invalid managed pool/admin pin"
        );
        ensure!(
            self.admin_policy.valid_with_limits(self.scope.limits)
                && self.admin_policy.budget_mib <= self.scope.capacity_mib
                && self.admin_policy.min_memory_budget_mib <= self.scope.min_memory_capacity_mib,
            "invalid bootstrap admin policy"
        );
        self.guest_network.validate(&self.scope.vm_ids)?;
        ensure!(
            matches!(self.cpu.as_str(), "host" | "x86-64-v2-AES" | "x86-64-v3"),
            "unsupported hardware CPU profile"
        );
        ensure!(
            !self.images.is_empty() && self.images.len() <= 8,
            "managed image catalog must contain 1..8 images"
        );
        let mut digests = BTreeSet::new();
        for i in &self.images {
            ensure!(
                super::protocol::valid_image_digest(&i.image_sha256)
                    && digests.insert(&i.image_sha256)
                    && i.path
                        == Path::new(&format!("/var/lib/pve-dev-images/{}.qcow2", i.image_sha256)),
                "invalid managed content-addressed image allowlist"
            );
            ensure!(
                !i.label.is_empty()
                    && i.label.len() <= 80
                    && !i.label.chars().any(char::is_control)
                    && !i.os_version.is_empty()
                    && i.os_version.len() <= 32
                    && !i.os_version.chars().any(char::is_control)
                    && valid_label(&i.default_user, 32)
                    && (4096..=self.scope.limits.disk_mib).contains(&i.min_disk_mib),
                "invalid image catalog metadata"
            );
            ensure!(
                i.image_sha256 != UBUNTU_2404_SHA256
                    || (i.default_user == "ubuntu" && i.os_version == "24.04"),
                "Canonical Ubuntu image metadata differs from its approved provenance"
            );
        }
        let mut ids = BTreeSet::new();
        ensure!(self.imported_vms.len() <= 128, "too many imported records");
        for row in &self.imported_vms {
            row.validate()?;
            ensure!(
                !self.scope.vm_ids.contains(&row.vm.vm_id)
                    && !(9001..=9299).contains(&row.vm.vm_id)
                    && ids.insert(row.vm.vm_id),
                "imported VMID overlaps new resources or duplicate import"
            );
        }
        ensure!(
            self.settled_job_pins.len() <= 4096
                && self
                    .settled_job_pins
                    .iter()
                    .all(|p| super::protocol::valid_image_digest(p)),
            "invalid settled job fingerprint pins"
        );
        let mut adopted_ids = BTreeSet::new();
        let mut adopted_datasets = BTreeSet::new();
        let mut adopted_storages = BTreeSet::new();
        for row in &self.adopted_vms {
            row.validate(self)?;
            ensure!(
                adopted_ids.insert(row.vm.vm_id)
                    && adopted_datasets.insert(&row.dataset)
                    && adopted_storages.insert(&row.storage),
                "duplicate adopted allocation"
            );
        }
        Ok(())
    }
    pub fn validate_retained_job(&self, job: &Job, terminal: bool) -> Result<()> {
        if terminal
            && self
                .settled_job_pins
                .contains(&super::host::fingerprint(job)?)
        {
            return Ok(());
        }
        self.validate_job(job)
    }

    pub fn validate_job(&self, job: &Job) -> Result<()> {
        if let Some(row) = job.vm.as_ref().and_then(|v| self.adopted(v.vm_id)) {
            return row.validate_job(job, self);
        }
        let failed_start_recovery = job.kind == JobKind::RecoverOperation
            && job.vm.is_some()
            && matches!(job.extension.as_deref(), Some(super::vm_change::JobExtension::Recover(spec))
                if spec.kind == super::vm_change::RecoveryKind::AcknowledgeFailedStart);
        ensure!(
            (matches!(
                job.kind,
                JobKind::EnsureUser
                    | JobKind::UpdateUserPolicy
                    | JobKind::DeleteUser
                    | JobKind::CreateGuest
                    | JobKind::Start
                    | JobKind::Shutdown
                    | JobKind::Stop
                    | JobKind::Delete
            ) && job.extension.is_none()
                || failed_start_recovery)
                && job.disk_shrink.is_none(),
            "operation is not qualified for managed backend"
        );
        ensure!(
            self.scope
                .permits(&job.owner.user_id, job.vm.as_ref().map(|v| v.vm_id))
                && ((job.owner.user_id == self.admin_user) == (job.owner.role == Role::Admin)),
            "managed owner, role or VMID escapes root scope"
        );
        ensure!(
            job.owner.policy().valid_with_limits(self.scope.limits),
            "managed owner policy exceeds root profile limits"
        );
        if let Some(vm) = &job.vm {
            ensure!(
                vm.owner == job.owner.user_id
                    && !self.imported_vms.iter().any(|r| r.vm.vm_id == vm.vm_id),
                "imported records have no mutation authority"
            );
            let guest = vm.guest.as_ref().ok_or_else(|| {
                anyhow::anyhow!("managed resources require bootable guest provenance")
            })?;
            let image = self
                .images
                .iter()
                .find(|i| i.image_sha256 == guest.image_sha256)
                .ok_or_else(|| anyhow::anyhow!("image not approved by root"))?;
            ensure!(
                vm.disk_mib >= image.min_disk_mib,
                "disk below approved image minimum"
            );
        }
        Ok(())
    }
    pub fn verify_identity(
        &self,
        environment: &str,
        hostname: &str,
        machine: &str,
        pool_guid: &str,
        root: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.validate(environment)?;
        ensure!(
            hostname == self.scope.node
                && machine == self.scope.machine_id_sha256
                && pool_guid == self.pool_guid,
            "managed machine/node/pool identity changed"
        );
        let cap = (self.scope.capacity_mib * super::host::MIB).to_string();
        for (key, expected) in [
            ("type", "filesystem"),
            ("guid", self.scope.root_guid.as_str()),
            ("org.pve-dev:managed", environment),
            ("quota", cap.as_str()),
            ("reservation", cap.as_str()),
            ("mountpoint", "none"),
        ] {
            ensure!(
                root.get(key).is_some_and(|v| v == expected),
                "managed root {key} changed or missing"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedConfig {
    pub environment: String,
    pub binding: ManagedBinding,
}
impl ManagedConfig {
    pub fn read(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "managed config must have an absolute symlink-free path"
        );
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file()
                && metadata.len() <= 65536
                && metadata.nlink() == 1
                && metadata.mode() & 0o077 == 0,
            "managed config must be a small private single-link regular file"
        );
        for parent in path.ancestors() {
            let m = fs::metadata(parent)?;
            ensure!(
                m.uid() == 0 && m.mode() & 0o022 == 0,
                "managed config ancestry must be root-owned and not writable by group/others"
            );
        }
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.binding.validate(&config.environment)?;
        Ok(config)
    }
}

/// Shared path construction only. Each backend still validates its own authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Namespace {
    pub node: String,
    pub pool: String,
    pub root_dataset: String,
    pub storage_prefix: String,
    pub features: BTreeSet<String>,
}
impl Namespace {
    pub fn control_dataset(&self) -> String {
        format!("{}/control", self.root_dataset)
    }
}
impl From<&TrialScope> for Namespace {
    fn from(s: &TrialScope) -> Self {
        Self {
            node: s.node.clone(),
            pool: s.pool.clone(),
            root_dataset: s.root_dataset.clone(),
            storage_prefix: s.storage_prefix.clone(),
            features: s.features.clone(),
        }
    }
}
impl From<&ManagedScope> for Namespace {
    fn from(s: &ManagedScope) -> Self {
        Self {
            node: s.node.clone(),
            pool: s.pool.clone(),
            root_dataset: s.root_dataset.clone(),
            storage_prefix: s.storage_prefix.clone(),
            features: BTreeSet::new(),
        }
    }
}
