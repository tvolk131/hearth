use pve_dev::control::JobKind;
use std::collections::{BTreeMap, BTreeSet};

use iroh::SecretKey;
use pve_dev::control::{
    host::{Host, HostBinding, HostConfig, HostRequest},
    physical_trial::{TrialBinding, TrialScope},
    protocol::{OperationStatus, Request, ResourceLimits, Role, VmAction, VmState},
    store::{Binding, Store},
    transport::{self, Network, ServerConfig},
};
use redb::ReadableTable;
use serde_json::{Value, json};

const ENV: &str = "0123456789abcdef0123456789abcdef";

fn trial() -> TrialBinding {
    TrialBinding {
        pool_guid: "111".into(),
        scope: TrialScope {
            node: "pve2".into(),
            pool: "rpool".into(),
            root_dataset: format!("rpool/pve-dev-trial-{ENV}"),
            root_guid: "222".into(),
            machine_id_sha256: "a".repeat(64),
            storage_prefix: "trial-01234567-".into(),
            owners: BTreeSet::from(["trial-admin".into(), "trial-user".into()]),
            vm_ids: BTreeSet::from([9051, 9053]),
            capacity_mib: 12288,
            features: Default::default(),
            limits: None,
        },
    }
}

fn binding() -> Binding {
    Binding {
        environment: ENV.into(),
        backend: "physical_trial".into(),
        disk_capacity_mib: 12288,
        min_memory_capacity_mib: 1024,
        guest_images: vec![],
        physical_trial: Some(trial()),
        managed: None,
    }
}

fn featured_binding() -> Binding {
    let mut value = binding();
    value.disk_capacity_mib = 65536;
    value.min_memory_capacity_mib = 1536;
    let scope = &mut value.physical_trial.as_mut().unwrap().scope;
    scope.node = "pve1".into();
    scope.vm_ids = BTreeSet::from([9001, 9002, 9003, 9004]);
    scope.capacity_mib = value.disk_capacity_mib;
    scope.features = BTreeSet::from([
        "enrollment".into(),
        "migration".into(),
        "recovery".into(),
        "resize".into(),
    ]);
    scope.limits = Some(ResourceLimits {
        disk_mib: 32768,
        budget_mib: 49152,
        min_memory_mib: 4096,
        memory_mib: 8192,
        cores: 4,
    });
    value
}

#[test]
fn full_featured_server_configuration_initializes_64gib_with_an_admissible_bootstrap() {
    // No helper or endpoint is started. This follows the production init/open
    // filesystem path and independently checks the queued helper payload.
    for tight in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("controller");
        let peer = SecretKey::generate().public().to_string();
        let mut value = featured_binding();
        if tight {
            value.min_memory_capacity_mib = 256;
            value.physical_trial.as_mut().unwrap().scope.limits = Some(ResourceLimits {
                disk_mib: 512,
                budget_mib: 2048,
                min_memory_mib: 128,
                memory_mib: 256,
                cores: 1,
            });
        }
        let config = ServerConfig {
            schema: 1,
            binding: value.clone(),
            host_socket: root.path().join("helper.sock"),
            network: Network::Loopback,
            port: 0,
            migration_network: Some(Network::Public),
        };
        transport::initialize(&directory, &config, &peer).unwrap();
        let (saved, store, _) = transport::open(&directory).unwrap();
        assert_eq!(saved.binding, value);
        assert_eq!(saved.network, Network::Loopback);
        assert_eq!(saved.migration_network, Some(Network::Public));
        let catalog = store.read().unwrap();
        assert_eq!(catalog.schema, 6);
        assert_eq!(catalog.binding.disk_capacity_mib, 65536);
        assert!(catalog.vms.is_empty());
        assert_eq!(catalog.operations.len(), 1);
        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 1);
        let bootstrap = &pending[0];
        assert_eq!(bootstrap.public.status, OperationStatus::Queued);
        assert_eq!(bootstrap.job.kind, JobKind::EnsureUser);
        assert!(bootstrap.job.vm.is_none());
        assert_eq!(bootstrap.job.owner.user_id, "trial-admin");
        assert_eq!(bootstrap.job.owner.role, Role::Admin);
        assert!(!bootstrap.job.owner.ready);
        assert_eq!(bootstrap.job.owner.max_cores, if tight { 1 } else { 4 });
        assert_eq!(
            bootstrap.job.owner.max_memory_budget_mib,
            if tight { 256 } else { 1024 }
        );
        assert_eq!(
            bootstrap.job.owner.min_memory_budget_mib,
            if tight { 128 } else { 256 }
        );
        assert!(
            bootstrap
                .job
                .owner
                .policy()
                .valid_with_limits(value.limits())
        );
        let helper = HostBinding {
            environment: ENV.into(),
            backend: "physical_trial".into(),
            pool_guid: "111".into(),
            physical_trial: value.physical_trial.clone(),
            managed: None,
        };
        helper.validate().unwrap();
        helper.validate_job(&bootstrap.job).unwrap();
        assert!(value.attested_by(&helper));
        let mut foreign = helper;
        foreign.physical_trial.as_mut().unwrap().scope.root_guid = "333".into();
        assert!(!value.attested_by(&foreign));
    }
}

