use iroh::SecretKey;
use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, MIB, SimulatedWorld, Step, World},
    protocol::*,
    store::{Binding, Store},
};
use redb::ReadableTable;
use serde_json::{Value, json};
use std::path::PathBuf;
const ENV: &str = "0123456789abcdef0123456789abcdef";
struct Lab {
    root: tempfile::TempDir,
    store: Store,
    admin: String,
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
                disk_capacity_mib: 2048,
                min_memory_capacity_mib: 1024,
                guest_images: vec![],
                physical_trial: None,
                managed: None,
            },
            &admin,
        )
        .unwrap();
        let world = root.path().join("world.json");
        let lab = Self {
            root,
            store,
            admin,
            world,
        };
        let host = lab.host().await;
        lab.finish(&host).await;
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
    fn edit(&self, action: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|world| {
                action(world);
                Ok(())
            })
            .unwrap();
    }
    fn snapshot(&self) -> World {
        SimulatedWorld::new(self.world.clone()).load().unwrap()
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
    fn user(&self) -> User {
        self.store.read().unwrap().users["erin"].clone()
    }
    fn update(&self, id: &str, budget: u64) -> Request {
        let user = self.user();
        let mut policy = user.policy();
        policy.budget_mib = budget;
        Request::UpdateUserPolicy {
            request_id: id.into(),
            user_id: "erin".into(),
            expected_revision: user.policy_revision,
            policy,
        }
    }
    fn create(&self, id: &str) -> Request {
        Request::CreateVm {
            request_id: id.into(),
            name: id.into(),
            disk_mib: 32,
            memory_mib: 128,
            min_memory_mib: 128,
            cores: 1,
            owner: None,
            guest: None,
        }
    }
    fn accepted(&self, request: Request) -> Value {
        self.store.handle(&self.admin, request).unwrap()
    }
}

#[tokio::test]
async fn safe_growth_unused_contraction_and_subsequent_lifecycle_preserve_history() {
    let (lab, host) = Lab::new().await;
    let created = lab.accepted(lab.create("one"));
    lab.finish(&host).await;
    let vmid = created["vm_id"].as_u64().unwrap() as u16;
    let before = lab.snapshot();
    let original = lab.store.read().unwrap().operations[created["operation_id"].as_str().unwrap()]
        .job
        .clone();
    let fingerprint = pve_dev::control::host::fingerprint(&original).unwrap();
    for (id, budget, revision) in [("grow", 256, 1), ("contract", 64, 2)] {
        let req = lab.update(id, budget);
        let value = lab.accepted(req.clone());
        assert!(!lab.user().ready);
        assert_eq!(lab.user().budget_mib, if revision == 1 { 128 } else { 256 });
        assert_eq!(lab.accepted(req), value);
        assert!(lab.store.handle(&lab.admin, lab.create("during")).is_err());
        assert!(
            lab.store
                .handle(
                    &lab.admin,
                    Request::VmAction {
                        request_id: "during-action".into(),
                        vm_id: vmid,
                        kind: VmAction::Start
                    }
                )
                .is_err()
        );
        lab.finish(&host).await;
        let user = lab.user();
        assert!(user.ready);
        assert_eq!(user.budget_mib, budget);
        assert_eq!(user.policy_revision, revision);
        let world = lab.snapshot();
        assert_eq!(
            world.datasets["labpool/control/erin"]["quota"],
            (budget * MIB).to_string()
        );
        assert_eq!(
            world.datasets["labpool/control/erin"]["reservation"],
            (budget * MIB).to_string()
        );
        assert_eq!(
            world.datasets["labpool/control/erin/vm-9001-disk-0"],
            before.datasets["labpool/control/erin/vm-9001-disk-0"]
        );
        assert_eq!(world.vms[&vmid].config, before.vms[&vmid].config);
    }
    assert_eq!(
        pve_dev::control::host::fingerprint(&original).unwrap(),
        fingerprint
    );
    assert_eq!(
        host.submit(original).unwrap().status,
        OperationStatus::Verified
    );
    for (id, kind) in [
        ("start", VmAction::Start),
        ("stop", VmAction::Stop),
        ("delete", VmAction::Delete),
    ] {
        lab.accepted(Request::VmAction {
            request_id: id.into(),
            vm_id: vmid,
            kind,
        });
        lab.finish(&host).await;
    }
    assert!(lab.snapshot().vms.is_empty());
    assert_eq!(lab.store.read().unwrap().schema, 2);
    assert_eq!(host.journal().unwrap().schema, 2);
}

