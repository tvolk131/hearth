use pve_dev::control::{
    host::{HostBinding, validate as legacy_validate},
    managed::{
        GuestNetwork, ManagedBinding, ManagedConfig, ManagedImage, ManagedScope, UBUNTU_2404_SHA256,
    },
    protocol::{
        Guest, ImportedVm, OperationStatus, Request, ResourceLimits, ResourcePolicy, Role, Vm,
        VmAction, VmState,
    },
    store::{Binding, Store, disk_charge, validate_scope},
    transport::{self, Network, ServerConfig},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
const ENV: &str = "abcdef0123456789abcdef0123456789";
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe";
fn imported(id: u16, owner: &str, state: VmState, label: &str) -> ImportedVm {
    ImportedVm {
        vm: Vm {
            vm_id: id,
            owner: owner.into(),
            name: format!("dev-{owner}"),
            disk_mib: 1310720,
            memory_mib: 32768,
            min_memory_mib: 12288,
            cores: 8,
            state,
            guest: None,
        },
        label: label.into(),
        observed_at: 1794500000,
        configuration_sha256: "b".repeat(64),
        reason: "Imported without changing guest/config/storage; legacy mutation not qualified"
            .into(),
    }
}
fn managed() -> ManagedBinding {
    ManagedBinding {
        adopted_vms: Vec::new(),
        settled_job_pins: BTreeSet::new(),
        pool_guid: "123".into(),
        scope: ManagedScope {
            node: "pve1".into(),
            pool: "rpool".into(),
            root_dataset: format!("rpool/pve-dev-managed-{ENV}"),
            root_guid: "456".into(),
            machine_id_sha256: "a".repeat(64),
            storage_prefix: "managed-abcdef01-".into(),
            owners: BTreeSet::from([
                "alex".into(),
                "frank".into(),
                "heidi".into(),
                "empty".into(),
            ]),
            vm_ids: BTreeSet::from([9201, 9202]),
            capacity_mib: 32768,
            min_memory_capacity_mib: 4096,
            memory_capacity_mib: 8192,
            cores_capacity: 8,
            limits: ResourceLimits {
                disk_mib: 16384,
                budget_mib: 24576,
                min_memory_mib: 4096,
                memory_mib: 8192,
                cores: 8,
            },
        },
        admin_user: "alex".into(),
        admin_policy: policy(),
        admin_only_mutations: true,
        guest_network: GuestNetwork {
            bridge: "vmbr1".into(),
            subnet: "10.10.0.0/24".into(),
            gateway: "10.10.0.1".parse().unwrap(),
            dns: "1.1.1.1".parse().unwrap(),
            addresses: BTreeMap::from([
                (9201, "10.10.0.201".parse().unwrap()),
                (9202, "10.10.0.202".parse().unwrap()),
            ]),
        },
        cpu: "host".into(),
        images: vec![ManagedImage {
            path: format!("/var/lib/pve-dev-images/{UBUNTU_2404_SHA256}.qcow2").into(),
            image_sha256: UBUNTU_2404_SHA256.into(),
            label: "Ubuntu24.04LTS".into(),
            os_version: "24.04".into(),
            default_user: "ubuntu".into(),
            min_disk_mib: 4096,
        }],
        imported_vms: vec![
            imported(101, "alex", VmState::Running, "Existing VM (imported)"),
            imported(
                103,
                "heidi",
                VmState::Stopped,
                "Stopped HDD recovery (imported)",
            ),
        ],
    }
}
fn policy() -> ResourcePolicy {
    ResourcePolicy {
        budget_mib: 8192,
        min_memory_budget_mib: 512,
        max_memory_budget_mib: 2048,
        max_cores: 4,
        max_vms: 2,
    }
}
fn binding(m: ManagedBinding) -> Binding {
    Binding {
        environment: ENV.into(),
        backend: "managed".into(),
        disk_capacity_mib: m.scope.capacity_mib,
        min_memory_capacity_mib: m.scope.min_memory_capacity_mib,
        guest_images: m.images.iter().map(|i| i.image_sha256.clone()).collect(),
        physical_trial: None,
        managed: Some(m),
    }
}
fn helper(b: &Binding) -> HostBinding {
    HostBinding {
        environment: b.environment.clone(),
        backend: b.backend.clone(),
        pool_guid: b.managed.as_ref().unwrap().pool_guid.clone(),
        physical_trial: None,
        managed: b.managed.clone(),
    }
}
fn complete(s: &Store) {
    for r in s.pending().unwrap() {
        s.progress(
            &r.public.operation_id,
            OperationStatus::Verified,
            "unit-test-catalog-transition-only",
            None,
        )
        .unwrap();
    }
}
fn create(owner: &str, id: &str) -> Request {
    Request::CreateVm {
        request_id: id.into(),
        name: format!("new-{id}"),
        disk_mib: 4096,
        memory_mib: 1024,
        min_memory_mib: 512,
        cores: 2,
        owner: Some(owner.into()),
        guest: Some(Guest {
            image_sha256: UBUNTU_2404_SHA256.into(),
            ssh_public_key: KEY.into(),
        }),
    }
}
fn user(name: &str) -> Request {
    Request::CreateUser {
        request_id: format!("user-{name}"),
        user_id: name.into(),
        budget_mib: 8192,
        min_memory_budget_mib: 512,
        max_memory_budget_mib: 2048,
        max_cores: 4,
        max_vms: 2,
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    peer: String,
    binding: Binding,
}
fn fixture(m: ManagedBinding) -> Fixture {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("controller.redb");
    let peer = iroh::SecretKey::generate().public().to_string();
    let b = binding(m);
    let store = Store::create(&path, b.clone(), &peer).unwrap();
    complete(&store);
    Fixture {
        _dir: d,
        path,
        store,
        peer,
        binding: b,
    }
}
fn call(s: &Store, p: &str, v: Value) -> Result<Value, pve_dev::control::protocol::ApiError> {
    s.handle(p, serde_json::from_value(v).unwrap())
}

#[test]
fn normal_accounts_and_imports_are_retained_without_adoption_or_new_reservations() {
    let f = fixture(managed());
    let c = f.store.read().unwrap();
    assert_eq!(c.schema, 7);
    assert_eq!(c.users.len(), 1);
    assert!(c.users.contains_key("alex"));
    assert!(!c.users.contains_key("erin"));
    assert_eq!(c.imported_vms.len(), 2);
    assert!(c.vms.is_empty());
    assert_eq!(c.operations.len(), 1);
    assert!(c.operations.values().all(|r| r.job.vm.is_none()));
    let d = f.store.handle(&f.peer, Request::Dashboard {}).unwrap();
    assert_eq!(d["budget"]["allocated_mib"], 0);
    assert_eq!(d["budget"]["remaining_mib"], 8192);
    assert_eq!(d["imported_vms"][1]["vm"]["state"], "stopped");
    assert!(
        d["imported_vms"][1]["label"]
            .as_str()
            .unwrap()
            .contains("recovery")
    );
    assert_eq!(d["vms"], json!([]));
    f.store.handle(&f.peer, user("frank")).unwrap();
    complete(&f.store);
    let accepted = f.store.handle(&f.peer, create("frank", "one")).unwrap();
    assert_eq!(accepted["vm_id"], 9201);
    let before = f.store.read().unwrap();
    assert_eq!(before.imported_vms, c.imported_vms);
    let charge = disk_charge(&before.vms[&9201]);
    let member = iroh::SecretKey::generate().public().to_string();
    f.store
        .handle(
            &f.peer,
            Request::EnrollDevice {
                user_id: "frank".into(),
                endpoint_id: member.clone(),
                label: "member".into(),
            },
        )
        .unwrap();
    let d = f.store.handle(&member, Request::Dashboard {}).unwrap();
    assert_eq!(d["budget"]["allocated_mib"], charge);
    assert!(d.get("imported_vms").is_none());
    assert_eq!(d["vms"].as_array().unwrap().len(), 1);
    drop(f.store);
    let reopened = Store::open(&f.path, &f.binding).unwrap();
    let after = reopened.read().unwrap();
    assert_eq!(after.imported_vms, c.imported_vms);
    assert_eq!(after.operations.len(), before.operations.len());
    assert_eq!(after.vms.len(), 1);
}

#[test]
fn imported_records_never_authorize_power_delete_resize_enroll_or_account_changes() {
    let f = fixture(managed());
    let before = f.store.read().unwrap();
    for kind in [
        VmAction::Start,
        VmAction::Shutdown,
        VmAction::Stop,
        VmAction::Delete,
    ] {
        assert!(
            f.store
                .handle(
                    &f.peer,
                    Request::VmAction {
                        request_id: format!("import-{kind:?}"),
                        vm_id: 103,
                        kind
                    }
                )
                .is_err()
        );
    }
    for action in [
        json!({"action":"plan_vm_growth","vm_id":103,"target_disk_mib":1400000}),
        json!({"action":"plan_vm_shrink","vm_id":103,"target_disk_mib":4096}),
        json!({"action":"plan_vm_enrollment","vm_id":103,"owner":"heidi"}),
        json!({"action":"enroll_vm","request_id":"adopt","plan_id":"unknown","vm_id":103,"owner":"heidi","confirm_enrollment":true}),
    ] {
        assert!(call(&f.store, &f.peer, action).is_err());
    }
    assert!(
        f.store
            .change(|c| {
                c.imported_vms[0].vm.state = VmState::Stopped;
                Ok(())
            })
            .is_err()
    );
    assert!(
        f.store
            .change(|c| {
                c.vms.insert(103, c.imported_vms[1].vm.clone());
                Ok(())
            })
            .is_err()
    );
    let after = f.store.read().unwrap();
    assert_eq!(after.imported_vms, before.imported_vms);
    assert_eq!(after.operations.len(), before.operations.len());
    assert_eq!(after.users.len(), 1);
    // An external owner label does not require or reserve a managed account.
    f.store.handle(&f.peer, user("heidi")).unwrap();
    complete(&f.store);
    f.store
        .handle(
            &f.peer,
            Request::DeleteUser {
                request_id: "empty-delete".into(),
                user_id: "heidi".into(),
                expected_revision: 0,
            },
        )
        .unwrap();
    complete(&f.store);
    assert_eq!(f.store.read().unwrap().imported_vms, before.imported_vms);
}

#[test]
fn admin_only_profile_enforces_members_and_legacy_owner_mode_can_be_explicitly_selected() {
    for admin_only in [true, false] {
        let mut m = managed();
        m.admin_only_mutations = admin_only;
        let f = fixture(m);
        f.store.handle(&f.peer, user("frank")).unwrap();
        complete(&f.store);
        let member = iroh::SecretKey::generate().public().to_string();
        f.store
            .handle(
                &f.peer,
                Request::EnrollDevice {
                    user_id: "frank".into(),
                    endpoint_id: member.clone(),
                    label: "member".into(),
                },
            )
            .unwrap();
        assert!(f.store.handle(&member, Request::Whoami {}).is_ok());
        assert!(
            f.store
                .handle(&member, Request::ListImageCatalog {})
                .is_ok()
        );
        assert_eq!(
            f.store.handle(&member, user("empty")).unwrap_err().code,
            "forbidden"
        );
        let result = f.store.handle(&member, create("frank", "member"));
        assert_eq!(result.is_ok(), !admin_only);
        assert!(
            f.store
                .handle(&member, create("alex", "cross-account"))
                .is_err()
        );
        assert!(f.store.handle("unknown", Request::Dashboard {}).is_err());
    }
}

#[test]
fn catalog_and_helper_image_allowlist_refuse_unapproved_or_unbootable_profiles() {
    let f = fixture(managed());
    let catalog = f
        .store
        .handle(&f.peer, Request::ListImageCatalog {})
        .unwrap();
    assert_eq!(
        catalog,
        json!([{"image_sha256":UBUNTU_2404_SHA256,"label":"Ubuntu24.04LTS","os_version":"24.04","default_user":"ubuntu","min_disk_mib":4096}])
    );
    assert_eq!(
        f.store.handle(&f.peer, Request::ListImages {}).unwrap(),
        json!([UBUNTU_2404_SHA256])
    );
    for change in ["unknown-image", "small", "blank"] {
        let mut r = serde_json::to_value(create("alex", change)).unwrap();
        match change {
            "unknown-image" => r["guest"]["image_sha256"] = json!("f".repeat(64)),
            "small" => r["disk_mib"] = json!(2048),
            _ => {
                r.as_object_mut().unwrap().remove("guest");
            }
        }
        assert_eq!(call(&f.store, &f.peer, r).unwrap_err().code, "invalid");
    }
    assert!(f.store.read().unwrap().vms.is_empty());
    let accepted = f.store.handle(&f.peer, create("alex", "valid")).unwrap();
    let c = f.store.read().unwrap();
    let job = &c.operations[accepted["operation_id"].as_str().unwrap()].job;
    let h = helper(&f.binding);
    h.validate_job(job).unwrap();
    assert!(legacy_validate(job).is_err());
    let mut bad = job.clone();
    bad.vm
        .as_mut()
        .unwrap()
        .guest
        .as_mut()
        .unwrap()
        .image_sha256 = "f".repeat(64);
    assert!(h.validate_job(&bad).is_err());
    let mut bad = job.clone();
    bad.owner.role = Role::Member;
    assert!(h.validate_job(&bad).is_err());
    for id in [101, 103, 9001, 9199, 9299] {
        let mut bad = job.clone();
        bad.vm.as_mut().unwrap().vm_id = id;
        assert!(h.validate_job(&bad).is_err());
    }
    for kind in [
        "create",
        "grow_vm",
        "shrink_vm",
        "enroll_vm",
        "recover_operation",
        "migration",
        "exec",
        "reboot_host",
    ] {
        let mut bad = job.clone();
        bad.kind = kind.into();
        assert!(h.validate_job(&bad).is_err());
    }
}

#[test]
fn immutable_machine_scope_detects_identity_marker_cap_and_config_drift() {
    let m = managed();
    m.validate(ENV).unwrap();
    let cap = (m.scope.capacity_mib * 1024 * 1024).to_string();
    let root = BTreeMap::from([
        ("type".into(), "filesystem".into()),
        ("guid".into(), m.scope.root_guid.clone()),
        ("org.pve-dev:managed".into(), ENV.into()),
        ("quota".into(), cap.clone()),
        ("reservation".into(), cap),
        ("mountpoint".into(), "none".into()),
    ]);
    let verify =
        |r: &BTreeMap<String, String>| m.verify_identity(ENV, "pve1", &"a".repeat(64), "123", r);
    verify(&root).unwrap();
    for key in root.keys() {
        let mut r = root.clone();
        r.insert(key.clone(), "changed".into());
        assert!(verify(&r).is_err());
    }
    assert!(
        m.verify_identity(ENV, "pve2", &"a".repeat(64), "123", &root)
            .is_err()
    );
    assert!(
        m.verify_identity(ENV, "pve1", &"c".repeat(64), "123", &root)
            .is_err()
    );
    assert!(
        m.verify_identity(ENV, "pve1", &"a".repeat(64), "124", &root)
            .is_err()
    );
    let f = fixture(m);
    let h = helper(&f.binding);
    assert!(f.binding.attested_by(&h));
    let mut foreign = h.clone();
    foreign.managed.as_mut().unwrap().scope.root_guid = "999".into();
    assert!(!f.binding.attested_by(&foreign));
    let mut changed = f.binding.clone();
    changed.managed.as_mut().unwrap().guest_network.bridge = "vmbr9".into();
    drop(f.store);
    assert!(Store::open(&f.path, &changed).is_err());
}

#[test]
fn namespace_and_network_validation_prevent_path_or_address_escape() {
    let m = managed();
    for root in [
        "rpool",
        "rpool/data",
        "rpool/data/dev-heidi",
        "rpool/pve-dev-managed-../data",
        "rpool/pve-dev-trial-abcdef0123456789abcdef0123456789",
    ] {
        let mut bad = m.clone();
        bad.scope.root_dataset = root.into();
        assert!(bad.validate(ENV).is_err());
    }
    for owner in ["../heidi", "a/b", "-root", "root;id"] {
        let mut bad = m.clone();
        bad.scope.owners.insert(owner.into());
        assert!(bad.validate(ENV).is_err());
    }
    assert_eq!(
        m.guest_network.ipconfig(9201).unwrap(),
        "ip=10.10.0.201/24,gw=10.10.0.1"
    );
    assert!(m.guest_network.ipconfig(103).is_err());
    for ip in [
        "10.10.0.1",
        "10.10.0.0",
        "10.10.0.255",
        "192.168.0.201",
        "10.10.0.202",
    ] {
        let mut bad = m.clone();
        bad.guest_network
            .addresses
            .insert(9201, ip.parse().unwrap());
        assert!(bad.validate(ENV).is_err());
    }
    let mut bad = m.clone();
    bad.guest_network.addresses.remove(&9202);
    assert!(bad.validate(ENV).is_err());
    let mut bad = m.clone();
    bad.scope.vm_ids.insert(103);
    assert!(bad.validate(ENV).is_err());
    let mut bad = m.clone();
    bad.images[0].path = "/etc/shadow".into();
    assert!(bad.validate(ENV).is_err());
    let mut bad = m.clone();
    bad.images[0].default_user = "dev".into();
    assert!(bad.validate(ENV).is_err());
    let f = fixture(m);
    assert!(f.store.handle(&f.peer, user("not-allowed")).is_err());
    let mut c = f.store.read().unwrap();
    c.imported_vms.clear();
    assert!(validate_scope(&c).is_err());
}

#[test]
fn host_scoped_103_records_remain_distinct_and_cannot_expand_mutable_id_allowlist() {
    let pve1 = managed();
    let mut pve2 = pve1.clone();
    pve2.scope.node = "pve2".into();
    pve2.scope.machine_id_sha256 = "c".repeat(64);
    pve2.pool_guid = "789".into();
    pve2.imported_vms = vec![imported(
        103,
        "heidi",
        VmState::Running,
        "Active SSD VM (imported)",
    )];
    pve2.validate(ENV).unwrap();
    let first = fixture(pve1);
    let second = fixture(pve2);
    assert!(!first.binding.attested_by(&helper(&second.binding)));
    assert_eq!(
        first.store.read().unwrap().imported_vms[1].vm.state,
        VmState::Stopped
    );
    assert_eq!(
        second.store.read().unwrap().imported_vms[0].vm.state,
        VmState::Running
    );
    let mut bad = managed();
    bad.imported_vms[0].vm.vm_id = 9201;
    assert!(bad.validate(ENV).is_err());
}

#[test]
fn scope_compute_limits_and_vmid_exhaustion_include_pending_new_vms_only() {
    let mut m = managed();
    m.scope.memory_capacity_mib = 2048;
    m.scope.limits.memory_mib = 2048;
    m.scope.limits.min_memory_mib = 2048;
    m.scope.cores_capacity = 4;
    m.scope.limits.cores = 4;
    m.scope.min_memory_capacity_mib = 2048;
    let f = fixture(m);
    f.store.handle(&f.peer, user("frank")).unwrap();
    complete(&f.store);
    f.store.handle(&f.peer, create("alex", "first")).unwrap();
    f.store.handle(&f.peer, create("frank", "second")).unwrap();
    assert!(f.store.handle(&f.peer, create("alex", "third")).is_err());
    let c = f.store.read().unwrap();
    assert_eq!(c.vms.keys().copied().collect::<Vec<_>>(), vec![9201, 9202]);
    assert_eq!(c.imported_vms.len(), 2);
    let mut extreme = serde_json::to_value(create("alex", "extreme")).unwrap();
    extreme["memory_mib"] = json!(u64::MAX);
    assert!(call(&f.store, &f.peer, extreme).is_err());
}

#[test]
fn persistent_public_transport_keeps_enrolled_identity_and_config_pins_without_network_access() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("controller");
    let b = binding(managed());
    let peer = iroh::SecretKey::generate().public().to_string();
    let config = ServerConfig {
        schema: 1,
        binding: b.clone(),
        host_socket: d.path().join("helper.sock"),
        network: Network::Public,
        port: 0,
        migration_network: None,
    };
    transport::initialize(&dir, &config, &peer).unwrap();
    let (_, s, key) = transport::open(&dir).unwrap();
    assert!(s.authorized(&peer));
    let identity = key.public().to_string();
    drop(s);
    let (_, s, key) = transport::open(&dir).unwrap();
    assert_eq!(identity, key.public().to_string());
    assert_eq!(
        s.read().unwrap().imported_vms,
        b.managed.unwrap().imported_vms
    );
}

#[test]
fn missing_managed_fields_preserve_old_serialized_defaults_and_untrusted_config_is_rejected() {
    let old:Binding=serde_json::from_value(json!({"environment":ENV,"backend":"simulated","disk_capacity_mib":2048,"min_memory_capacity_mib":1024})).unwrap();
    assert!(old.managed.is_none());
    assert!(old.physical_trial.is_none());
    assert!(serde_json::to_value(old).unwrap().get("managed").is_none());
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("root.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&ManagedConfig {
            environment: ENV.into(),
            binding: managed(),
        })
        .unwrap(),
    )
    .unwrap();
    assert!(ManagedConfig::read(&file).is_err());
    let mut encoded = serde_json::to_value(managed()).unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .remove("admin_only_mutations");
    let defaulted: ManagedBinding = serde_json::from_value(encoded).unwrap();
    assert!(defaulted.admin_only_mutations);
}

