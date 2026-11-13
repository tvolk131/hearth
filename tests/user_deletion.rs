use iroh::SecretKey;
use pve_dev::control::JobKind;
use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, SimulatedWorld, Step, World},
    protocol::*,
    store::{Binding, Store},
};
use serde_json::Value;
use std::path::PathBuf;
const ENV: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn independent_owner_storage_aliases_block_before_and_after_managed_storage_removal() {
    for after_removal in [false, true] {
        for descendant in [false, true] {
            let (lab, host) = Lab::new().await;
            let op = lab.call(lab.deletion("alias-guard"));
            if after_removal {
                lab.edit(|world| {
                    world.faults.push(Fault {
                        step: Step::OwnerStorage,
                        point: FaultPoint::AfterEffect,
                    })
                });
                let id = op["operation_id"].as_str().unwrap();
                host.submit(lab.store.read().unwrap().operations[id].job.clone())
                    .unwrap();
                assert!(host.run_pending().await.is_err());
                assert!(!lab.snapshot().storages.contains_key("ctl-alice"));
            }
            lab.edit(|world| {
                let mut alias = world.storages["ctl-erin"].clone();
                alias.insert("storage".into(), "independent-alias".into());
                alias.insert(
                    "pool".into(),
                    if descendant {
                        "labpool/control/alice/missing-child"
                    } else {
                        "labpool/control/alice"
                    }
                    .into(),
                );
                world.storages.insert("independent-alias".into(), alias);
            });
            let before = lab.snapshot();
            lab.finish(&host).await;
            let catalog = lab.store.read().unwrap();
            let record = &catalog.operations[op["operation_id"].as_str().unwrap()];
            assert_eq!(record.public.status, OperationStatus::Blocked);
            assert!(
                record
                    .public
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("another storage ID aliases")
            );
            assert_eq!(lab.snapshot().datasets, before.datasets);
            assert_eq!(lab.snapshot().storages, before.storages);
            assert_eq!(lab.user().state, AccountState::Deleting);
            assert!(!lab.user().ready);
            assert!(
                lab.store
                    .handle(&lab.admin, lab.create_user("frank", 256))
                    .is_err()
            );
        }
    }
}

#[tokio::test]
async fn ancestor_storage_pool_aliases_are_preserved_without_blocking_empty_owner_removal() {
    let (lab, host) = Lab::new().await;
    lab.edit(|world| {
        for (id, pool) in [
            ("ancestor-pool", "labpool"),
            ("ancestor-control", "labpool/control"),
        ] {
            let mut alias = world.storages["ctl-erin"].clone();
            alias.insert("storage".into(), id.into());
            alias.insert("pool".into(), pool.into());
            world.storages.insert(id.into(), alias);
        }
    });
    let before = lab.snapshot().storages;
    lab.call(lab.deletion("allowed-ancestors"));
    lab.finish(&host).await;
    assert_eq!(lab.user().state, AccountState::Deleted);
    for id in ["ancestor-pool", "ancestor-control"] {
        assert_eq!(lab.snapshot().storages[id], before[id]);
    }
}

#[tokio::test]
async fn embedded_storage_references_block_empty_owner_removal_without_effects() {
    let (lab, host) = Lab::new().await;
    lab.edit(|world| {
        world.vms.insert(
            9999,
            pve_dev::control::host::SimulatedVm {
                config: std::collections::BTreeMap::from([(
                    "description".into(),
                    "embedded,storage=ctl-alice:vm-9999-disk-0,other=value".into(),
                )]),
                ..Default::default()
            },
        );
    });
    let before = lab.snapshot();
    let op = lab.call(lab.deletion("embedded-reference"));
    lab.finish(&host).await;
    let catalog = lab.store.read().unwrap();
    let record = &catalog.operations[op["operation_id"].as_str().unwrap()];
    assert_eq!(record.public.status, OperationStatus::Blocked);
    assert!(
        record
            .public
            .error
            .as_deref()
            .unwrap()
            .contains("still references")
    );
    assert_eq!(lab.snapshot().datasets, before.datasets);
    assert_eq!(lab.snapshot().storages, before.storages);
    assert_eq!(lab.user().state, AccountState::Deleting);
    assert!(!lab.user().ready);
    assert!(
        lab.store
            .handle(&lab.admin, lab.create_user("frank", 256))
            .is_err()
    );
}