#[tokio::test]
async fn stale_zero_below_allocations_and_unsettled_owners_are_refused_without_intents() {
    let (lab, host) = Lab::new().await;
    let mut zero = lab.user().policy();
    zero.budget_mib = 0;
    assert_eq!(
        lab.store
            .handle(
                &lab.admin,
                Request::UpdateUserPolicy {
                    request_id: "zero".into(),
                    user_id: "erin".into(),
                    expected_revision: 0,
                    policy: zero
                }
            )
            .unwrap_err()
            .code,
        "invalid"
    );
    let stale = lab.update("stale", 64);
    lab.accepted(lab.update("grow", 256));
    lab.finish(&host).await;
    assert_eq!(
        lab.store.handle(&lab.admin, stale).unwrap_err().code,
        "conflict"
    );
    lab.accepted(lab.create("one"));
    assert!(
        lab.store
            .handle(&lab.admin, lab.update("pending", 64))
            .is_err()
    );
    lab.finish(&host).await;
    lab.accepted(lab.create("two"));
    lab.finish(&host).await;
    assert!(
        lab.store
            .handle(&lab.admin, lab.update("too-small", 64))
            .unwrap_err()
            .message
            .contains("committed")
    );
    assert_eq!(lab.user().budget_mib, 256);
    assert!(lab.user().ready);
}

#[tokio::test]
async fn actual_snapshot_usage_unmanaged_descendants_and_guid_drift_block_before_changes() {
    for drift in 0..3 {
        let (lab, host) = Lab::new().await;
        lab.accepted(lab.create("one"));
        lab.finish(&host).await;
        let operation = lab.accepted(lab.update("shrink", 64));
        lab.edit(|world| match drift {
            0 => {
                world
                    .datasets
                    .get_mut("labpool/control/erin")
                    .unwrap()
                    .insert("extra_used".into(), (80 * MIB).to_string());
            }
            1 => {
                world
                    .zfs_create("labpool/control/erin/foreign", None, &[])
                    .unwrap();
            }
            _ => {
                world
                    .datasets
                    .get_mut("labpool/control/erin")
                    .unwrap()
                    .insert("guid".into(), "foreign-guid".into());
            }
        });
        let before = lab.snapshot().datasets;
        lab.finish(&host).await;
        assert_eq!(
            lab.store.read().unwrap().operations[operation["operation_id"].as_str().unwrap()]
                .public
                .status,
            OperationStatus::Blocked
        );
        assert_eq!(lab.snapshot().datasets, before);
        assert_eq!(lab.user().budget_mib, 128);
        assert!(!lab.user().ready);
        assert!(
            lab.store
                .handle(&lab.admin, lab.create("after-block"))
                .is_err()
        );
    }
}