#[test]
fn reviewed_failed_start_cancellation_retains_allocation_and_exact_retry_identity() {
    use pve_dev::control::vm_change::{JobExtension, RecoveryKind};
    let f = fixture(managed());
    let vm = f
        .store
        .handle(&f.peer, create("alex", "failed-start-vm"))
        .unwrap();
    complete(&f.store);
    let start = Request::VmAction {
        request_id: "first-start".into(),
        vm_id: 9201,
        kind: VmAction::Start,
    };
    let accepted = f.store.handle(&f.peer, start.clone()).unwrap();
    let id = accepted["operation_id"].as_str().unwrap();
    f.store
        .progress(
            id,
            OperationStatus::Blocked,
            "start",
            Some("start failed: QEMU exited with code 1".into()),
        )
        .unwrap();
    let before = f.store.handle(&f.peer, Request::Dashboard {}).unwrap();
    assert!(
        f.store
            .change(|catalog| {
                catalog.operations.get_mut(id).unwrap().public.status = OperationStatus::Cancelled;
                Ok(())
            })
            .is_err(),
        "a blocked managed start cannot be cancelled without a verified root decision"
    );
    let review = "a".repeat(64);
    for kind in [
        RecoveryKind::ResumeVerifiedEffect,
        RecoveryKind::AbortPreparation,
    ] {
        assert!(
            f.store
                .queue_reviewed_recovery(&f.peer, "wrong-kind", id, &review, kind, true)
                .is_err()
        );
    }
    assert!(
        f.store
            .queue_reviewed_recovery(
                &f.peer,
                "unconfirmed",
                id,
                &review,
                RecoveryKind::AcknowledgeFailedStart,
                false
            )
            .is_err()
    );
    let request = |request_id| {
        f.store.queue_reviewed_recovery(
            &f.peer,
            request_id,
            id,
            &review,
            RecoveryKind::AcknowledgeFailedStart,
            true,
        )
    };
    let recovered = request("ack-start").unwrap();
    assert_eq!(request("ack-start").unwrap(), recovered);
    assert_eq!(
        f.store
            .queue_reviewed_recovery(
                &f.peer,
                "ack-start",
                id,
                &"b".repeat(64),
                RecoveryKind::AcknowledgeFailedStart,
                true
            )
            .unwrap_err()
            .code,
        "conflict"
    );
    let recovery_id = recovered["operation_id"].as_str().unwrap();
    let c = f.store.read().unwrap();
    let recovery = &c.operations[recovery_id].job;
    helper(&f.binding).validate_job(recovery).unwrap();
    assert!(
        matches!(recovery.extension.as_deref(),Some(JobExtension::Recover(s)) if s.kind==RecoveryKind::AcknowledgeFailedStart)
    );
    let mut imported = recovery.clone();
    imported.vm.as_mut().unwrap().vm_id = 103;
    assert!(helper(&f.binding).validate_job(&imported).is_err());
    assert!(
        f.store
            .change(|catalog| {
                catalog
                    .operations
                    .get_mut(recovery_id)
                    .unwrap()
                    .job
                    .vm
                    .as_mut()
                    .unwrap()
                    .name = "replacement".into();
                Ok(())
            })
            .is_err(),
        "a recovery cannot replace the original frozen VM profile"
    );
    // This catalog-only test substitutes a verified helper decision. The root
    // adapter's receipt/identity gates are tested separately; no effect runs.
    f.store
        .progress(recovery_id, OperationStatus::Verified, "complete", None)
        .unwrap();
    let after = f.store.handle(&f.peer, Request::Dashboard {}).unwrap();
    assert_eq!(after["budget"], before["budget"]);
    assert_eq!(after["imported_vms"], before["imported_vms"]);
    let c = f.store.read().unwrap();
    assert_eq!(c.vms[&9201].state, VmState::Stopped);
    assert_eq!(c.operations[id].public.status, OperationStatus::Cancelled);
    assert!(
        c.operations[id]
            .public
            .error
            .as_ref()
            .unwrap()
            .contains("QEMU")
    );
    assert_eq!(
        f.store.handle(&f.peer, start).unwrap()["operation_id"],
        accepted["operation_id"]
    );
    let retried = request("ack-start").unwrap();
    assert_eq!(retried["operation_id"], recovered["operation_id"]);
    assert_eq!(retried["status"], "verified");
    assert!(request("another-ack").is_err());
    let next = f
        .store
        .handle(
            &f.peer,
            Request::VmAction {
                request_id: "new-start".into(),
                vm_id: 9201,
                kind: VmAction::Start,
            },
        )
        .unwrap();
    assert_ne!(next["operation_id"], accepted["operation_id"]);
    assert_eq!(vm["vm_id"], 9201);
    assert_eq!(f.store.read().unwrap().vms.len(), 1);
}