#[tokio::test]
async fn pending_effective_storage_reference_blocks_empty_owner_cleanup_without_effects() {
    let (lab, host) = Lab::new().await;
    lab.edit(|world| {
        world.vms.insert(
            9999,
            pve_dev::control::host::SimulatedVm {
                config: std::collections::BTreeMap::from([(
                    "description".into(),
                    "unmanaged guest".into(),
                )]),
                pending_config: std::collections::BTreeMap::from([(
                    "unused0".into(),
                    "ctl-alice:vm-9999-disk-0".into(),
                )]),
                ..Default::default()
            },
        );
    });
    let before = lab.snapshot();
    let op = lab.call(lab.deletion("pending-reference"));
    lab.finish(&host).await;
    let record = &lab.store.read().unwrap().operations[op["operation_id"].as_str().unwrap()];
    assert_eq!(record.public.status, OperationStatus::Blocked);
    assert!(
        record
            .public
            .error
            .as_deref()
            .unwrap()
            .contains("still references")
    );
    assert_eq!(lab.snapshot().datasets, before.datasets);
    assert_eq!(lab.snapshot().storages, before.storages);
    assert_eq!(lab.user().state, AccountState::Deleting);
    assert!(!lab.user().ready);
    assert!(
        lab.store
            .handle(&lab.admin, lab.create_user("frank", 256))
            .is_err()
    );
}

#[tokio::test]
async fn local_provenance_returns_recorded_identities_and_unsettled_status_without_adoption() {
    use pve_dev::control::host::HostRequest;
    let (lab, host) = Lab::new().await;
    let recorded = host.journal().unwrap().owners["alice"].identities["dataset"].clone();
    lab.edit(|world| {
        world
            .datasets
            .get_mut("labpool/control/alice")
            .unwrap()
            .insert("guid".into(), "999999".into());
    });
    let manifest = host
        .handle(HostRequest::OwnedResources)
        .await
        .unwrap()
        .resources
        .unwrap();
    assert_eq!(manifest.datasets["labpool/control/alice"], recorded);
    assert_ne!(manifest.datasets["labpool/control/alice"]["guid"], "999999");
    assert!(manifest.storages.contains_key("ctl-alice"));
    assert!(manifest.unsettled_jobs.is_empty());
    let queued = lab.call(lab.deletion("unsettled-provenance"));
    let id = queued["operation_id"].as_str().unwrap();
    host.submit(lab.store.read().unwrap().operations[id].job.clone())
        .unwrap();
    let manifest = host
        .handle(HostRequest::OwnedResources)
        .await
        .unwrap()
        .resources
        .unwrap();
    assert_eq!(manifest.unsettled_jobs[id], OperationStatus::Queued);
    assert!(
        serde_json::to_value(&manifest)
            .unwrap()
            .get("jobs")
            .is_none()
    );
}