#[tokio::test]
async fn every_policy_property_crash_boundary_keeps_claims_and_recovers_only_with_proof() {
    for budget in [64, 256] {
        for step in [Step::PolicyFirst, Step::PolicySecond, Step::PolicyCommit] {
            for point in [FaultPoint::BeforeEffect, FaultPoint::AfterEffect] {
                for reboot in [false, true] {
                    let (lab, host) = Lab::new().await;
                    lab.accepted(lab.create("one"));
                    lab.finish(&host).await;
                    let value = lab.accepted(lab.update("edit", budget));
                    let id = value["operation_id"].as_str().unwrap();
                    let job = lab.store.read().unwrap().operations[id].job.clone();
                    host.submit(job).unwrap();
                    lab.edit(|world| world.faults.push(Fault { step, point }));
                    assert!(
                        host.run_pending().await.is_err(),
                        "{budget} {step:?} {point:?}"
                    );
                    assert_eq!(lab.user().budget_mib, 128);
                    assert!(!lab.user().ready);
                    drop(host);
                    if reboot {
                        lab.edit(World::new_boot)
                    }
                    let host = lab.host().await;
                    lab.finish(&host).await;
                    let expected = if point == FaultPoint::BeforeEffect
                        && step != Step::PolicyCommit
                        && !reboot
                    {
                        OperationStatus::Blocked
                    } else {
                        OperationStatus::Verified
                    };
                    let record = lab.store.read().unwrap().operations[id].public.clone();
                    assert_eq!(
                        record.status, expected,
                        "budget={budget} {step:?} {point:?} reboot={reboot}: {record:?}"
                    );
                    if expected == OperationStatus::Verified {
                        assert_eq!(lab.user().budget_mib, budget);
                        assert_eq!(lab.user().policy_revision, 1)
                    } else {
                        assert_eq!(lab.user().budget_mib, 128);
                        assert!(!lab.user().ready)
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn pending_increases_hold_capacity_and_competing_acceptance_is_serialized() {
    let (lab, host) = Lab::new().await;
    // Disk capacity is 2048 MiB. Pending growth from128 to1900 leaves148, not1920.
    lab.accepted(lab.update("grow", 1900));
    let add = Request::CreateUser {
        request_id: "other".into(),
        user_id: "other".into(),
        budget_mib: 256,
        min_memory_budget_mib: 128,
        max_memory_budget_mib: 256,
        max_cores: 1,
        max_vms: 1,
    };
    assert!(lab.store.handle(&lab.admin, add).is_err());
    assert!(
        lab.store
            .handle(&lab.admin, lab.update("second", 256))
            .is_err()
    );
    lab.finish(&host).await;
    assert_eq!(lab.user().budget_mib, 1900);
}

#[test]
fn legacy_job_serialization_and_canonical_fingerprint_are_pinned() {
    let value = json!({"operation_id":"legacy","kind":"ensure_user","owner":{"user_id":"alice","role":"member","budget_mib":128,"min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":4,"ready":false},"vm":null});
    let job: pve_dev::control::store::Job = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&job).unwrap(), value);
    let expected = r#"{"operation_id":"legacy","kind":"ensure_user","owner":{"user_id":"alice","role":"member","budget_mib":128,"min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":4,"ready":false},"vm":null}"#;
    assert_eq!(serde_json::to_vec(&job).unwrap(), expected.as_bytes());
    // Fingerprints hash the canonical (key-sorted) form behind a version tag.
    let canonical = r#"{"kind":"ensure_user","operation_id":"legacy","owner":{"budget_mib":128,"max_cores":4,"max_memory_budget_mib":1024,"max_vms":4,"min_memory_budget_mib":256,"ready":false,"role":"member","user_id":"alice"},"vm":null}"#;
    use sha2::Digest;
    assert_eq!(
        pve_dev::control::host::fingerprint(&job).unwrap(),
        format!(
            "{:x}",
            sha2::Sha256::digest([b"hearth-job-v1\n".as_slice(), canonical.as_bytes()].concat())
        )
    );
    let bytes = serde_json::to_vec(&job).unwrap();
    let legacy = String::from_utf8(bytes).unwrap();
    assert!(!legacy.contains("policy_change"));
    assert!(!legacy.contains("policy_revision"));
    assert_eq!(
        pve_dev::control::host::plan(pve_dev::control::JobKind::EnsureUser).unwrap(),
        &[Step::Control, Step::Dataset, Step::Storage]
    );
}

#[tokio::test]
async fn actual_usage_and_per_vm_unbacked_margin_are_additive_not_competing_floors() {
    for two in [false, true] {
        let (lab, host) = Lab::new().await;
        lab.accepted(lab.update("grow", 512));
        lab.finish(&host).await;
        lab.accepted(lab.create("one"));
        lab.finish(&host).await;
        if two {
            lab.accepted(lab.create("two"));
            lab.finish(&host).await;
        }
        lab.edit(|world| {
            if two {
                world
                    .datasets
                    .get_mut("labpool/control/erin/vm-9001-disk-0")
                    .unwrap()
                    .insert("refreservation".into(), (100 * MIB).to_string());
            }
            world
                .datasets
                .get_mut("labpool/control/erin")
                .unwrap()
                .insert(
                    "extra_used".into(),
                    ((if two { 49 } else { 25 }) * MIB).to_string(),
                );
        });
        let target = if two { 192 } else { 64 };
        let op = lab.accepted(lab.update("margin", target));
        lab.finish(&host).await;
        let record = lab.store.read().unwrap().operations[op["operation_id"].as_str().unwrap()]
            .public
            .clone();
        assert_eq!(record.status, OperationStatus::Blocked);
        assert!(record.error.unwrap().contains("metadata allowance"));
        assert_eq!(
            lab.snapshot().datasets["labpool/control/erin"]["quota"],
            (512 * MIB).to_string()
        );
    }
}

#[tokio::test]
async fn partial_contraction_usage_drift_blocks_without_releasing_catalog_claims() {
    let (lab, host) = Lab::new().await;
    lab.accepted(lab.create("one"));
    lab.finish(&host).await;
    let op = lab.accepted(lab.update("partial", 64));
    let id = op["operation_id"].as_str().unwrap();
    host.submit(lab.store.read().unwrap().operations[id].job.clone())
        .unwrap();
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::PolicyFirst,
            point: FaultPoint::AfterEffect,
        })
    });
    assert!(host.run_pending().await.is_err());
    drop(host);
    lab.edit(|world| {
        world
            .datasets
            .get_mut("labpool/control/erin")
            .unwrap()
            .insert("extra_used".into(), (25 * MIB).to_string());
    });
    let host = lab.host().await;
    lab.finish(&host).await;
    assert_eq!(
        lab.store.read().unwrap().operations[id].public.status,
        OperationStatus::Blocked
    );
    assert_eq!(lab.user().budget_mib, 128);
    assert!(!lab.user().ready);
    let properties = &lab.snapshot().datasets["labpool/control/erin"];
    assert_eq!(properties["quota"], (128 * MIB).to_string());
    assert_eq!(properties["reservation"], (64 * MIB).to_string());
}

