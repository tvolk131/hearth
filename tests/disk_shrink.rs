use iroh::SecretKey;
use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, MIB, SimulatedWorld, Step, World},
    protocol::*,
    shrink::ShrinkSpec,
    store::{Binding, Store},
};
use serde_json::{Value, json};
use std::path::PathBuf;

const ENV: &str = "0123456789abcdef0123456789abcdef";
const ROOT: &str = "labpool/control/alice/vm-9001-disk-0";
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
        let member = SecretKey::generate().public().to_string();
        let store = Store::create(
            &root.path().join("state.redb"),
            Binding {
                environment: ENV.into(),
                backend: "simulated".into(),
                disk_capacity_mib: 16384,
                min_memory_capacity_mib: 2048,
                guest_images: vec!["a".repeat(64)],
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
            member,
            world,
        };
        let host = lab.open().await;
        lab.edit(|w| w.pool_size_mib = 24576);
        lab.flush(&host).await;
        lab.send(
            json!({"action":"create_user","request_id":"alice","user_id":"alice","budget_mib":6144,
            "min_memory_budget_mib":512,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":2}),
        );
        lab.flush(&host).await;
        lab.send(json!({"action":"enroll_device","user_id":"alice","endpoint_id":lab.member,"label":"member"}));
        lab.send(json!({"action":"create_vm","request_id":"guest","name":"dev-alice","owner":"alice","disk_mib":4096,
            "memory_mib":512,"min_memory_mib":128,"cores":1,"guest":{"image_sha256":"a".repeat(64),
            "ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}}));
        lab.flush(&host).await;
        (lab, host)
    }
    async fn open(&self) -> Host {
        Host::open(HostConfig::simulated(
            self.root.path().join("helper"),
            ENV,
            self.world.clone(),
        ))
        .await
        .unwrap()
    }
    fn edit(&self, f: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|w| {
                f(w);
                Ok(())
            })
            .unwrap();
    }
    fn world(&self) -> World {
        SimulatedWorld::new(self.world.clone()).load().unwrap()
    }
    fn send(&self, value: Value) -> Value {
        self.store
            .handle(&self.admin, serde_json::from_value(value).unwrap())
            .unwrap()
    }
    async fn flush(&self, host: &Host) {
        for r in self.store.pending().unwrap() {
            host.submit(r.job.clone()).unwrap();
            host.run_pending().await.unwrap();
            let j = host.journal().unwrap();
            let proof = &j.jobs[&r.public.operation_id];
            assert_eq!(proof.status, OperationStatus::Verified, "{:?}", proof.error);
            self.store
                .progress(
                    &r.public.operation_id,
                    proof.status.clone(),
                    &proof.step,
                    proof.error.clone(),
                )
                .unwrap();
        }
    }
    fn request(&self) -> Request {
        serde_json::from_value(
            json!({"action":"shrink_vm","request_id":"shrink","vm_id":9001,
        "expected_disk_mib":4096,"target_disk_mib":3072,"confirm_downtime":true}),
        )
        .unwrap()
    }
    fn shrink(&self) -> Value {
        self.store.handle(&self.admin, self.request()).unwrap()
    }
    fn finish(&self, prep: &str) -> Value {
        self.send(
            json!({"action":"finish_vm_shrink","request_id":"finish","vm_id":9001,
        "preparation_id":prep,"confirm_guest_verified":true,"confirm_delete_recovery":true}),
        )
    }
    fn action(&self, id: &str, kind: &str) -> Value {
        self.send(json!({"action":"vm_action","request_id":id,"vm_id":9001,"kind":kind}))
    }
}

#[tokio::test]
async fn prepared_guest_lifecycle_finish_and_quota_release_preserve_provenance() {
    let (lab, host) = Lab::new().await;
    let original = lab.world();
    let accepted = lab.shrink();
    let prep = accepted["operation_id"].as_str().unwrap();
    assert_eq!(
        lab.store.handle(&lab.admin, lab.request()).unwrap(),
        accepted
    );
    lab.flush(&host).await;
    let current = lab.world();
    assert_ne!(
        current.datasets[ROOT]["guid"],
        original.datasets[ROOT]["guid"]
    );
    let work = format!("labpool/pve-dev-shrink-{prep}");
    assert_eq!(
        current.datasets[&format!("{work}/original")]["guid"],
        original.datasets[ROOT]["guid"]
    );
    assert_eq!(current.datasets[ROOT]["volsize"], (3072 * MIB).to_string());
    assert_eq!(
        current.vms[&9001].config["smbios1"],
        original.vms[&9001].config["smbios1"]
    );
    assert_eq!(
        current.vms[&9001].config["ide2"],
        original.vms[&9001].config["ide2"]
    );
    assert_eq!(lab.store.read().unwrap().vms[&9001].disk_mib, 3072);
    assert!(lab.store.read().unwrap().disk_shrinks[&9001].prepared);
    let mut policy = lab.store.read().unwrap().users["alice"].policy();
    policy.budget_mib = 4096;
    let update = Request::UpdateUserPolicy {
        request_id: "contract".into(),
        user_id: "alice".into(),
        expected_revision: 0,
        policy,
    };
    assert!(lab.store.handle(&lab.admin, update.clone()).is_err());
    assert!(
        lab.store
            .handle(
                &lab.admin,
                Request::VmAction {
                    request_id: "delete-early".into(),
                    vm_id: 9001,
                    kind: VmAction::Delete
                }
            )
            .is_err()
    );
    lab.action("start-target", "start");
    lab.flush(&host).await;
    assert!(
        lab.store
            .handle(
                &lab.admin,
                serde_json::from_value(
                    json!({"action":"finish_vm_shrink","request_id":"running-finish","vm_id":9001,
        "preparation_id":prep,"confirm_guest_verified":true,"confirm_delete_recovery":true})
                )
                .unwrap()
            )
            .is_err()
    );
    lab.action("stop-target", "shutdown");
    lab.flush(&host).await;
    lab.finish(prep);
    lab.flush(&host).await;
    assert!(lab.store.read().unwrap().disk_shrinks.is_empty());
    assert!(host.journal().unwrap().disk_shrinks.is_empty());
    assert!(!lab.world().datasets.keys().any(|x| x.starts_with(&work)));
    lab.store.handle(&lab.admin, update).unwrap();
    lab.flush(&host).await;
    assert_eq!(
        lab.world().datasets["labpool/control/alice"]["reservation"],
        (4096 * MIB).to_string()
    );
    lab.action("delete-final", "delete");
    lab.flush(&host).await;
    assert_eq!(lab.store.read().unwrap().schema, 5);
    assert_eq!(host.journal().unwrap().schema, 5);
}

#[tokio::test]
async fn repeated_shrinks_and_restarts_preserve_creation_provenance_and_release_each_hold() {
    let (lab, mut host) = Lab::new().await;
    let original = lab.world();
    let creator = host.journal().unwrap().machines[&9001]
        .create_operation
        .clone();
    for (round, (source, target)) in [(4096u64, 2560u64), (2560, 2048)].into_iter().enumerate() {
        let before = lab.world();
        let accepted = lab.send(
            json!({"action":"shrink_vm","request_id":format!("repeat-prep-{round}"),"vm_id":9001,
            "expected_disk_mib":source,"target_disk_mib":target,"confirm_downtime":true}),
        );
        let prep = accepted["operation_id"].as_str().unwrap();
        lab.flush(&host).await;
        let work = format!("labpool/pve-dev-shrink-{prep}");
        assert_eq!(
            lab.world().datasets[&format!("{work}/original")]["guid"],
            before.datasets[ROOT]["guid"]
        );
        assert_ne!(
            lab.world().datasets[ROOT]["guid"],
            before.datasets[ROOT]["guid"]
        );
        drop(host);
        host = lab.open().await;
        lab.action(&format!("repeat-start-{round}"), "start");
        lab.flush(&host).await;
        lab.action(&format!("repeat-shutdown-{round}"), "shutdown");
        lab.flush(&host).await;
        lab.send(json!({"action":"finish_vm_shrink","request_id":format!("repeat-finish-{round}"),"vm_id":9001,
            "preparation_id":prep,"confirm_guest_verified":true,"confirm_delete_recovery":true}));
        lab.flush(&host).await;
        assert!(lab.store.read().unwrap().disk_shrinks.is_empty());
        assert!(host.journal().unwrap().disk_shrinks.is_empty());
        assert!(
            !lab.world()
                .datasets
                .keys()
                .any(|name| name.starts_with(&work))
        );
        assert_eq!(
            lab.world().datasets[ROOT]["volsize"],
            (target * MIB).to_string()
        );
        assert_eq!(
            lab.world().vms[&9001].config["smbios1"],
            original.vms[&9001].config["smbios1"]
        );
        assert_eq!(
            lab.world().vms[&9001].config["ide2"],
            original.vms[&9001].config["ide2"]
        );
        drop(host);
        host = lab.open().await;
        assert_eq!(
            host.journal().unwrap().machines[&9001].create_operation,
            creator
        );
    }
    lab.action("repeat-delete", "delete");
    lab.flush(&host).await;
    assert!(!lab.world().datasets.contains_key(ROOT));
}

#[tokio::test]
async fn member_stale_running_unconfirmed_and_capacity_refusals_create_no_intents() {
    let (lab, host) = Lab::new().await;
    let before = lab.store.read().unwrap().operations.len();
    assert_eq!(
        lab.store
            .handle(&lab.member, lab.request())
            .unwrap_err()
            .code,
        "forbidden"
    );
    for (field, value) in [
        ("expected_disk_mib", json!(8192)),
        ("target_disk_mib", json!(4096)),
        ("target_disk_mib", json!(1024)),
        ("confirm_downtime", json!(false)),
    ] {
        let mut req = serde_json::to_value(lab.request()).unwrap();
        req[field] = value;
        assert!(
            lab.store
                .handle(&lab.admin, serde_json::from_value(req).unwrap())
                .is_err()
        );
    }
    assert_eq!(before, lab.store.read().unwrap().operations.len());
    lab.action("start", "start");
    lab.flush(&host).await;
    assert!(lab.store.handle(&lab.admin, lab.request()).is_err());
    lab.action("stop", "stop");
    lab.flush(&host).await;
    lab.edit(|w| w.pool_size_mib = 8192);
    let accepted = lab.shrink();
    let r = lab
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|r| r.public.operation_id == accepted["operation_id"])
        .unwrap();
    host.submit(r.job).unwrap();
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs[accepted["operation_id"].as_str().unwrap()].status,
        OperationStatus::Blocked
    );
    assert!(!lab.world().vms[&9001].config.contains_key("lock"));
}

#[tokio::test]
async fn all_prepare_crash_boundaries_retain_original_and_never_replay_uncertain_effects() {
    for step in [Step::ShrinkStage, Step::ShrinkSwap, Step::ShrinkUnlock] {
        for point in [
            FaultPoint::BeforeEffect,
            FaultPoint::AfterEffect,
            FaultPoint::AfterReceipt,
        ] {
            for reboot in [false, true] {
                let (lab, host) = Lab::new().await;
                let original_guid = lab.world().datasets[ROOT]["guid"].clone();
                let accepted = lab.shrink();
                let id = accepted["operation_id"].as_str().unwrap();
                let job = lab
                    .store
                    .pending()
                    .unwrap()
                    .into_iter()
                    .find(|r| r.public.operation_id == id)
                    .unwrap()
                    .job;
                lab.edit(|w| w.faults.push(Fault { step, point }));
                host.submit(job.clone()).unwrap();
                assert!(host.run_pending().await.is_err());
                drop(host);
                if reboot {
                    lab.edit(World::new_boot);
                }
                let host = lab.open().await;
                host.run_pending().await.unwrap();
                let j = host.journal().unwrap();
                let result = &j.jobs[id];
                let wanted = if point == FaultPoint::AfterReceipt {
                    OperationStatus::Verified
                } else {
                    OperationStatus::Blocked
                };
                assert_eq!(
                    result.status, wanted,
                    "{step:?} {point:?} reboot={reboot}: {:?}",
                    result.error
                );
                assert!(
                    lab.world()
                        .datasets
                        .values()
                        .any(|v| v.get("guid") == Some(&original_guid)),
                    "original lost"
                );
                assert_eq!(host.submit(job).unwrap().status, wanted);
                assert!(j.disk_shrinks.contains_key(&9001));
            }
        }
    }
}

#[tokio::test]
async fn finish_crashes_do_not_release_claims_without_verified_deletion() {
    for point in [
        FaultPoint::BeforeEffect,
        FaultPoint::AfterEffect,
        FaultPoint::AfterReceipt,
    ] {
        let (lab, host) = Lab::new().await;
        let p = lab.shrink();
        lab.flush(&host).await;
        let prep = p["operation_id"].as_str().unwrap();
        let finished = lab.finish(prep);
        let id = finished["operation_id"].as_str().unwrap();
        let job = lab
            .store
            .pending()
            .unwrap()
            .into_iter()
            .find(|r| r.public.operation_id == id)
            .unwrap()
            .job;
        lab.edit(|w| {
            w.faults.push(Fault {
                step: Step::ShrinkFinish,
                point,
            })
        });
        host.submit(job).unwrap();
        assert!(host.run_pending().await.is_err());
        drop(host);
        let host = lab.open().await;
        host.run_pending().await.unwrap();
        let j = host.journal().unwrap();
        let r = &j.jobs[id];
        assert_eq!(
            r.status,
            if point == FaultPoint::AfterReceipt {
                OperationStatus::Verified
            } else {
                OperationStatus::Blocked
            }
        );
        lab.store
            .progress(id, r.status.clone(), &r.step, r.error.clone())
            .unwrap();
        assert_eq!(
            lab.store.read().unwrap().disk_shrinks.is_empty(),
            point == FaultPoint::AfterReceipt
        );
    }
}

#[tokio::test]
async fn read_only_plan_holds_and_tampered_claims_are_checked() {
    let (lab, host) = Lab::new().await;
    let before = lab.store.read().unwrap().operations.len();
    let plan = lab.send(json!({"action":"plan_vm_shrink","vm_id":9001,"target_disk_mib":3072}));
    assert_eq!(
        plan["workspace_mib"],
        ShrinkSpec {
            previous_disk_mib: 4096,
            target_disk_mib: 3072,
            preparation_id: None,
            root_partition_uuid: None,
        }
        .workspace_mib()
    );
    assert_eq!(before, lab.store.read().unwrap().operations.len());
    lab.shrink();
    lab.flush(&host).await;
    let mut c = lab.store.read().unwrap();
    c.disk_shrinks.clear();
    assert!(pve_dev::control::store::validate_scope(&c).is_err());
    let mut j = host.journal().unwrap();
    j.disk_shrinks.clear();
    assert!(pve_dev::control::host::validate_scope(&j).is_err());
}

#[tokio::test]
async fn durable_progress_tracks_verified_steps_and_survives_helper_reopen() {
    use pve_dev::control::host::HostRequest;
    let (lab, host) = Lab::new().await;
    let accepted = lab.shrink();
    let id = accepted["operation_id"].as_str().unwrap();
    let job = lab
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|r| r.public.operation_id == id)
        .unwrap()
        .job;
    let queued = host.submit(job).unwrap();
    let q = queued.progress.unwrap();
    assert_eq!((q.completed_steps, q.total_steps), (0, 3));
    assert!(q.started_at.is_none() && q.finished_at.is_none());
    host.run_pending().await.unwrap();
    let completed = host
        .handle(HostRequest::Status {
            environment: ENV.into(),
            operation_id: id.into(),
        })
        .await
        .unwrap();
    let progress = completed.progress.clone().unwrap();
    assert_eq!((progress.completed_steps, progress.total_steps), (3, 3));
    assert!(progress.started_at.unwrap() <= progress.finished_at.unwrap());
    lab.store
        .progress_detailed(
            id,
            completed.status.clone(),
            &completed.step,
            completed.error.clone(),
            completed.progress,
        )
        .unwrap();
    let recorded = lab
        .store
        .handle(
            &lab.admin,
            Request::GetOperation {
                operation_id: id.into(),
            },
        )
        .unwrap();
    assert_eq!(
        recorded["progress"],
        serde_json::to_value(&progress).unwrap()
    );
    let steps = host.journal().unwrap().jobs[id].steps.clone();
    assert!(
        steps
            .iter()
            .all(|s| s.started_at.is_some() && s.finished_at.is_some())
    );
    drop(host);
    let reopened = lab.open().await;
    let after = reopened
        .handle(HostRequest::Status {
            environment: ENV.into(),
            operation_id: id.into(),
        })
        .await
        .unwrap();
    assert_eq!(after.progress, Some(progress));
    let legacy = serde_json::json!({"operation_id":"old","owner":"alice","request_id":"old","kind":"start","vm_id":9001,
      "status":"verified","step":"complete","error":null,"created_at":1});
    assert!(
        serde_json::from_value::<Operation>(legacy)
            .unwrap()
            .progress
            .is_none()
    );
}

async fn blocked_copy_fixture() -> (Lab, Host, String) {
    let (lab, host) = Lab::new().await;
    let accepted = lab.shrink();
    let id = accepted["operation_id"].as_str().unwrap().to_owned();
    let job = lab
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|r| r.public.operation_id == id)
        .unwrap()
        .job;
    lab.edit(|w| {
        w.faults.push(Fault {
            step: Step::ShrinkStage,
            point: FaultPoint::AfterEffect,
        })
    });
    host.submit(job).unwrap();
    assert!(host.run_pending().await.is_err());
    drop(host);
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    let proof = host.journal().unwrap().jobs[&id].clone();
    assert_eq!(proof.status, OperationStatus::Blocked);
    lab.store
        .progress(&id, proof.status, &proof.step, proof.error)
        .unwrap();
    (lab, host, id)
}
async fn inspection(host: &Host, id: &str) -> Value {
    host.handle(pve_dev::control::host::HostRequest::InspectOperation {
        environment: ENV.into(),
        operation_id: id.into(),
    })
    .await
    .unwrap()
    .inspection
    .unwrap()
}