struct Lab {
    root: tempfile::TempDir,
    store: Store,
    admin: String,
    member: String,
    world: PathBuf,
}
impl Lab {
    async fn new() -> (Self, Host) {
        let root = tempfile::tempdir().unwrap();
        let admin = SecretKey::generate().public().to_string();
        let store = Store::create(
            &root.path().join("state.redb"),
            Binding {
                environment: ENV.into(),
                backend: "simulated".into(),
                disk_capacity_mib: 400,
                min_memory_capacity_mib: 1024,
                guest_images: vec![],
                physical_trial: None,
                managed: None,
            },
            &admin,
        )
        .unwrap();
        let member = SecretKey::generate().public().to_string();
        let world = root.path().join("world.json");
        let lab = Self {
            root,
            store,
            admin,
            member,
            world,
        };
        let host = lab.host().await;
        lab.finish(&host).await;
        lab.call(lab.create_user("alice", 128));
        lab.finish(&host).await;
        lab.call(Request::EnrollDevice {
            user_id: "alice".into(),
            endpoint_id: lab.member.clone(),
            label: "Alice".into(),
        });
        (lab, host)
    }
    async fn host(&self) -> Host {
        Host::open(HostConfig::simulated(
            self.root.path().join("helper"),
            ENV,
            self.world.clone(),
        ))
        .await
        .unwrap()
    }
    fn call(&self, r: Request) -> Value {
        self.store.handle(&self.admin, r).unwrap()
    }
    fn create_user(&self, id: &str, budget: u64) -> Request {
        Request::CreateUser {
            request_id: format!("new-{id}"),
            user_id: id.into(),
            budget_mib: budget,
            min_memory_budget_mib: 128,
            max_memory_budget_mib: 1024,
            max_cores: 4,
            max_vms: 4,
        }
    }
    fn deletion(&self, id: &str) -> Request {
        Request::DeleteUser {
            request_id: id.into(),
            user_id: "alice".into(),
            expected_revision: 0,
        }
    }
    async fn finish(&self, host: &Host) {
        for record in self.store.pending().unwrap() {
            host.submit(record.job.clone()).unwrap();
            host.run_pending().await.unwrap();
            let proof = &host.journal().unwrap().jobs[&record.public.operation_id];
            self.store
                .progress(
                    &record.public.operation_id,
                    proof.status.clone(),
                    &proof.step,
                    proof.error.clone(),
                )
                .unwrap();
        }
    }
    fn edit(&self, action: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|world| {
                action(world);
                Ok(())
            })
            .unwrap()
    }
    fn snapshot(&self) -> World {
        SimulatedWorld::new(self.world.clone()).load().unwrap()
    }
    fn user(&self) -> User {
        self.store.read().unwrap().users["alice"].clone()
    }
    fn create_vm(&self) -> Request {
        Request::CreateVm {
            request_id: "alice-vm".into(),
            name: "alice-vm".into(),
            disk_mib: 32,
            memory_mib: 128,
            min_memory_mib: 128,
            cores: 1,
            owner: Some("alice".into()),
            guest: None,
        }
    }
}

#[tokio::test]
async fn deleting_an_empty_member_revokes_keys_retains_tombstones_and_releases_only_verified_capacity()
 {
    let (lab, host) = Lab::new().await;
    let request = lab.deletion("delete-alice");
    let op = lab.call(request.clone());
    assert_eq!(lab.user().state, AccountState::Deleting);
    assert!(!lab.user().ready);
    assert!(
        lab.store
            .handle(&lab.member, Request::Dashboard {})
            .is_err()
    );
    assert_eq!(lab.call(request.clone()), op);
    assert!(
        lab.store
            .handle(&lab.admin, lab.create_user("frank", 256))
            .is_err()
    );
    assert!(
        lab.store
            .handle(
                &lab.admin,
                Request::EnrollDevice {
                    user_id: "alice".into(),
                    endpoint_id: SecretKey::generate().public().to_string(),
                    label: "new".into()
                }
            )
            .is_err()
    );
    lab.finish(&host).await;
    assert_eq!(lab.user().state, AccountState::Deleted);
    assert!(!lab.user().ready);
    let world = lab.snapshot();
    assert!(!world.datasets.contains_key("labpool/control/alice"));
    assert!(!world.storages.contains_key("ctl-alice"));
    assert!(world.datasets.contains_key("labpool/control/erin"));
    assert!(world.datasets.contains_key("labpool/control"));
    assert_eq!(
        host.journal().unwrap().owners["alice"].state,
        AccountState::Deleted
    );
    assert_eq!(lab.call(request)["status"], "verified");
    let mut reused = lab.create_user("alice", 128);
    if let Request::CreateUser { request_id, .. } = &mut reused {
        *request_id = "fresh-reuse-attempt".into();
    }
    assert!(lab.store.handle(&lab.admin, reused).is_err());
    assert_eq!(
        lab.call(lab.create_user("alice", 128))["status"],
        "verified"
    );
    assert_eq!(lab.user().state, AccountState::Deleted);
    lab.call(lab.create_user("frank", 256));
    lab.finish(&host).await;
    assert_eq!(lab.store.read().unwrap().schema, 3);
    assert_eq!(host.journal().unwrap().schema, 3);
}