#[tokio::test]
async fn members_cannot_change_any_policy_and_concurrent_host_claims_do_not_overcommit() {
    let (lab, host) = Lab::new().await;
    lab.accepted(Request::CreateUser {
        request_id: "new-frank".into(),
        user_id: "frank".into(),
        budget_mib: 128,
        min_memory_budget_mib: 256,
        max_memory_budget_mib: 1024,
        max_cores: 8,
        max_vms: 4,
    });
    lab.finish(&host).await;
    let peer = SecretKey::generate().public().to_string();
    lab.accepted(Request::EnrollDevice {
        user_id: "frank".into(),
        endpoint_id: peer.clone(),
        label: "frank".into(),
    });
    assert_eq!(
        lab.store
            .handle(&peer, lab.update("forbidden", 256))
            .unwrap_err()
            .code,
        "forbidden"
    );
    let before = lab.store.read().unwrap();
    let frank = before.users["frank"].clone();
    let mut policy = frank.policy();
    policy.budget_mib = 1400;
    let a = lab.update("a", 1400);
    let b = Request::UpdateUserPolicy {
        request_id: "b".into(),
        user_id: "frank".into(),
        expected_revision: 0,
        policy,
    };
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            lab.store.handle(&lab.admin, a)
        });
        let right = scope.spawn(|| {
            barrier.wait();
            lab.store.handle(&lab.admin, b)
        });
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    lab.finish(&host).await;
    assert!(
        lab.store
            .read()
            .unwrap()
            .users
            .values()
            .map(|user| user.budget_mib)
            .sum::<u64>()
            <= 2048
    );
}

