use iroh::SecretKey;
use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, HostRequest, Phase, SimulatedWorld, Step, World},
    protocol::*,
    store::{Binding, Store},
    vm_change::RecoveryKind,
};
use serde_json::{Value, json};

const ENV: &str = "a123456789abcdef0123456789abcdef";

struct Lab {
    dir: tempfile::TempDir,
    store: Store,
    admin: String,
}

impl Lab {
    async fn new() -> (Self, Host) {
        let dir = tempfile::tempdir().unwrap();
        let admin = SecretKey::generate().public().to_string();
        let store = Store::create(
            &dir.path().join("state.redb"),
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
        let lab = Self { dir, store, admin };
        let host = lab.open().await;
        lab.edit(|w| w.pool_size_mib = 65536);
        lab.flush(&host).await;
        lab.send(
            json!({"action":"create_user","request_id":"owner","user_id":"alice","budget_mib":8192,
            "min_memory_budget_mib":512,"max_memory_budget_mib":1024,"max_cores":2,"max_vms":1}),
        );
        lab.flush(&host).await;
        lab.send(json!({"action":"create_vm","request_id":"vm","name":"review","owner":"alice",
            "disk_mib":4096,"memory_mib":512,"min_memory_mib":128,"cores":1,
            "guest":{"image_sha256":"a".repeat(64),"ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}}));
        lab.flush(&host).await;
        (lab, host)
    }
    async fn open(&self) -> Host {
        Host::open(HostConfig::simulated(
            self.dir.path().join("helper"),
            ENV,
            self.dir.path().join("world.json"),
        ))
        .await
        .unwrap()
    }
    fn edit(&self, change: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.dir.path().join("world.json"))
            .edit(|w| {
                change(w);
                Ok(())
            })
            .unwrap();
    }
    fn send(&self, value: Value) -> Value {
        self.store
            .handle(&self.admin, serde_json::from_value(value).unwrap())
            .unwrap()
    }
    async fn flush(&self, host: &Host) {
        for _ in 0..20 {
            let pending = self.store.pending().unwrap();
            if pending.is_empty() {
                return;
            }
            for record in pending {
                host.submit(record.job).unwrap();
                host.run_pending().await.unwrap();
                self.sync(host, &record.public.operation_id);
            }
        }
        panic!("pending work did not settle");
    }
    fn sync(&self, host: &Host, id: &str) {
        let journal = host.journal().unwrap();
        let r = &journal.jobs[id];
        self.store
            .progress(id, r.status.clone(), &r.step, r.error.clone())
            .unwrap();
    }
    async fn inspect(&self, host: &Host, id: &str) -> Value {
        host.handle(HostRequest::InspectOperation {
            environment: ENV.into(),
            operation_id: id.into(),
        })
        .await
        .unwrap()
        .inspection
        .unwrap()
    }
    async fn recovery(&self, host: &Host, id: &str, name: &str, kind: RecoveryKind) -> String {
        let inspection = self.inspect(host, id).await;
        assert!(
            inspection["allowed_actions"]
                .as_array()
                .unwrap()
                .contains(&serde_json::to_value(&kind).unwrap()),
            "{inspection}"
        );
        self.store
            .queue_reviewed_recovery(
                &self.admin,
                name,
                id,
                inspection["inspection_id"].as_str().unwrap(),
                kind,
                true,
            )
            .unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn crash(&self, host: &Host, id: &str, step: Step) {
        self.edit(|w| {
            w.faults = vec![Fault {
                step,
                point: FaultPoint::AfterEffect,
            }]
        });
        let job = self.store.read().unwrap().operations[id].job.clone();
        host.submit(job).unwrap();
        assert!(host.run_pending().await.is_err());
    }
    async fn reopen_blocked(&self, host: Host, id: &str) -> Host {
        drop(host);
        let host = self.open().await;
        host.run_pending().await.unwrap();
        self.sync(&host, id);
        assert_eq!(
            host.journal().unwrap().jobs[id].status,
            OperationStatus::Blocked
        );
        host
    }
}

#[tokio::test]
async fn a_second_explicit_recovery_can_continue_after_a_recovery_effect_was_committed() {
    for step in [
        Step::GrowVolume,
        Step::GrowFilesystem,
        Step::GrowUnlock,
        Step::ShrinkSwap,
    ] {
        let (lab, host) = Lab::new().await;
        let (kind, target) = if step == Step::ShrinkSwap {
            ("shrink_vm", 3072)
        } else {
            ("grow_vm", 6144)
        };
        let accepted = lab.send(
            json!({"action":kind,"request_id":"resize","vm_id":9001,"expected_disk_mib":4096,
            "target_disk_mib":target,"confirm_downtime":true}),
        );
        let id = accepted["operation_id"].as_str().unwrap().to_owned();
        lab.crash(&host, &id, step).await;
        let host = lab.reopen_blocked(host, &id).await;
        let recovery = lab
            .recovery(
                &host,
                &id,
                "first-recovery",
                RecoveryKind::ResumeVerifiedEffect,
            )
            .await;
        lab.crash(&host, &recovery, Step::Recover).await;
        let host = lab.reopen_blocked(host, &recovery).await;
        let original = host.journal().unwrap().jobs[&id].clone();
        assert_eq!(original.status, OperationStatus::Blocked);
        assert_eq!(
            original
                .steps
                .iter()
                .find(|s| s.step == step)
                .unwrap()
                .phase,
            Phase::Verified
        );
        let second = lab
            .recovery(
                &host,
                &id,
                "second-recovery",
                RecoveryKind::ResumeVerifiedEffect,
            )
            .await;
        lab.flush(&host).await;
        let journal = host.journal().unwrap();
        assert_eq!(
            journal.jobs[&second].status,
            OperationStatus::Verified,
            "{step:?}: {:?}",
            journal.jobs[&second].error
        );
        assert_eq!(
            journal.jobs[&id].status,
            OperationStatus::Verified,
            "{step:?}: {:?}",
            journal.jobs[&id].error
        );
        assert_eq!(lab.store.read().unwrap().vms[&9001].disk_mib, target);
    }
}

#[tokio::test]
async fn an_interrupted_abort_keeps_capacity_until_its_exact_receipt_is_explicitly_adopted() {
    for (kind, step, target) in [
        ("grow_vm", Step::GrowPrepare, 6144),
        ("shrink_vm", Step::ShrinkStage, 3072),
    ] {
        let (lab, host) = Lab::new().await;
        let accepted = lab.send(
            json!({"action":kind,"request_id":"resize","vm_id":9001,"expected_disk_mib":4096,
        "target_disk_mib":target,"confirm_downtime":true}),
        );
        let id = accepted["operation_id"].as_str().unwrap().to_owned();
        lab.crash(&host, &id, step).await;
        let host = lab.reopen_blocked(host, &id).await;
        let recovery = lab
            .recovery(&host, &id, "abort", RecoveryKind::AbortPreparation)
            .await;
        lab.crash(&host, &recovery, Step::Recover).await;
        let host = lab.reopen_blocked(host, &recovery).await;
        let catalog = lab.store.read().unwrap();
        assert!(
            catalog.disk_growths.contains_key(&9001) || catalog.disk_shrinks.contains_key(&9001)
        );
        assert_eq!(catalog.vms[&9001].disk_mib, 4096);
        assert_eq!(catalog.vms[&9001].state, VmState::Blocked);
        assert_eq!(
            catalog.operations[&id].public.status,
            OperationStatus::Blocked
        );
        let plan: Request = serde_json::from_value(
            json!({"action":"vm_action","request_id":"cannot-start","vm_id":9001,"kind":"start"}),
        )
        .unwrap();
        assert!(lab.store.handle(&lab.admin, plan).is_err());
        assert_eq!(
            host.journal().unwrap().jobs[&recovery].status,
            OperationStatus::Blocked
        );
        let second = lab
            .recovery(&host, &id, "adopt-abort", RecoveryKind::AbortPreparation)
            .await;
        lab.flush(&host).await;
        let journal = host.journal().unwrap();
        let catalog = lab.store.read().unwrap();
        assert_eq!(
            journal.jobs[&second].status,
            OperationStatus::Verified,
            "{kind}: {:?}",
            journal.jobs[&second].error
        );
        assert_eq!(
            catalog.operations[&id].public.status,
            OperationStatus::Cancelled
        );
        assert!(
            !catalog.disk_growths.contains_key(&9001) && !catalog.disk_shrinks.contains_key(&9001)
        );
        assert_eq!(catalog.vms[&9001].disk_mib, 4096);
        assert_eq!(catalog.vms[&9001].state, VmState::Stopped);
    }
}

#[tokio::test]
async fn rebooted_inspection_or_replaced_owner_allocation_cannot_release_claims() {
    for drift in ["boot", "owner"] {
        let (lab, host) = Lab::new().await;
        let accepted = lab.send(
            json!({"action":"grow_vm","request_id":"resize","vm_id":9001,"expected_disk_mib":4096,
            "target_disk_mib":6144,"confirm_downtime":true}),
        );
        let id = accepted["operation_id"].as_str().unwrap().to_owned();
        lab.crash(&host, &id, Step::GrowPrepare).await;
        let host = lab.reopen_blocked(host, &id).await;
        let recovery = lab
            .recovery(&host, &id, "reviewed-abort", RecoveryKind::AbortPreparation)
            .await;
        lab.edit(|world| {
            if drift == "boot" {
                world.boot_id = "next-boot".into()
            } else {
                world
                    .datasets
                    .get_mut("labpool/control/alice")
                    .unwrap()
                    .insert("guid".into(), "99999999".into());
            }
        });
        lab.flush(&host).await;
        assert_eq!(
            host.journal().unwrap().jobs[&recovery].status,
            OperationStatus::Blocked,
            "{drift}"
        );
        let catalog = lab.store.read().unwrap();
        assert!(catalog.disk_growths.contains_key(&9001));
        assert_eq!(catalog.vms[&9001].disk_mib, 4096);
        assert_eq!(
            catalog.operations[&id].public.status,
            OperationStatus::Blocked
        );
    }
}

#[tokio::test]
async fn planned_continuation_refuses_a_missing_historical_resize_receipt() {
    let (lab, host) = Lab::new().await;
    let accepted = lab.send(
        json!({"action":"grow_vm","request_id":"resize","vm_id":9001,"expected_disk_mib":4096,
        "target_disk_mib":6144,"confirm_downtime":true}),
    );
    let id = accepted["operation_id"].as_str().unwrap().to_owned();
    lab.crash(&host, &id, Step::GrowVolume).await;
    let host = lab.reopen_blocked(host, &id).await;
    let recovery = lab
        .recovery(&host, &id, "first", RecoveryKind::ResumeVerifiedEffect)
        .await;
    lab.crash(&host, &recovery, Step::Recover).await;
    let host = lab.reopen_blocked(host, &recovery).await;
    let second = lab
        .recovery(
            &host,
            &id,
            "missing-receipt",
            RecoveryKind::ResumeVerifiedEffect,
        )
        .await;
    lab.edit(|world| {
        let key = format!("simulated:growth-{id}");
        let root = world
            .datasets
            .get_mut("labpool/control/alice/vm-9001-disk-0")
            .unwrap();
        let mut state: Value = serde_json::from_str(&root[&key]).unwrap();
        state["receipts"]
            .as_object_mut()
            .unwrap()
            .remove("grow_prepare");
        root.insert(key, state.to_string());
    });
    lab.flush(&host).await;
    let journal = host.journal().unwrap();
    assert_eq!(journal.jobs[&second].status, OperationStatus::Blocked);
    assert!(
        journal.jobs[&second]
            .error
            .as_deref()
            .unwrap()
            .contains("historical resize receipt")
    );
    assert!(lab.store.read().unwrap().disk_growths.contains_key(&9001));
}