#[test]
fn high_capacity_requires_an_explicit_featured_physical_scope() {
    let root = tempfile::tempdir().unwrap();
    let peer = SecretKey::generate().public().to_string();
    for backend in ["lab", "simulated", "physical_trial"] {
        let mut value = featured_binding();
        value.backend = backend.into();
        if backend == "physical_trial" {
            let scope = &mut value.physical_trial.as_mut().unwrap().scope;
            scope.features.clear();
            scope.limits = None;
        } else {
            value.physical_trial = None;
        }
        let path = root.path().join(format!("{backend}.redb"));
        assert!(
            Store::create(&path, value, &peer).is_err(),
            "accepted legacy {backend} at 64GiB"
        );
        assert!(!path.exists(), "refusal created a catalog for {backend}");
    }
}

#[test]
fn featured_capacity_cannot_exceed_or_differ_from_its_immutable_scope() {
    let root = tempfile::tempdir().unwrap();
    let peer = SecretKey::generate().public().to_string();
    for declared in [32768, 65537] {
        let mut value = featured_binding();
        value.disk_capacity_mib = declared;
        let directory = root.path().join(declared.to_string());
        let config = ServerConfig {
            schema: 1,
            binding: value,
            host_socket: root.path().join("helper.sock"),
            network: Network::Loopback,
            port: 0,
            migration_network: Some(Network::Public),
        };
        assert!(transport::initialize(&directory, &config, &peer).is_err());
        assert!(
            !directory.exists(),
            "scope mismatch wrote initialization state"
        );
    }
}

fn finish(store: &Store) {
    // Controller admission tests; actual helper qualification is separate.
    for record in store.pending().unwrap() {
        store
            .progress(
                &record.public.operation_id,
                OperationStatus::Verified,
                "complete",
                None,
            )
            .unwrap();
    }
}

fn member(request_id: &str, user_id: &str) -> Request {
    Request::CreateUser {
        request_id: request_id.into(),
        user_id: user_id.into(),
        budget_mib: 128,
        min_memory_budget_mib: 256,
        max_memory_budget_mib: 1024,
        max_cores: 8,
        max_vms: 4,
    }
}

