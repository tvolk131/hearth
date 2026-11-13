use iroh::SecretKey;
use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, MIB, SimulatedWorld, Step, World},
    protocol::*,
    store::{Binding, Store, disk_charge},
};
use serde_json::{Value, json};
use std::path::PathBuf;

const ENV: &str = "1123456789abcdef0123456789abcdef";
const ROOT: &str = "labpool/control/alice/vm-9001-disk-0";

#[test]
fn production_size_limits_accept_750_gib_only_under_explicit_limits() {
    use pve_dev::control::{shrink::ShrinkSpec, vm_change::GrowthSpec};
    let size = 750 * 1024;
    let policy = ResourcePolicy {
        budget_mib: size + 4096,
        min_memory_budget_mib: 512,
        max_memory_budget_mib: 1024,
        max_cores: 2,
        max_vms: 1,
    };
    assert!(!policy.valid());
    let limits = ResourceLimits {
        disk_mib: 1024 * 1024,
        budget_mib: 2 * 1024 * 1024,
        min_memory_mib: 4096,
        memory_mib: 8192,
        cores: 16,
    };
    assert!(policy.valid_with_limits(limits));
    let growth = GrowthSpec {
        previous_disk_mib: 500 * 1024,
        target_disk_mib: size,
        root_partition_uuid: None,
    };
    assert!(growth.valid());
    let shrink = ShrinkSpec {
        previous_disk_mib: size,
        target_disk_mib: 500 * 1024,
        preparation_id: None,
        root_partition_uuid: None,
    };
    assert!(shrink.valid());
    assert_eq!(shrink.workspace_mib(), 1_344_064);
    assert!(
        !GrowthSpec {
            target_disk_mib: 16 * 1024 * 1024 + 1,
            ..growth
        }
        .valid()
    );
}

