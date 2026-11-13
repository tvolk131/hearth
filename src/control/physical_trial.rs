//! Explicit, bounded physical-host trial scope. This does not adopt existing VMs.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::host::{LabImage, valid_environment, valid_label};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrialScope {
    pub node: String,
    pub pool: String,
    pub root_dataset: String,
    pub root_guid: String,
    pub machine_id_sha256: String,
    pub storage_prefix: String,
    pub owners: BTreeSet<String>,
    pub vm_ids: BTreeSet<u16>,
    pub capacity_mib: u64,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<super::protocol::ResourceLimits>,
}

impl TrialScope {
    pub fn control_dataset(&self) -> String {
        format!("{}/control", self.root_dataset)
    }
    pub fn validate(&self, environment: &str) -> Result<()> {
        ensure!(valid_environment(environment), "invalid trial environment");
        ensure!(
            valid_label(&self.node, 63) && valid_label(&self.pool, 32),
            "invalid trial node/pool name"
        );
        ensure!(
            self.root_dataset == format!("{}/pve-dev-trial-{environment}", self.pool),
            "physical trial must use its dedicated, environment-named dataset"
        );
        ensure!(decimal_guid(&self.root_guid), "invalid trial root GUID");
        ensure!(
            super::protocol::valid_image_digest(&self.machine_id_sha256),
            "invalid machine identity digest"
        );
        ensure!(
            self.storage_prefix == format!("trial-{}-", &environment[..8]),
            "invalid scoped storage prefix"
        );
        ensure!(
            self.owners == BTreeSet::from(["trial-admin".into(), "trial-user".into()]),
            "physical trials support only the dedicated trial-admin and trial-user accounts"
        );
        ensure!(
            !self.vm_ids.is_empty()
                && self.vm_ids.len() <= 4
                && self.vm_ids.iter().all(|id| (9001..=9099).contains(id)),
            "physical trial must allow at most four explicit lab-range VM IDs"
        );
        ensure!(
            self.features.iter().all(|f| matches!(
                f.as_str(),
                "resize" | "recovery" | "enrollment" | "migration"
            )),
            "unknown physical scope feature"
        );
        let maximum = if self.features.is_empty() {
            16384
        } else {
            4 * 1024 * 1024
        };
        ensure!(
            (8192..=maximum).contains(&self.capacity_mib),
            "physical trial storage cap exceeds explicit scope limits"
        );
        if let Some(limits) = self.limits {
            ensure!(
                !self.features.is_empty()
                    && limits.valid()
                    && limits.budget_mib <= self.capacity_mib,
                "invalid physical profile limits"
            );
        }
        Ok(())
    }

    pub fn permits(&self, owner: &str, vm_id: Option<u16>) -> bool {
        self.owners.contains(owner) && vm_id.is_none_or(|id| self.vm_ids.contains(&id))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrialBinding {
    pub pool_guid: String,
    pub scope: TrialScope,
}

impl TrialBinding {
    pub fn validate(&self, environment: &str) -> Result<()> {
        self.scope.validate(environment)?;
        ensure!(decimal_guid(&self.pool_guid), "invalid trial pool GUID");
        Ok(())
    }

    pub fn verify_identity(
        &self,
        environment: &str,
        hostname: &str,
        machine_id_sha256: &str,
        pool_guid: &str,
        root: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.validate(environment)?;
        ensure!(
            hostname == self.scope.node
                && machine_id_sha256 == self.scope.machine_id_sha256
                && pool_guid == self.pool_guid,
            "physical trial machine/node/pool identity changed; refusing"
        );
        let cap = (self.scope.capacity_mib * super::host::MIB).to_string();
        for (key, expected) in [
            ("type", "filesystem"),
            ("guid", self.scope.root_guid.as_str()),
            ("org.pve-dev:trial", environment),
            ("quota", cap.as_str()),
            ("reservation", cap.as_str()),
            ("mountpoint", "none"),
        ] {
            ensure!(
                root.get(key).is_some_and(|value| value == expected),
                "physical trial root {key} changed or missing; refusing"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialConfig {
    pub environment: String,
    pub pool_guid: String,
    pub scope: TrialScope,
    #[serde(default)]
    pub images: Vec<LabImage>,
}

pub fn decimal_guid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
}

impl TrialConfig {
    pub fn binding(&self) -> TrialBinding {
        TrialBinding {
            pool_guid: self.pool_guid.clone(),
            scope: self.scope.clone(),
        }
    }
    pub fn read(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "trial config must have an absolute, symlink-free path"
        );
        let metadata =
            fs::symlink_metadata(path).context("reading physical trial config metadata")?;
        ensure!(
            metadata.is_file() && metadata.len() <= 8192 && metadata.nlink() == 1,
            "trial config must be a small, single-link regular file"
        );
        for ancestor in path.ancestors() {
            let metadata = fs::metadata(ancestor)?;
            ensure!(
                metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
                "trial config and ancestry must be root-owned and not group/world writable"
            );
        }
        ensure!(metadata.mode() & 0o077 == 0, "trial config must be private");
        let config: Self =
            serde_json::from_slice(&fs::read(path)?).context("invalid physical trial config")?;
        config.scope.validate(&config.environment)?;
        ensure!(decimal_guid(&config.pool_guid), "invalid trial pool GUID");
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
            "invalid content-addressed trial image allowlist"
        );
        ensure!(
            config
                .images
                .iter()
                .map(|image| &image.sha256)
                .collect::<BTreeSet<_>>()
                .len()
                == config.images.len(),
            "duplicate trial image digest"
        );
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ENV: &str = "0123456789abcdef0123456789abcdef";
    fn scope() -> TrialScope {
        TrialScope {
            node: "pve1".into(),
            pool: "rpool".into(),
            root_dataset: format!("rpool/pve-dev-trial-{ENV}"),
            root_guid: "42".into(),
            machine_id_sha256: "a".repeat(64),
            storage_prefix: "trial-01234567-".into(),
            owners: BTreeSet::from(["trial-admin".into(), "trial-user".into()]),
            vm_ids: BTreeSet::from([9001, 9002]),
            capacity_mib: 12288,
            features: Default::default(),
            limits: None,
        }
    }
    #[test]
    fn scope_refuses_existing_namespaces_owners_and_vm_ids() {
        let valid = scope();
        valid.validate(ENV).unwrap();
        assert!(valid.permits("trial-user", Some(9001)));
        assert!(!valid.permits("heidi", Some(9001)));
        assert!(!valid.permits("trial-user", Some(103)));
        for root in [
            "rpool",
            "rpool/data",
            "rpool/ROOT",
            "rpool/data/dev-heidi",
            "rpool/pve-dev-trial-other",
        ] {
            let mut altered = valid.clone();
            altered.root_dataset = root.into();
            assert!(altered.validate(ENV).is_err());
        }
        let mut altered = valid.clone();
        altered.owners.insert("heidi".into());
        assert!(altered.validate(ENV).is_err());
        let mut altered = valid.clone();
        altered.vm_ids.insert(103);
        assert!(altered.validate(ENV).is_err());
        let mut altered = valid;
        altered.capacity_mib = 32768;
        assert!(altered.validate(ENV).is_err());
    }
    #[test]
    fn scope_serialization_binds_every_resource_boundary() {
        let original = scope();
        let encoded = serde_json::to_vec(&original).unwrap();
        let roundtrip: TrialScope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(original, roundtrip);
        let mut changed = original.clone();
        changed.root_guid = "43".into();
        assert_ne!(original, changed);
        let mut changed = original.clone();
        changed.machine_id_sha256 = "b".repeat(64);
        assert_ne!(original, changed);
    }
}
