//! Managed-only read observations and admission; no existing-resource effects.
use super::*;

pub(super) async fn verify_initial_imports(backend: &Backend, binding: &HostBinding) -> Result<()> {
    let Some(managed) = &binding.managed else {
        return Ok(());
    };
    for row in &managed.imported_vms {
        let path = format!("/nodes/{}/qemu/{}", managed.scope.node, row.vm.vm_id);
        let config = backend.api_get(&format!("{path}/config"), &[]).await?;
        let status = backend
            .api_get(&format!("{path}/status/current"), &[])
            .await?;
        verify_import(row, &config, &status)?;
    }
    Ok(())
}

pub(super) fn verify_import(
    row: &super::super::protocol::ImportedVm,
    config: &Value,
    status: &Value,
) -> Result<()> {
    ensure!(
        super::super::store::digest(config) == row.configuration_sha256,
        "imported VM {} current configuration differs from root-approved digest",
        row.vm.vm_id
    );
    let state = match row.vm.state {
        super::super::protocol::VmState::Running => "running",
        super::super::protocol::VmState::Stopped => "stopped",
        _ => bail!("invalid imported state"),
    };
    ensure!(
        status["status"].as_str() == Some(state),
        "imported VM status changed before initialization"
    );
    for (key, expected) in [
        ("name", row.vm.name.clone()),
        ("cores", row.vm.cores.to_string()),
        ("memory", row.vm.memory_mib.to_string()),
        ("balloon", row.vm.min_memory_mib.to_string()),
    ] {
        ensure!(
            config.get(key).is_some_and(|v| {
                let actual = text(v);
                let actual = if key == "memory" {
                    actual.strip_prefix("current=").unwrap_or(&actual)
                } else {
                    &actual
                };
                actual == expected
            }),
            "imported VM display metadata {key} differs from current configuration"
        );
    }
    let disk = config
        .get("scsi0")
        .and_then(Value::as_str)
        .context("imported root scsi0 missing")?;
    ensure!(
        attachment_bytes(disk)? == row.vm.disk_mib * MIB,
        "imported root disk display metadata differs"
    );
    Ok(())
}