#[tokio::test]
async fn explicit_recovery_accepts_a_proved_effect_without_replaying_the_copy() {
    use pve_dev::control::vm_change::RecoveryKind;
    let (lab, host, id) = blocked_copy_fixture().await;
    let stage = lab.world();
    let original = stage.datasets[ROOT]["guid"].clone();
    let review = inspection(&host, &id).await;
    assert!(
        review["allowed_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("resume_verified_effect"))
    );
    let accepted = lab
        .store
        .queue_reviewed_recovery(
            &lab.admin,
            "resume-copy",
            &id,
            review["inspection_id"].as_str().unwrap(),
            RecoveryKind::ResumeVerifiedEffect,
            true,
        )
        .unwrap();
    lab.flush(&host).await;
    lab.flush(&host).await;
    assert_eq!(lab.store.read().unwrap().vms[&9001].disk_mib, 3072);
    let history = host.journal().unwrap();
    assert_eq!(history.jobs[&id].status, OperationStatus::Verified);
    let work = format!("labpool/pve-dev-shrink-{id}");
    assert_eq!(
        lab.world().datasets[&format!("{work}/original")]["guid"],
        original
    );
    assert_eq!(
        history.jobs[accepted["operation_id"].as_str().unwrap()].status,
        OperationStatus::Verified
    );
    assert_eq!(
        lab.store
            .queue_reviewed_recovery(
                &lab.admin,
                "resume-copy",
                &id,
                review["inspection_id"].as_str().unwrap(),
                RecoveryKind::ResumeVerifiedEffect,
                true
            )
            .unwrap()["operation_id"],
        accepted["operation_id"]
    );
}

#[tokio::test]
async fn explicit_pre_swap_abort_preserves_source_and_releases_only_its_workspace() {
    use pve_dev::control::vm_change::RecoveryKind;
    let (lab, host, id) = blocked_copy_fixture().await;
    let source = lab.world().datasets[ROOT].clone();
    let review = inspection(&host, &id).await;
    assert!(
        review["allowed_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("abort_preparation"))
    );
    lab.store
        .queue_reviewed_recovery(
            &lab.admin,
            "abort-copy",
            &id,
            review["inspection_id"].as_str().unwrap(),
            RecoveryKind::AbortPreparation,
            true,
        )
        .unwrap();
    lab.flush(&host).await;
    let c = lab.store.read().unwrap();
    assert_eq!(c.operations[&id].public.status, OperationStatus::Cancelled);
    assert_eq!(c.vms[&9001].disk_mib, 4096);
    assert!(c.disk_shrinks.is_empty());
    assert_eq!(lab.world().datasets[ROOT], source);
    assert!(
        !lab.world()
            .datasets
            .keys()
            .any(|n| n.contains(&format!("pve-dev-shrink-{id}")))
    );
    drop(host);
    let host = lab.open().await;
    assert!(host.journal().unwrap().disk_shrinks.is_empty());
    assert_eq!(
        host.journal().unwrap().jobs[&id].status,
        OperationStatus::Cancelled
    );
    assert!(!lab.world().vms[&9001].config.contains_key("lock"));
}

#[tokio::test]
async fn stale_recovery_inspection_cannot_release_claims_or_adopt_changed_original() {
    use pve_dev::control::vm_change::RecoveryKind;
    let (lab, host, id) = blocked_copy_fixture().await;
    let review = inspection(&host, &id).await;
    let guid = lab.world().datasets[ROOT]["guid"].clone();
    lab.edit(|w| {
        w.datasets
            .get_mut(ROOT)
            .unwrap()
            .insert("guid".into(), "unexpected-guid".into());
    });
    let queued = lab
        .store
        .queue_reviewed_recovery(
            &lab.admin,
            "stale-resume",
            &id,
            review["inspection_id"].as_str().unwrap(),
            RecoveryKind::ResumeVerifiedEffect,
            true,
        )
        .unwrap();
    let recovery_id = queued["operation_id"].as_str().unwrap();
    let job = lab
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|r| r.public.operation_id == recovery_id)
        .unwrap()
        .job;
    host.submit(job).unwrap();
    host.run_pending().await.unwrap();
    let failed = host.journal().unwrap().jobs[recovery_id].clone();
    assert_eq!(failed.status, OperationStatus::Blocked);
    lab.store
        .progress(recovery_id, failed.status, &failed.step, failed.error)
        .unwrap();
    assert!(lab.store.read().unwrap().disk_shrinks.contains_key(&9001));
    assert_eq!(
        host.journal().unwrap().jobs[&id].status,
        OperationStatus::Blocked
    );
    lab.edit(|w| {
        w.datasets
            .get_mut(ROOT)
            .unwrap()
            .insert("guid".into(), guid);
    });
    let fresh = inspection(&host, &id).await;
    lab.store
        .queue_reviewed_recovery(
            &lab.admin,
            "fresh-resume",
            &id,
            fresh["inspection_id"].as_str().unwrap(),
            RecoveryKind::ResumeVerifiedEffect,
            true,
        )
        .unwrap();
    lab.flush(&host).await;
    lab.flush(&host).await;
    assert_eq!(
        lab.store.read().unwrap().operations[recovery_id]
            .public
            .status,
        OperationStatus::Cancelled
    );
    assert_eq!(
        host.journal().unwrap().jobs[recovery_id].status,
        OperationStatus::Cancelled
    );
    assert_eq!(lab.store.read().unwrap().vms[&9001].disk_mib, 3072);
}