#[tokio::test]
async fn users_with_stopped_running_pending_or_blocked_resources_cannot_be_deleted() {
    for condition in 0..4 {
        let (lab, host) = Lab::new().await;
        let created = lab.call(lab.create_vm());
        if condition != 0 {
            lab.finish(&host).await;
        }
        let vm = created["vm_id"].as_u64().unwrap() as u16;
        if condition == 2 {
            lab.call(Request::VmAction {
                request_id: "start".into(),
                vm_id: vm,
                kind: VmAction::Start,
            });
            lab.finish(&host).await;
        }
        if condition == 3 {
            let request = Request::VmAction {
                request_id: "blocked".into(),
                vm_id: vm,
                kind: VmAction::Start,
            };
            let op = lab.call(request);
            lab.store
                .progress(
                    op["operation_id"].as_str().unwrap(),
                    OperationStatus::Blocked,
                    "test-uncertain",
                    Some("unknown".into()),
                )
                .unwrap();
        }
        let before = lab.snapshot().datasets;
        assert!(
            lab.store
                .handle(&lab.admin, lab.deletion("no-cascade"))
                .is_err()
        );
        assert_eq!(lab.user().state, AccountState::Active);
        assert!(lab.store.authorized(&lab.member));
        assert_eq!(lab.snapshot().datasets, before);
    }
}

#[tokio::test]
async fn explicit_verified_vm_deletion_is_required_and_its_history_is_preserved() {
    let (lab, host) = Lab::new().await;
    let created = lab.call(lab.create_vm());
    lab.finish(&host).await;
    let vm = created["vm_id"].as_u64().unwrap() as u16;
    let original_job = lab.store.read().unwrap().operations
        [created["operation_id"].as_str().unwrap()]
    .job
    .clone();
    lab.call(Request::VmAction {
        request_id: "explicit-delete".into(),
        vm_id: vm,
        kind: VmAction::Delete,
    });
    assert!(
        lab.store
            .handle(&lab.admin, lab.deletion("too-early"))
            .is_err()
    );
    lab.finish(&host).await;
    lab.call(lab.deletion("now-empty"));
    lab.finish(&host).await;
    assert_eq!(lab.user().state, AccountState::Deleted);
    assert_eq!(
        host.submit(original_job).unwrap().status,
        OperationStatus::Verified
    );
    assert_eq!(lab.store.read().unwrap().vms[&vm].state, VmState::Deleted);
    assert!(lab.snapshot().vms.is_empty());
}