pub(super) fn admit_scope(journal: &Journal, context: &Context) -> Result<()> {
    let Some(m) = &journal.binding.managed else {
        return Ok(());
    };
    if let Some(row) = m.adopted(context.vm_id()) {
        row.validate_job(&journal.jobs[&context.operation_id].job, m)?;
        return Ok(());
    }
    ensure!(
        !journal
            .imported_vms
            .iter()
            .any(|r| r.vm.vm_id == context.vm_id()),
        "imported records are immutable"
    );
    if matches!(context.kind.as_str(), "ensure_user" | "update_user_policy") {
        let target = context
            .policy_change
            .as_ref()
            .map_or_else(|| context.owner_policy.clone(), |p| p.target.policy());
        let mut disk = target.budget_mib;
        let mut memory = target.min_memory_budget_mib;
        for owner in journal
            .owners
            .values()
            .filter(|o| o.user_id != context.owner && o.state != AccountState::Deleted)
        {
            disk = disk
                .checked_add(owner.budget_mib)
                .context("owner capacity overflow")?;
            memory = memory
                .checked_add(Host::owner_policy(journal, owner)?.min_memory_budget_mib)
                .context("memory capacity overflow")?;
        }
        ensure!(
            disk <= m.scope.capacity_mib && memory <= m.scope.min_memory_capacity_mib,
            "managed account admission ceiling exhausted"
        );
    }
    if context.kind == "create_guest" {
        let vm = context.vm()?;
        let active = journal
            .machines
            .values()
            .filter(|v| {
                v.vm_id != vm.vm_id
                    && v.state != MachineState::Deleted
                    && m.adopted(v.vm_id).is_none()
            })
            .collect::<Vec<_>>();
        ensure!(
            active.iter().map(|v| v.profile.memory_mib).sum::<u64>() + vm.memory_mib
                <= m.scope.memory_capacity_mib
                && active
                    .iter()
                    .map(|v| u64::from(v.profile.cores))
                    .sum::<u64>()
                    + u64::from(vm.cores)
                    <= u64::from(m.scope.cores_capacity),
            "managed compute ceiling exhausted"
        );
        let owned = active
            .iter()
            .filter(|v| v.owner == context.owner)
            .collect::<Vec<_>>();
        let disk = owned
            .iter()
            .map(|v| {
                let p = &v.profile;
                p.disk_mib + 4 + 8.max(p.disk_mib.div_ceil(20))
            })
            .sum::<u64>();
        let p = &context.owner_policy;
        ensure!(
            disk + super::super::store::disk_charge(vm) <= p.budget_mib
                && owned.iter().map(|v| v.profile.min_memory_mib).sum::<u64>() + vm.min_memory_mib
                    <= p.min_memory_budget_mib
                && owned.iter().map(|v| v.profile.memory_mib).sum::<u64>() + vm.memory_mib
                    <= p.max_memory_budget_mib
                && owned
                    .iter()
                    .map(|v| u64::from(v.profile.cores))
                    .sum::<u64>()
                    + u64::from(vm.cores)
                    <= u64::from(p.max_cores)
                && owned.len() < usize::from(p.max_vms),
            "managed helper owner policy exhausted"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        managed::ManagedConfig,
        protocol::{ResourcePolicy, VmState},
        store::{Job, digest},
    };

    fn config() -> ManagedConfig {
        serde_json::from_str(include_str!("../../../tests/fixtures/managed-config.json")).unwrap()
    }
    fn binding() -> HostBinding {
        let c = config();
        HostBinding {
            environment: c.environment,
            backend: "managed".into(),
            pool_guid: c.binding.pool_guid.clone(),
            physical_trial: None,
            managed: Some(c.binding),
        }
    }
    fn context() -> Context {
        let c = config();
        let policy = c.binding.admin_policy.clone();
        Context {environment:c.environment,operation_id:"managed-create".into(),kind:"create_guest".into(),owner:c.binding.admin_user.clone(),budget_mib:policy.budget_mib,vm:Some(serde_json::from_value(json!({"vm_id":9201,"owner":c.binding.admin_user,"name":"managed-ubuntu","disk_mib":4096,"memory_mib":1024,"min_memory_mib":512,"cores":2,"state":"creating","guest":{"image_sha256":crate::control::managed::UBUNTU_2404_SHA256,"ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}})).unwrap()),boot_id:"boot-one".into(),policy_revision:0,owner_policy:policy,policy_change:None,scope:Some(Namespace::from(&c.binding.scope)),managed:Some(c.binding),disk_shrink:None,extension:None}
    }
    fn journal() -> Journal {
        let b = binding();
        Journal {
            schema: 7,
            imported_vms: b.managed.as_ref().unwrap().imported_vms.clone(),
            binding: b,
            sequence: 0,
            control: None,
            owners: BTreeMap::new(),
            machines: BTreeMap::new(),
            jobs: BTreeMap::new(),
            disk_shrinks: BTreeMap::new(),
            migrations: BTreeMap::new(),
        }
    }
    #[test]
    fn managed_paths_hardware_network_and_cloudinit_remain_bound_to_new_resources() {
        let c = context();
        let m = c.managed.as_ref().unwrap();
        let cfg = c
            .expected_config()
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(cfg["kvm"], "1");
        assert_eq!(cfg["cpu"], "host");
        assert!(cfg["net0"].ends_with("bridge=vmbr1"));
        assert_eq!(cfg["ipconfig0"], "ip=10.0.0.201/24,gw=10.0.0.1");
        assert_eq!(cfg["nameserver"], "1.1.1.1");
        assert!(!cfg.contains_key("ciuser"));
        assert_eq!(m.images[0].default_user, "ubuntu");
        assert_eq!(c.vm_path(), "/nodes/pve1/qemu/9201");
        assert_eq!(
            c.dataset(),
            format!("{}/control/erin", m.scope.root_dataset)
        );
        assert!(
            c.root_volume()
                .starts_with(&(m.scope.root_dataset.clone() + "/control/"))
        );
        assert_eq!(c.storage(), "managed-01234567-erin");
        assert!(c.cloud_init_volume().ends_with("/vm-9201-cloudinit"));
        assert_eq!(c.delete_arguments()[0], ("purge", "0".into()));
        let mut missing = c.clone();
        missing.vm.as_mut().unwrap().vm_id = 103;
        assert!(missing.expected_config().is_err());
        let backend = Backend::Lab(Box::new(LabBackend {
            command_timeout: Duration::from_secs(1),
            images: vec![],
            scope: Some(Namespace::from(&m.scope)),
        }));
        assert_eq!(backend.pool(), "rpool");
        assert_eq!(backend.control_dataset(), c.control_dataset());
    }
    #[test]
    fn managed_generic_helper_validation_accepts_only_its_explicit_new_ids() {
        let c = context();
        let owner = super::super::super::protocol::User {
            user_id: c.owner.clone(),
            role: Role::Admin,
            budget_mib: c.owner_policy.budget_mib,
            min_memory_budget_mib: c.owner_policy.min_memory_budget_mib,
            max_memory_budget_mib: c.owner_policy.max_memory_budget_mib,
            max_cores: c.owner_policy.max_cores,
            max_vms: c.owner_policy.max_vms,
            ready: true,
            policy_revision: 0,
            state: AccountState::Active,
        };
        let job = Job {
            operation_id: c.operation_id.clone(),
            kind: c.kind.clone(),
            owner,
            vm: c.vm.clone(),
            policy_change: None,
            disk_shrink: None,
            extension: None,
        };
        let b = binding();
        b.validate_job(&job).unwrap();
        validate_with_limits(
            &job,
            b.limits(),
            b.expanded(),
            b.managed.as_ref().map(|m| &m.scope.vm_ids),
        )
        .unwrap();
        assert!(validate(&job).is_err());
        for id in [101, 103, 9001, 9200, 9299] {
            let mut bad = job.clone();
            bad.vm.as_mut().unwrap().vm_id = id;
            assert!(b.validate_job(&bad).is_err());
        }
        let mut bad = job.clone();
        bad.owner.max_memory_budget_mib = u64::MAX;
        assert!(b.validate_job(&bad).is_err());
    }
    #[test]
    fn managed_binding_round_trips_through_actual_tagged_helper_request_frames() {
        let b = binding();
        let c = context();
        let owner = super::super::super::protocol::User {
            user_id: c.owner.clone(),
            role: super::super::super::protocol::Role::Admin,
            budget_mib: c.owner_policy.budget_mib,
            min_memory_budget_mib: c.owner_policy.min_memory_budget_mib,
            max_memory_budget_mib: c.owner_policy.max_memory_budget_mib,
            max_cores: c.owner_policy.max_cores,
            max_vms: c.owner_policy.max_vms,
            ready: true,
            policy_revision: 0,
            state: AccountState::Active,
        };
        let job = Job {
            operation_id: "wire-test".into(),
            kind: "ensure_user".into(),
            owner,
            vm: None,
            policy_change: None,
            disk_shrink: None,
            extension: None,
        };
        for request in [
            HostRequest::ImportedVms {
                binding: Box::new(b.clone()),
            },
            HostRequest::SubmitBound {
                binding: Box::new(b),
                job,
            },
        ] {
            let bytes = serde_json::to_vec(&request).unwrap();
            let decoded: HostRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        }
        let cfg = config();
        let network = serde_json::to_string(&cfg.binding.guest_network).unwrap();
        for changed in [
            network.replace("\"9201\":", "\"09201\":"),
            network.replace("\"9201\":", "\"70000\":"),
            network.replace(
                "\"9201\":\"10.0.0.201\"",
                "\"9201\":\"10.0.0.201\",\"9201\":\"10.0.0.202\"",
            ),
        ] {
            assert!(
                serde_json::from_str::<crate::control::managed::GuestNetwork>(&changed).is_err()
            );
        }
    }

    #[test]
    fn imports_accept_pve_memory_property_only_with_matching_digest_and_value() {
        let mut cfg = json!({"name":"dev-erin","cores":8,"memory":"current=32768","balloon":12288,
            "scsi0":"old-storage:vm-101-disk-0,size=1280G"});
        let mut row = config().binding.imported_vms.remove(0);
        row.configuration_sha256 = digest(&cfg);
        verify_import(&row, &cfg, &json!({"status":"running"})).unwrap();
        cfg["memory"] = json!("current=16384");
        assert!(verify_import(&row, &cfg, &json!({"status":"running"})).is_err());
        row.configuration_sha256 = digest(&cfg);
        assert!(verify_import(&row, &cfg, &json!({"status":"running"})).is_err());
        cfg["memory"] = json!("current=current=32768");
        row.configuration_sha256 = digest(&cfg);
        assert!(verify_import(&row, &cfg, &json!({"status":"running"})).is_err());
    }

    #[test]
    fn imports_require_current_config_digest_status_and_exact_display_metadata() {
        let cfg = json!({"name":"dev-erin","cores":8,"memory":32768,"balloon":12288,"scsi0":"old-storage:vm-101-disk-0,size=1280G"});
        let mut row = config().binding.imported_vms.remove(0);
        row.configuration_sha256 = digest(&cfg);
        verify_import(&row, &cfg, &json!({"status":"running"})).unwrap();
        assert!(verify_import(&row, &cfg, &json!({"status":"stopped"})).is_err());
        let mut altered = cfg.clone();
        altered["memory"] = json!(16384);
        assert!(verify_import(&row, &altered, &json!({"status":"running"})).is_err());
        for field in ["name", "disk_mib", "cores", "memory_mib", "min_memory_mib"] {
            let mut v = serde_json::to_value(&row).unwrap();
            v["vm"][field] = if field == "name" {
                json!("other-vm")
            } else {
                json!(1)
            };
            let changed = serde_json::from_value(v).unwrap();
            assert!(verify_import(&changed, &cfg, &json!({"status":"running"})).is_err());
        }
        row.vm.state = VmState::Stopped;
        row.label = "Stopped HDD recovery (imported)".into();
        verify_import(&row, &cfg, &json!({"status":"stopped"})).unwrap();
    }
    #[test]
    fn helper_imports_persist_separately_in_redb_and_mutable_history_cannot_overlap() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("journal.redb");
        let db = Database::create(&path).unwrap();
        let j = journal();
        validate_scope(&j).unwrap();
        write_journal(&db, &j).unwrap();
        drop(db);
        let db = Database::open(&path).unwrap();
        let reopened = read_journal(&db).unwrap();
        validate_scope(&reopened).unwrap();
        assert_eq!(reopened.imported_vms, j.imported_vms);
        assert!(reopened.machines.is_empty());
        assert!(reopened.owners.is_empty());
        assert!(reopened.jobs.is_empty());
        let mut changed = reopened.clone();
        changed.imported_vms[0].vm.state = VmState::Stopped;
        assert!(validate_scope(&changed).is_err());
        let mut changed = reopened;
        changed.machines.insert(
            101,
            MachineRecord {
                vm_id: 101,
                owner: "erin".into(),
                profile: Profile {
                    name: "dev-erin".into(),
                    disk_mib: 4096,
                    memory_mib: 1024,
                    min_memory_mib: 512,
                    cores: 2,
                    guest: None,
                },
                create_operation: "pretend-import".into(),
                delete_operation: None,
                state: MachineState::Present,
                identities: BTreeMap::new(),
                pinned_config: None,
            },
        );
        assert!(validate_scope(&changed).is_err());
    }
    #[test]
    fn helper_scope_admission_counts_only_new_resources_and_checks_global_compute_and_reservations()
    {
        let mut j = journal();
        let mut c = context();
        c.kind = "ensure_user".into();
        c.vm = None;
        admit_scope(&j, &c).unwrap();
        c.owner_policy = ResourcePolicy {
            budget_mib: j.binding.managed.as_ref().unwrap().scope.capacity_mib + 1,
            ..c.owner_policy
        };
        assert!(admit_scope(&j, &c).is_err());
        let c = context();
        admit_scope(&j, &c).unwrap();
        let mut p = profile(c.vm().unwrap());
        p.memory_mib = j
            .binding
            .managed
            .as_ref()
            .unwrap()
            .scope
            .memory_capacity_mib;
        j.machines.insert(
            9202,
            MachineRecord {
                vm_id: 9202,
                owner: "frank".into(),
                profile: p,
                create_operation: "new-frank".into(),
                delete_operation: None,
                state: MachineState::Creating,
                identities: BTreeMap::new(),
                pinned_config: None,
            },
        );
        assert!(admit_scope(&j, &c).is_err());
        j.machines.get_mut(&9202).unwrap().state = MachineState::Deleted;
        admit_scope(&j, &c).unwrap();
        let mut imported = c;
        imported.vm.as_mut().unwrap().vm_id = 101;
        assert!(admit_scope(&j, &imported).is_err());
    }
}