struct Lab {
    directory: tempfile::TempDir,
    store: Store,
    admin: String,
    member: String,
    world: PathBuf,
}
impl Lab {
    async fn new() -> (Self, Host) {
        let directory = tempfile::tempdir().unwrap();
        let admin = SecretKey::generate().public().to_string();
        let member = SecretKey::generate().public().to_string();
        let store = Store::create(
            &directory.path().join("state.redb"),
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
        let world = directory.path().join("world.json");
        let lab = Self {
            directory,
            store,
            admin,
            member,
            world,
        };
        let host = lab.open().await;
        lab.edit(|world| world.pool_size_mib = 24576);
        lab.flush(&host).await;
        lab.send(
            json!({"action":"create_user","request_id":"alice","user_id":"alice","budget_mib":8192,
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
            self.directory.path().join("helper"),
            ENV,
            self.world.clone(),
        ))
        .await
        .unwrap()
    }
    fn edit(&self, f: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|world| {
                f(world);
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
    fn growth(&self, request: &str, old: u64, target: u64) -> Request {
        serde_json::from_value(json!({"action":"grow_vm","request_id":request,"vm_id":9001,
            "expected_disk_mib":old,"target_disk_mib":target,"confirm_downtime":true}))
        .unwrap()
    }
    fn accept(&self, request: &str, old: u64, target: u64) -> Value {
        self.store
            .handle(&self.admin, self.growth(request, old, target))
            .unwrap()
    }
    fn action(&self, request: &str, kind: &str) -> Value {
        self.send(json!({"action":"vm_action","request_id":request,"vm_id":9001,"kind":kind}))
    }
    async fn flush(&self, host: &Host) {
        for pending in self.store.pending().unwrap() {
            host.submit(pending.job.clone()).unwrap();
            host.run_pending().await.unwrap();
            let journal = host.journal().unwrap();
            let job = &journal.jobs[&pending.public.operation_id];
            assert_eq!(job.status, OperationStatus::Verified, "{:?}", job.error);
            self.store
                .progress(
                    &pending.public.operation_id,
                    job.status.clone(),
                    &job.step,
                    job.error.clone(),
                )
                .unwrap();
        }
    }
}

#[tokio::test]
async fn growth_repeats_across_restarts_without_replacing_guest_or_disk_identity() {
    let (lab, mut host) = Lab::new().await;
    let original = lab.world();
    let original_creator = host.journal().unwrap().machines[&9001]
        .create_operation
        .clone();
    for (round, (old, target)) in [(4096, 6144), (6144, 7168)].into_iter().enumerate() {
        let request = format!("growth-{round}");
        let accepted = lab.accept(&request, old, target);
        assert_eq!(
            lab.store
                .handle(&lab.admin, lab.growth(&request, old, target))
                .unwrap(),
            accepted
        );
        lab.flush(&host).await;
        let current = lab.world();
        for key in ["guid", "createtxg", "org.pve-dev:image"] {
            assert_eq!(current.datasets[ROOT][key], original.datasets[ROOT][key]);
        }
        assert_eq!(
            current.datasets[ROOT]["volsize"],
            (target * MIB).to_string()
        );
        assert!(
            current.datasets[ROOT]["refreservation"]
                .parse::<u64>()
                .unwrap()
                >= target * MIB
        );
        for key in ["smbios1", "vmgenid", "ide2", "name", "cores"] {
            assert_eq!(
                current.vms[&9001].config.get(key),
                original.vms[&9001].config.get(key)
            );
        }
        assert!(!current.vms[&9001].config.contains_key("lock"));
        assert!(!current.datasets[ROOT].contains_key("org.pve-dev:grow"));
        assert_eq!(lab.store.read().unwrap().vms[&9001].disk_mib, target);
        assert_eq!(
            host.journal().unwrap().machines[&9001].profile.disk_mib,
            target
        );
        assert_eq!(
            host.journal().unwrap().machines[&9001].create_operation,
            original_creator
        );
        assert_eq!(lab.store.read().unwrap().users["alice"].budget_mib, 8192);
        drop(host);
        host = lab.open().await;
        lab.action(&format!("start-{round}"), "start");
        lab.flush(&host).await;
        lab.action(&format!("shutdown-{round}"), "shutdown");
        lab.flush(&host).await;
    }
    lab.action("delete-grown", "delete");
    lab.flush(&host).await;
    assert!(!lab.world().datasets.contains_key(ROOT));
    assert!(lab.world().vms.is_empty());
}

#[tokio::test]
async fn growth_plan_is_readonly_and_refusals_preserve_admission() {
    let (lab, host) = Lab::new().await;
    let before = lab.store.read().unwrap();
    let plan = lab.send(json!({"action":"plan_vm_growth","vm_id":9001,"target_disk_mib":6144}));
    let mut target = before.vms[&9001].clone();
    target.disk_mib = 6144;
    assert_eq!(
        plan["additional_charge_mib"],
        disk_charge(&target) - disk_charge(&before.vms[&9001])
    );
    assert_eq!(
        lab.store.read().unwrap().operations.len(),
        before.operations.len()
    );
    assert!(
        lab.store
            .handle(&lab.member, lab.growth("member", 4096, 6144))
            .is_err()
    );
    for (old, target) in [(3072, 6144), (4096, 4096), (4096, 3072), (4096, 8193)] {
        assert!(
            lab.store
                .handle(&lab.admin, lab.growth("invalid", old, target))
                .is_err()
        );
    }
    let unconfirmed = serde_json::from_value(
        json!({"action":"grow_vm","request_id":"unconfirmed","vm_id":9001,
        "expected_disk_mib":4096,"target_disk_mib":6144,"confirm_downtime":false}),
    )
    .unwrap();
    assert!(lab.store.handle(&lab.admin, unconfirmed).is_err());
    lab.action("start-before-growth", "start");
    lab.flush(&host).await;
    assert!(
        lab.store
            .handle(&lab.admin, lab.growth("running", 4096, 6144))
            .is_err()
    );
}

#[tokio::test]
async fn growth_freezes_mutations_and_retains_target_charge_after_ambiguous_extension() {
    let (lab, host) = Lab::new().await;
    let accepted = lab.accept("grow", 4096, 6144);
    let id = accepted["operation_id"].as_str().unwrap();
    let job = lab
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|r| r.public.operation_id == id)
        .unwrap()
        .job;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::GrowVolume,
            point: FaultPoint::AfterEffect,
        })
    });
    host.submit(job).unwrap();
    assert!(host.run_pending().await.is_err());
    drop(host);
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    let journal = host.journal().unwrap();
    let job = &journal.jobs[id];
    assert_eq!(job.status, OperationStatus::Blocked);
    lab.store
        .progress(id, job.status.clone(), &job.step, job.error.clone())
        .unwrap();
    assert_eq!(
        lab.world().datasets[ROOT]["volsize"],
        (6144 * MIB).to_string()
    );
    assert_eq!(lab.store.read().unwrap().vms[&9001].disk_mib, 4096);
    let mut policy = lab.store.read().unwrap().users["alice"].policy();
    policy.budget_mib = 5120;
    assert!(
        lab.store
            .handle(
                &lab.admin,
                Request::UpdateUserPolicy {
                    request_id: "contract-pending".into(),
                    user_id: "alice".into(),
                    expected_revision: 0,
                    policy
                }
            )
            .is_err()
    );
    assert!(
        lab.store
            .handle(
                &lab.admin,
                serde_json::from_value(json!({"action":"vm_action","request_id":"delete-pending",
        "vm_id":9001,"kind":"delete"}))
                .unwrap()
            )
            .is_err()
    );
    assert_eq!(lab.world().vms[&9001].config["lock"], "migrate");
}

#[tokio::test]
async fn growth_crash_boundaries_block_ambiguous_effects_on_same_and_new_boots() {
    for step in [
        Step::GrowPrepare,
        Step::GrowVolume,
        Step::GrowPartition,
        Step::GrowFilesystem,
        Step::GrowUnlock,
    ] {
        for point in [
            FaultPoint::BeforeEffect,
            FaultPoint::AfterEffect,
            FaultPoint::AfterReceipt,
        ] {
            for reboot in [false, true] {
                let (lab, host) = Lab::new().await;
                let guid = lab.world().datasets[ROOT]["guid"].clone();
                let accepted = lab.accept("grow", 4096, 6144);
                let id = accepted["operation_id"].as_str().unwrap();
                let job = lab
                    .store
                    .pending()
                    .unwrap()
                    .into_iter()
                    .find(|r| r.public.operation_id == id)
                    .unwrap()
                    .job;
                lab.edit(|world| world.faults.push(Fault { step, point }));
                host.submit(job.clone()).unwrap();
                assert!(host.run_pending().await.is_err());
                drop(host);
                if reboot {
                    lab.edit(World::new_boot);
                }
                let host = lab.open().await;
                host.run_pending().await.unwrap();
                let journal = host.journal().unwrap();
                let result = &journal.jobs[id];
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
                assert_eq!(lab.world().datasets[ROOT]["guid"], guid);
                assert_eq!(host.submit(job).unwrap().status, wanted);
                let before = serde_json::to_value(lab.world()).unwrap();
                host.run_pending().await.unwrap();
                assert_eq!(
                    serde_json::to_value(lab.world()).unwrap(),
                    before,
                    "blocked growth replayed effects"
                );
            }
        }
    }
}