#[test]
fn physical_shutdown_admission_matches_helper_and_advertised_scope_capabilities() {
    for features in [
        vec![],
        vec!["recovery"],
        vec!["enrollment"],
        vec!["resize"],
        vec!["migration"],
        vec!["resize", "migration"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let peer = SecretKey::generate().public().to_string();
        let mut value = binding();
        value.physical_trial.as_mut().unwrap().scope.features = features
            .iter()
            .map(|feature| (*feature).to_string())
            .collect();
        let allowed = features.contains(&"resize") || features.contains(&"migration");
        let store = Store::create(&root.path().join("state.redb"), value.clone(), &peer).unwrap();
        finish(&store);
        let created = store
            .handle(
                &peer,
                Request::CreateVm {
                    request_id: "create".into(),
                    name: "shutdown-fixture".into(),
                    disk_mib: 32,
                    memory_mib: 128,
                    min_memory_mib: 128,
                    cores: 1,
                    owner: None,
                    guest: None,
                },
            )
            .unwrap();
        let vm_id = created["vm_id"].as_u64().unwrap() as u16;
        finish(&store);
        let started = store
            .handle(
                &peer,
                Request::VmAction {
                    request_id: "start".into(),
                    vm_id,
                    kind: VmAction::Start,
                },
            )
            .unwrap();
        finish(&store);
        assert_eq!(store.read().unwrap().vms[&vm_id].state, VmState::Running);
        let capabilities = store.handle(&peer, Request::Dashboard {}).unwrap()["capabilities"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            capabilities
                .iter()
                .filter(|name| name.as_str() == Some("shutdown"))
                .count(),
            usize::from(allowed),
            "{features:?}"
        );
        let helper = HostBinding {
            environment: ENV.into(),
            backend: "physical_trial".into(),
            pool_guid: "111".into(),
            physical_trial: value.physical_trial.clone(),
            managed: None,
        };
        helper.validate().unwrap();
        let mut hypothetical = store.read().unwrap().operations
            [started["operation_id"].as_str().unwrap()]
        .job
        .clone();
        hypothetical.kind = JobKind::Shutdown;
        assert_eq!(
            helper.validate_job(&hypothetical).is_ok(),
            allowed,
            "{features:?}"
        );
        let before = serde_json::to_value(store.read().unwrap()).unwrap();
        let request = Request::VmAction {
            request_id: "shutdown".into(),
            vm_id,
            kind: VmAction::Shutdown,
        };
        let result = store.handle(&peer, request.clone());
        if allowed {
            let accepted = result.unwrap();
            assert_eq!(accepted["kind"], "shutdown");
            assert_eq!(accepted["status"], "queued");
            assert_eq!(accepted, store.handle(&peer, request).unwrap());
            let pending = store.pending().unwrap();
            assert_eq!(pending.len(), 1);
            helper.validate_job(&pending[0].job).unwrap();
            assert_eq!(pending[0].job.kind, JobKind::Shutdown);
            assert_eq!(store.read().unwrap().vms[&vm_id].state, VmState::Running);
            finish(&store);
            assert_eq!(store.read().unwrap().vms[&vm_id].state, VmState::Stopped);
        } else {
            assert_eq!(result.unwrap_err().code, "forbidden", "{features:?}");
            assert_eq!(
                serde_json::to_value(store.read().unwrap()).unwrap(),
                before,
                "refused scope queued a shutdown: {features:?}"
            );
            assert!(store.pending().unwrap().is_empty());
        }
    }
}

#[test]
fn physical_shrink_finish_requires_the_exact_stopped_preparation_and_reviewed_acknowledgments() {
    let root = tempfile::tempdir().unwrap();
    let admin = SecretKey::generate().public().to_string();
    let member = SecretKey::generate().public().to_string();
    let mut value = featured_binding();
    value.physical_trial.as_mut().unwrap().scope.features = BTreeSet::from(["resize".into()]);
    let store = Store::create(&root.path().join("state.redb"), value.clone(), &admin).unwrap();
    finish(&store);
    store
        .handle(
            &admin,
            Request::CreateUser {
                request_id: "member".into(),
                user_id: "trial-user".into(),
                budget_mib: 8192,
                min_memory_budget_mib: 512,
                max_memory_budget_mib: 1024,
                max_cores: 4,
                max_vms: 2,
            },
        )
        .unwrap();
    finish(&store);
    store
        .handle(
            &admin,
            Request::EnrollDevice {
                user_id: "trial-user".into(),
                endpoint_id: member.clone(),
                label: "fixture member".into(),
            },
        )
        .unwrap();
    let created = store
        .handle(
            &member,
            Request::CreateVm {
                request_id: "create".into(),
                name: "shrink-finish-fixture".into(),
                disk_mib: 4096,
                memory_mib: 512,
                min_memory_mib: 128,
                cores: 1,
                owner: None,
                guest: None,
            },
        )
        .unwrap();
    let vm_id = created["vm_id"].as_u64().unwrap() as u16;
    finish(&store);
    let prepared = store
        .handle(
            &admin,
            Request::ShrinkVm {
                request_id: "prepare".into(),
                vm_id,
                expected_disk_mib: 4096,
                target_disk_mib: 3072,
                confirm_downtime: true,
                root_partition_uuid: None,
            },
        )
        .unwrap();
    let preparation_id = prepared["operation_id"].as_str().unwrap().to_string();
    finish(&store);
    let catalog = store.read().unwrap();
    assert!(catalog.disk_shrinks[&vm_id].prepared);
    assert_eq!(catalog.disk_shrinks[&vm_id].operation_id, preparation_id);
    assert_eq!(catalog.vms[&vm_id].state, VmState::Stopped);
    assert_eq!(catalog.vms[&vm_id].disk_mib, 3072);
    let request =
        |id: &str, preparation: &str, guest: bool, delete: bool| Request::FinishVmShrink {
            request_id: id.into(),
            vm_id,
            preparation_id: preparation.into(),
            confirm_guest_verified: guest,
            confirm_delete_recovery: delete,
        };
    for (peer, request, code) in [
        (
            &member,
            request("member-denied", &preparation_id, true, true),
            "forbidden",
        ),
        (
            &admin,
            request("no-guest-ack", &preparation_id, false, true),
            "invalid",
        ),
        (
            &admin,
            request("no-delete-ack", &preparation_id, true, false),
            "invalid",
        ),
        (
            &admin,
            request("no-acks", &preparation_id, false, false),
            "invalid",
        ),
        (
            &admin,
            request("wrong-preparation", &"f".repeat(64), true, true),
            "conflict",
        ),
    ] {
        let before = serde_json::to_value(store.read().unwrap()).unwrap();
        assert_eq!(store.handle(peer, request).unwrap_err().code, code);
        assert_eq!(serde_json::to_value(store.read().unwrap()).unwrap(), before);
    }
    store
        .handle(
            &member,
            Request::VmAction {
                request_id: "start".into(),
                vm_id,
                kind: VmAction::Start,
            },
        )
        .unwrap();
    finish(&store);
    let before = serde_json::to_value(store.read().unwrap()).unwrap();
    assert_eq!(
        store
            .handle(
                &admin,
                request("running-refused", &preparation_id, true, true)
            )
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(serde_json::to_value(store.read().unwrap()).unwrap(), before);
    store
        .handle(
            &member,
            Request::VmAction {
                request_id: "shutdown".into(),
                vm_id,
                kind: VmAction::Shutdown,
            },
        )
        .unwrap();
    finish(&store);
    let reviewed = request("finish-reviewed", &preparation_id, true, true);
    let accepted = store.handle(&admin, reviewed.clone()).unwrap();
    assert_eq!(accepted["kind"], "finish_vm_shrink");
    assert_eq!(accepted["status"], "queued");
    assert_eq!(store.handle(&admin, reviewed).unwrap(), accepted);
    let pending = store.pending().unwrap();
    assert_eq!(pending.len(), 1);
    let helper = HostBinding {
        environment: ENV.into(),
        backend: "physical_trial".into(),
        pool_guid: "111".into(),
        physical_trial: value.physical_trial,
        managed: None,
    };
    helper.validate().unwrap();
    helper.validate_job(&pending[0].job).unwrap();
    assert_eq!(
        pending[0]
            .job
            .disk_shrink
            .as_ref()
            .unwrap()
            .preparation_id
            .as_deref(),
        Some(preparation_id.as_str())
    );
    assert_eq!(pending[0].job.vm.as_ref().unwrap().state, VmState::Stopped);
    assert!(
        store.read().unwrap().disk_shrinks.contains_key(&vm_id),
        "acceptance deleted the recovery hold"
    );
    finish(&store);
    assert!(!store.read().unwrap().disk_shrinks.contains_key(&vm_id));
    assert_eq!(store.read().unwrap().vms[&vm_id].state, VmState::Stopped);
    assert_eq!(store.read().unwrap().vms[&vm_id].disk_mib, 3072);
    assert_eq!(store.read().unwrap().users["trial-user"].budget_mib, 8192);
}

#[test]
fn physical_shrink_finish_is_forbidden_without_explicit_resize_capability() {
    for features in [
        vec![],
        vec!["migration"],
        vec!["recovery"],
        vec!["enrollment"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let peer = SecretKey::generate().public().to_string();
        let mut value = binding();
        value.physical_trial.as_mut().unwrap().scope.features =
            features.iter().map(|v| (*v).to_string()).collect();
        let store = Store::create(&root.path().join("state.redb"), value, &peer).unwrap();
        finish(&store);
        let before = serde_json::to_value(store.read().unwrap()).unwrap();
        let result = store.handle(
            &peer,
            Request::FinishVmShrink {
                request_id: "finish-forbidden".into(),
                vm_id: 9051,
                preparation_id: "f".repeat(64),
                confirm_guest_verified: true,
                confirm_delete_recovery: true,
            },
        );
        assert_eq!(result.unwrap_err().code, "forbidden", "{features:?}");
        assert_eq!(serde_json::to_value(store.read().unwrap()).unwrap(), before);
    }
}

#[test]
fn legacy_bindings_preserve_exact_json_and_reject_mislabeled_scope() {
    let legacy = json!({"environment":ENV,"backend":"simulated","disk_capacity_mib":2048,"min_memory_capacity_mib":1024});
    let decoded: Binding = serde_json::from_value(legacy.clone()).unwrap();
    assert!(decoded.physical_trial.is_none());
    assert_eq!(serde_json::to_value(&decoded).unwrap(), legacy);
    let helper = json!({"environment":ENV,"backend":"simulated","pool_guid":"111"});
    let decoded: HostBinding = serde_json::from_value(helper.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), helper);
    let mut altered = binding();
    altered.backend = "lab".into();
    assert!(altered.validate().is_err());
    altered = binding();
    altered.physical_trial = None;
    assert!(altered.validate().is_err());
    altered = binding();
    altered.disk_capacity_mib += 1;
    assert!(altered.validate().is_err());
}

#[test]
fn every_actual_identity_pin_and_root_cap_is_required() {
    let trial = trial();
    let cap = (trial.scope.capacity_mib * 1024 * 1024).to_string();
    let properties = BTreeMap::from([
        ("type".into(), "filesystem".into()),
        ("guid".into(), "222".into()),
        ("org.pve-dev:trial".into(), ENV.into()),
        ("quota".into(), cap.clone()),
        ("reservation".into(), cap),
        ("mountpoint".into(), "none".into()),
    ]);
    let check = |hostname: &str, machine: &str, pool: &str, root: &BTreeMap<String, String>| {
        trial.verify_identity(ENV, hostname, machine, pool, root)
    };
    check("pve2", &"a".repeat(64), "111", &properties).unwrap();
    assert!(check("pve1", &"a".repeat(64), "111", &properties).is_err());
    assert!(check("pve2", &"b".repeat(64), "111", &properties).is_err());
    assert!(check("pve2", &"a".repeat(64), "112", &properties).is_err());
    for key in properties.keys() {
        let mut changed = properties.clone();
        changed.insert(key.clone(), "wrong".into());
        assert!(
            check("pve2", &"a".repeat(64), "111", &changed).is_err(),
            "{key}"
        );
        changed.remove(key);
        assert!(
            check("pve2", &"a".repeat(64), "111", &changed).is_err(),
            "missing {key}"
        );
    }
}

#[test]
fn controller_bootstrap_owners_and_vm_ids_remain_inside_explicit_scope() {
    let root = tempfile::tempdir().unwrap();
    let peer = SecretKey::generate().public().to_string();
    let store = Store::create(&root.path().join("state.redb"), binding(), &peer).unwrap();
    assert_eq!(store.read().unwrap().schema, 4);
    assert_eq!(
        store
            .read()
            .unwrap()
            .users
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["trial-admin"]
    );
    finish(&store);
    assert_eq!(
        store
            .handle(&peer, member("outside", "heidi"))
            .unwrap_err()
            .code,
        "forbidden"
    );
    store.handle(&peer, member("member", "trial-user")).unwrap();
    finish(&store);
    for (number, expected) in [(1, 9051), (2, 9053)] {
        let result = store
            .handle(
                &peer,
                Request::CreateVm {
                    request_id: format!("vm-{number}"),
                    name: format!("trial-{number}"),
                    disk_mib: 32,
                    memory_mib: 128,
                    min_memory_mib: 128,
                    cores: 1,
                    owner: Some("trial-user".into()),
                    guest: None,
                },
            )
            .unwrap();
        assert_eq!(result["vm_id"], expected);
        finish(&store);
    }
    let before = store.read().unwrap().operations.len();
    assert_eq!(
        store
            .handle(
                &peer,
                Request::CreateVm {
                    request_id: "third".into(),
                    name: "third".into(),
                    disk_mib: 32,
                    memory_mib: 128,
                    min_memory_mib: 128,
                    cores: 1,
                    owner: Some("trial-user".into()),
                    guest: None
                }
            )
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(store.read().unwrap().operations.len(), before);
    drop(store);
    let mut changed = binding();
    changed.physical_trial.as_mut().unwrap().scope.root_guid = "333".into();
    assert!(Store::open(&root.path().join("state.redb"), &changed).is_err());
}

#[test]
fn physical_schema_downgrade_fence_survives_policy_and_deletion_transitions() {
    let root = tempfile::tempdir().unwrap();
    let peer = SecretKey::generate().public().to_string();
    let store = Store::create(&root.path().join("state.redb"), binding(), &peer).unwrap();
    finish(&store);
    store.handle(&peer, member("member", "trial-user")).unwrap();
    finish(&store);
    let mut policy = store.read().unwrap().users["trial-user"].policy();
    policy.budget_mib = 256;
    store
        .handle(
            &peer,
            Request::UpdateUserPolicy {
                request_id: "policy".into(),
                user_id: "trial-user".into(),
                expected_revision: 0,
                policy,
            },
        )
        .unwrap();
    finish(&store);
    assert_eq!(store.read().unwrap().schema, 4);
    store
        .handle(
            &peer,
            Request::DeleteUser {
                request_id: "delete".into(),
                user_id: "trial-user".into(),
                expected_revision: 1,
            },
        )
        .unwrap();
    finish(&store);
    assert_eq!(store.read().unwrap().schema, 4);
    assert_eq!(
        store.read().unwrap().users["trial-user"].state,
        pve_dev::control::protocol::AccountState::Deleted
    );
    drop(store);
    let db = redb::Database::open(root.path().join("state.redb")).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<u8, &[u8]>::new(
                "controller_catalog_v1",
            ))
            .unwrap();
        let mut catalog: Value =
            serde_json::from_slice(table.get(0).unwrap().unwrap().value()).unwrap();
        catalog["schema"] = json!(3);
        table
            .insert(0, serde_json::to_vec(&catalog).unwrap().as_slice())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    assert!(Store::open(&root.path().join("state.redb"), &binding()).is_err());
}

#[tokio::test]
async fn bound_submission_rejects_retargeted_helper_before_persisting_work() {
    let root = tempfile::tempdir().unwrap();
    let peer = SecretKey::generate().public().to_string();
    let store = Store::create(&root.path().join("catalog.redb"), binding(), &peer).unwrap();
    let job = store.pending().unwrap().remove(0).job;
    let host = Host::open(HostConfig::simulated(
        root.path().join("helper"),
        ENV,
        root.path().join("world.json"),
    ))
    .await
    .unwrap();
    let expected = HostBinding {
        environment: ENV.into(),
        backend: "physical_trial".into(),
        pool_guid: "111".into(),
        physical_trial: Some(trial()),
        managed: None,
    };
    assert!(
        host.handle(HostRequest::SubmitBound {
            binding: Box::new(expected),
            job
        })
        .await
        .is_err()
    );
    assert!(host.journal().unwrap().jobs.is_empty());
    let health = host.handle(HostRequest::Health).await.unwrap();
    assert!(!binding().attested_by(health.binding.as_ref().unwrap()));
}

#[test]
fn helper_job_allowlist_independently_rejects_foreign_owners_roles_and_ids() {
    let root = tempfile::tempdir().unwrap();
    let peer = SecretKey::generate().public().to_string();
    let store = Store::create(&root.path().join("state.redb"), binding(), &peer).unwrap();
    let job = store.pending().unwrap().remove(0).job;
    let host = HostBinding {
        environment: ENV.into(),
        backend: "physical_trial".into(),
        pool_guid: "111".into(),
        physical_trial: Some(trial()),
        managed: None,
    };
    host.validate_job(&job).unwrap();
    let mut changed = job.clone();
    changed.owner.user_id = "heidi".into();
    changed.owner.role = Role::Member;
    assert!(host.validate_job(&changed).is_err());
    let mut changed = job;
    changed.owner.role = Role::Member;
    assert!(host.validate_job(&changed).is_err());
    let altered: Value = json!({"operation_id":"foreign-vm", "kind":"create", "owner":store.read().unwrap().users["trial-admin"],
        "vm":{"vm_id":103,"owner":"trial-admin","name":"foreign","disk_mib":32,"memory_mib":128,"min_memory_mib":128,"cores":1,"state":"creating"}});
    assert!(
        host.validate_job(&serde_json::from_value(altered).unwrap())
            .is_err()
    );
}

#[test]
fn physical_controller_transport_cannot_be_reopened_with_public_routing() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("controller");
    let peer = SecretKey::generate().public().to_string();
    let mut config = ServerConfig {
        schema: 1,
        binding: binding(),
        host_socket: root.path().join("helper.sock"),
        network: Network::Direct,
        port: 0,
        migration_network: None,
    };
    assert!(transport::initialize(&directory, &config, &peer).is_err());
    assert!(!directory.exists());
    config.network = Network::Loopback;
    transport::initialize(&directory, &config, &peer).unwrap();
    config.network = Network::Public;
    std::fs::write(
        directory.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert!(transport::open(&directory).is_err());
}

#[tokio::test]
async fn offline_physical_binding_restore_preserves_scope_and_stays_quarantined() {
    use pve_dev::control::{
        host::BackendConfig,
        maintenance::{self, Pins},
    };
    use redb::{Database, Durability, TableDefinition};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().canonicalize().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let controller = directory.join("controller");
    let helper = directory.join("helper");
    let peer = SecretKey::generate().public().to_string();
    let config = ServerConfig {
        schema: 1,
        binding: binding(),
        host_socket: directory.join("helper.sock"),
        network: Network::Loopback,
        port: 0,
        migration_network: None,
    };
    transport::initialize(&controller, &config, &peer).unwrap();
    // Construct an offline, pre-effect journal fixture. This exercises bundle
    // validation/quarantine; it makes no physical-host execution claim.
    let host = Host::open(HostConfig::simulated(
        helper.clone(),
        ENV,
        directory.join("world.json"),
    ))
    .await
    .unwrap();
    let mut journal = host.journal().unwrap();
    drop(host);
    journal.schema = 4;
    journal.binding = HostBinding {
        environment: ENV.into(),
        backend: "physical_trial".into(),
        pool_guid: "111".into(),
        physical_trial: Some(trial()),
        managed: None,
    };
    let database = Database::open(helper.join("host-journal.redb")).unwrap();
    let mut tx = database.begin_write().unwrap();
    tx.set_durability(Durability::Immediate).unwrap();
    tx.open_table(TableDefinition::<u8, &[u8]>::new("host_journal_v1"))
        .unwrap()
        .insert(0, serde_json::to_vec(&journal).unwrap().as_slice())
        .unwrap();
    tx.commit().unwrap();
    drop(database);
    let (_, store, key) = transport::open(&controller).unwrap();
    drop(store);
    let pins = Pins {
        environment: ENV.into(),
        server_id: key.public().to_string(),
        pool_guid: "111".into(),
    };
    let backup = directory.join("backup");
    let receipt = maintenance::backup(&controller, &helper, None, &backup, &pins, true).unwrap();
    let restored = directory.join("restored");
    let result = maintenance::restore(&backup, &restored, &pins, &receipt.payload_sha256).unwrap();
    assert!(result.quarantined && !result.activates_workers);
    assert!(transport::open(&restored.join("controller")).is_err());
    let restored_store = Store::open(&restored.join("controller/state.redb"), &binding()).unwrap();
    assert_eq!(restored_store.read().unwrap().binding, binding());
    let error = Host::open(HostConfig {
        state_dir: restored.join("helper"),
        environment: ENV.into(),
        backend: BackendConfig::PhysicalTrial {
            binding: Box::new(trial()),
            images: vec![],
        },
        task_timeout: std::time::Duration::from_secs(1),
        poll_interval: std::time::Duration::from_secs(1),
        command_timeout: std::time::Duration::from_secs(1),
    })
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("quarantined"));
}

#[test]
fn offline_inspection_requires_identity_pins_and_preserves_existing_ledger() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("controller");
    let peer = SecretKey::generate().public().to_string();
    let config = ServerConfig {
        schema: 1,
        binding: binding(),
        host_socket: root.path().join("helper.sock"),
        network: Network::Loopback,
        port: 0,
        migration_network: None,
    };
    transport::initialize(&directory, &config, &peer).unwrap();
    let (_, store, key) = transport::open(&directory).unwrap();
    let before = serde_json::to_value(store.read().unwrap()).unwrap();
    drop(store);
    let run = |path: &std::path::Path, server: &str, environment: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_pve-devd"))
            .arg("inspect-offline")
            .arg("--data-dir")
            .arg(path)
            .arg("--expected-server")
            .arg(server)
            .arg("--expected-environment")
            .arg(environment)
            .output()
            .unwrap()
    };
    let server = key.public().to_string();
    let output = run(&directory, &server, ENV);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["server_id"], server);
    assert_eq!(data["binding"], before["binding"]);
    assert_eq!(data["operations"][0]["status"], "queued");
    assert!(
        data.get("identity").is_none()
            && data.get("jobs").is_none()
            && data.get("devices").is_none()
    );
    let (_, store, _) = transport::open(&directory).unwrap();
    assert_eq!(serde_json::to_value(store.read().unwrap()).unwrap(), before);
    drop(store);
    let wrong = SecretKey::generate().public().to_string();
    for result in [
        run(&directory, &wrong, ENV),
        run(&directory, &server, &"f".repeat(32)),
        run(&root.path().join("missing"), &server, ENV),
    ] {
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
    assert!(!root.path().join("missing").exists());
}