#[tokio::test]
async fn allocation_and_policy_race_accepts_only_one_owner_intent() {
    let (lab, _host) = Lab::new().await;
    let barrier = std::sync::Barrier::new(2);
    let update = lab.update("policy", 256);
    let vm = lab.create("racing-vm");
    let results = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            lab.store.handle(&lab.admin, update)
        });
        let right = scope.spawn(|| {
            barrier.wait();
            lab.store.handle(&lab.admin, vm)
        });
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
}

#[tokio::test]
async fn low_pool_headroom_allows_a_nonallocating_reduction_but_blocks_expansion() {
    for (budget, expected) in [
        (64, OperationStatus::Verified),
        (256, OperationStatus::Blocked),
    ] {
        let (lab, host) = Lab::new().await;
        //640MiB minus128MiB reservation leaves512; extra sibling use reduces it below floor.
        lab.edit(|world| {
            world.pool_size_mib = 600;
        });
        let op = lab.accepted(lab.update("pressure", budget));
        lab.finish(&host).await;
        assert_eq!(
            lab.store.read().unwrap().operations[op["operation_id"].as_str().unwrap()]
                .public
                .status,
            expected
        );
        if budget == 64 {
            assert_eq!(lab.user().budget_mib, 64);
            assert!(lab.user().ready)
        } else {
            assert_eq!(lab.user().budget_mib, 128);
            assert!(!lab.user().ready)
        }
    }
}

#[tokio::test]
async fn legacy_pending_helper_plan_and_original_owner_policy_survive_upgrade() {
    let (lab, host) = Lab::new().await;
    let op = lab.accepted(lab.create("legacy-pending"));
    let id = op["operation_id"].as_str().unwrap();
    let frozen = lab.store.read().unwrap().operations[id].job.clone();
    let fingerprint = pve_dev::control::host::fingerprint(&frozen).unwrap();
    host.submit(frozen).unwrap();
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Vm,
            point: FaultPoint::BeforeEffect,
        })
    });
    assert!(host.run_pending().await.is_err());
    drop(host);
    // Emulate the previous schema1 owner record, which had no stored policy.
    {
        use redb::TableDefinition;
        let db = redb::Database::open(lab.root.path().join("helper/host-journal.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(TableDefinition::<u8, &[u8]>::new("host_journal_v1"))
                .unwrap();
            let bytes = table.get(0).unwrap().unwrap().value().to_vec();
            let mut journal: Value = serde_json::from_slice(&bytes).unwrap();
            journal["owners"]["erin"]
                .as_object_mut()
                .unwrap()
                .remove("policy");
            assert_eq!(journal["schema"], 1);
            let bytes = serde_json::to_vec(&journal).unwrap();
            table.insert(0, bytes.as_slice()).unwrap();
        }
        tx.commit().unwrap();
    }
    lab.edit(World::new_boot);
    let host = lab.host().await;
    lab.finish(&host).await;
    let history = host.journal().unwrap();
    let record = &history.jobs[id];
    assert_eq!(record.status, OperationStatus::Verified);
    assert_eq!(record.fingerprint, fingerprint);
    assert_eq!(
        record.steps.iter().map(|r| r.step).collect::<Vec<_>>(),
        pve_dev::control::host::plan(pve_dev::control::JobKind::Create).unwrap()
    );
    assert!(history.owners["erin"].policy.is_none());
    lab.accepted(lab.update("first-after-upgrade", 256));
    lab.finish(&host).await;
    assert_eq!(lab.user().policy_revision, 1);
    assert!(host.journal().unwrap().owners["erin"].policy.is_some());
}