#[tokio::test]
async fn deletion_refuses_member_admin_self_stale_or_missing_identity() {
    let (lab, _host) = Lab::new().await;
    assert_eq!(
        lab.store
            .handle(&lab.member, lab.deletion("member-attempt"))
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        lab.store
            .handle(
                &lab.admin,
                Request::DeleteUser {
                    request_id: "self".into(),
                    user_id: "erin".into(),
                    expected_revision: 0
                }
            )
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert!(
        lab.store
            .handle(
                &lab.admin,
                Request::DeleteUser {
                    request_id: "stale".into(),
                    user_id: "alice".into(),
                    expected_revision: 1
                }
            )
            .is_err()
    );
    assert!(
        lab.store
            .handle(
                &lab.admin,
                Request::DeleteUser {
                    request_id: "missing".into(),
                    user_id: "unknown".into(),
                    expected_revision: 0
                }
            )
            .is_err()
    );
    assert!(lab.store.authorized(&lab.member));
    assert_eq!(lab.user().state, AccountState::Active);
    lab.store
        .change(|catalog| {
            catalog.users.get_mut("alice").unwrap().role = Role::Admin;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        lab.store
            .handle(&lab.admin, lab.deletion("other-admin"))
            .unwrap_err()
            .code,
        "forbidden"
    );
}

#[tokio::test]
async fn files_snapshots_foreign_children_mounts_and_storage_drift_block_without_destruction() {
    for drift in 0..6 {
        let (lab, host) = Lab::new().await;
        let op = lab.call(lab.deletion("empty-only"));
        lab.edit(|world| match drift {
            0 => {
                world
                    .datasets
                    .get_mut("labpool/control/alice")
                    .unwrap()
                    .insert("file_count".into(), "1".into());
            }
            1 => {
                world
                    .datasets
                    .insert("labpool/control/alice@keep".into(), Default::default());
            }
            2 => {
                world
                    .zfs_create("labpool/control/alice/foreign", None, &[])
                    .unwrap();
            }
            3 => {
                world
                    .datasets
                    .get_mut("labpool/control/alice")
                    .unwrap()
                    .insert("mounted".into(), "yes".into());
            }
            4 => {
                world
                    .storages
                    .get_mut("ctl-alice")
                    .unwrap()
                    .insert("pool".into(), "labpool/control/erin".into());
            }
            _ => {
                world
                    .datasets
                    .get_mut("labpool/control/alice")
                    .unwrap()
                    .insert("probe_failure".into(), "1".into());
            }
        });
        let before = lab.snapshot();
        lab.finish(&host).await;
        let proof = lab.store.read().unwrap().operations[op["operation_id"].as_str().unwrap()]
            .public
            .clone();
        assert_eq!(
            proof.status,
            OperationStatus::Blocked,
            "drift {drift}: {proof:?}"
        );
        assert_eq!(lab.snapshot().datasets, before.datasets);
        assert_eq!(lab.snapshot().storages, before.storages);
        assert_eq!(lab.user().state, AccountState::Deleting);
        assert!(!lab.store.authorized(&lab.member));
        assert!(
            lab.store
                .handle(&lab.admin, lab.create_user("frank", 256))
                .is_err()
        );
    }
}

#[tokio::test]
async fn every_empty_owner_cleanup_boundary_recovers_or_blocks_with_claims_retained() {
    for step in [
        Step::OwnerPreflight,
        Step::OwnerStorage,
        Step::OwnerDataset,
        Step::OwnerRelease,
    ] {
        for point in [FaultPoint::BeforeEffect, FaultPoint::AfterEffect] {
            for reboot in [false, true] {
                let (lab, host) = Lab::new().await;
                let op = lab.call(lab.deletion("crash-delete"));
                let id = op["operation_id"].as_str().unwrap();
                host.submit(lab.store.read().unwrap().operations[id].job.clone())
                    .unwrap();
                lab.edit(|world| world.faults.push(Fault { step, point }));
                assert!(host.run_pending().await.is_err(), "{step:?} {point:?}");
                assert_eq!(lab.user().state, AccountState::Deleting);
                assert!(
                    lab.store
                        .handle(&lab.admin, lab.create_user("frank", 256))
                        .is_err()
                );
                drop(host);
                if reboot {
                    lab.edit(World::new_boot)
                }
                let host = lab.host().await;
                lab.finish(&host).await;
                let blocked = point == FaultPoint::BeforeEffect
                    && matches!(step, Step::OwnerStorage | Step::OwnerDataset)
                    && !reboot;
                let record = lab.store.read().unwrap().operations[id].public.clone();
                assert_eq!(
                    record.status,
                    if blocked {
                        OperationStatus::Blocked
                    } else {
                        OperationStatus::Verified
                    },
                    "{step:?} {point:?} reboot={reboot}: {record:?}"
                );
                assert_eq!(
                    lab.user().state,
                    if blocked {
                        AccountState::Deleting
                    } else {
                        AccountState::Deleted
                    }
                );
            }
        }
    }
}

#[tokio::test]
async fn a_partial_cleanup_rechecks_foreign_dataset_identity_and_keeps_budget() {
    let (lab, host) = Lab::new().await;
    let op = lab.call(lab.deletion("partial"));
    let id = op["operation_id"].as_str().unwrap();
    host.submit(lab.store.read().unwrap().operations[id].job.clone())
        .unwrap();
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::OwnerStorage,
            point: FaultPoint::AfterEffect,
        })
    });
    assert!(host.run_pending().await.is_err());
    drop(host);
    lab.edit(|world| {
        world
            .datasets
            .get_mut("labpool/control/alice")
            .unwrap()
            .insert("guid".into(), "foreign".into());
        world.new_boot();
    });
    let host = lab.host().await;
    lab.finish(&host).await;
    assert_eq!(
        lab.store.read().unwrap().operations[id].public.status,
        OperationStatus::Blocked
    );
    assert!(
        lab.snapshot()
            .datasets
            .contains_key("labpool/control/alice")
    );
    assert!(
        lab.store
            .handle(&lab.admin, lab.create_user("frank", 256))
            .is_err()
    );
}

#[tokio::test]
async fn unknown_guest_references_and_recreated_helper_requests_never_destroy_or_recreate() {
    let (lab, host) = Lab::new().await;
    let op = lab.call(lab.deletion("references"));
    lab.edit(|world| {
        world.vms.insert(
            9999,
            pve_dev::control::host::SimulatedVm {
                config: std::collections::BTreeMap::from([(
                    "scsi0".into(),
                    "ctl-alice:vm-9999-disk-0".into(),
                )]),
                pending_config: Default::default(),
                running: false,
                pid: None,
            },
        );
    });
    lab.finish(&host).await;
    assert_eq!(
        lab.store.read().unwrap().operations[op["operation_id"].as_str().unwrap()]
            .public
            .status,
        OperationStatus::Blocked
    );
    assert!(lab.snapshot().storages.contains_key("ctl-alice"));
    assert!(
        lab.snapshot()
            .datasets
            .contains_key("labpool/control/alice")
    );
    let (lab, host) = Lab::new().await;
    let initial = host
        .journal()
        .unwrap()
        .jobs
        .values()
        .find(|job| job.job.owner.user_id == "alice")
        .unwrap()
        .job
        .clone();
    lab.call(lab.deletion("empty"));
    lab.finish(&host).await;
    let mut changed = initial;
    changed.operation_id = "new-ensure-cannot-resurrect".into();
    host.submit(changed).unwrap();
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["new-ensure-cannot-resurrect"].status,
        OperationStatus::Blocked
    );
    assert!(
        !lab.snapshot()
            .datasets
            .contains_key("labpool/control/alice")
    );
}

#[tokio::test]
async fn deletion_after_verified_policy_update_requires_and_preserves_current_revision() {
    let (lab, host) = Lab::new().await;
    let mut policy = lab.user().policy();
    policy.budget_mib = 256;
    lab.call(Request::UpdateUserPolicy {
        request_id: "policy".into(),
        user_id: "alice".into(),
        expected_revision: 0,
        policy,
    });
    lab.finish(&host).await;
    assert_eq!(lab.user().policy_revision, 1);
    assert!(lab.store.handle(&lab.admin, lab.deletion("stale")).is_err());
    let req = Request::DeleteUser {
        request_id: "current".into(),
        user_id: "alice".into(),
        expected_revision: 1,
    };
    lab.call(req);
    lab.finish(&host).await;
    assert_eq!(lab.user().state, AccountState::Deleted);
    assert_eq!(lab.user().policy_revision, 1);
    assert_eq!(host.journal().unwrap().owners["alice"].policy_revision, 1);
}

#[tokio::test]
async fn helper_initial_role_proof_refuses_a_forged_member_payload_for_the_administrator() {
    let (lab, host) = Lab::new().await;
    let mut admin = lab.store.read().unwrap().users["erin"].clone();
    admin.role = Role::Member;
    host.submit(pve_dev::control::store::Job {
        operation_id: "forged-admin-deletion".into(),
        kind: JobKind::DeleteUser,
        owner: admin,
        vm: None,
        disk_shrink: None,
        extension: None,
        policy_change: None,
    })
    .unwrap();
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["forged-admin-deletion"].status,
        OperationStatus::Blocked
    );
    assert!(lab.snapshot().datasets.contains_key("labpool/control/erin"));
    assert!(lab.snapshot().storages.contains_key("ctl-erin"));
}
