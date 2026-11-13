//! Independent regression checks for cross-policy accounting and stale helpers.
use std::path::PathBuf;

use iroh::SecretKey;
use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, MIB, SimulatedWorld, Step, World},
    protocol::*,
    store::{Binding, Job, PolicyChange, Store},
};
use serde_json::Value;

const ENV: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn nonstorage_edit_does_not_impose_a_new_disk_headroom_requirement() {
    let (lab, host) = Lab::new().await;
    let mut policy = lab.store.read().unwrap().users["erin"].policy();
    policy.budget_mib = 64;
    lab.accept(lab.policy("initial-unused-contraction", policy));
    lab.finish(&host).await;
    let mut create = Lab::create();
    if let Request::CreateVm { disk_mib, .. } = &mut create {
        *disk_mib = 52; // 52 + 4 cloud-init + 8 metadata = 64 MiB.
    }
    lab.accept(create);
    lab.finish(&host).await;
    lab.edit(|world| {
        world
            .datasets
            .get_mut("labpool/control/erin")
            .unwrap()
            .insert("extra_used".into(), MIB.to_string());
    });
    let before = lab.world().datasets;
    let mut policy = lab.store.read().unwrap().users["erin"].policy();
    policy.max_cores = 9;
    let operation = lab.accept(lab.policy("cores-only-with-unchanged-disk", policy));
    lab.finish(&host).await;
    let catalog = lab.store.read().unwrap();
    let record = &catalog.operations[operation["operation_id"].as_str().unwrap()];
    assert_eq!(
        record.public.status,
        OperationStatus::Verified,
        "{:?}",
        record.public.error
    );
    assert!(catalog.users["erin"].ready);
    assert_eq!(catalog.users["erin"].max_cores, 9);
    assert_eq!(catalog.users["erin"].budget_mib, 64);
    assert_eq!(lab.world().datasets, before);
}

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
                disk_capacity_mib: 384,
                min_memory_capacity_mib: 768,
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

    fn world(&self) -> World {
        SimulatedWorld::new(self.world.clone()).load().unwrap()
    }

    fn edit(&self, action: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|world| {
                action(world);
                Ok(())
            })
            .unwrap();
    }

    fn accept(&self, request: Request) -> Value {
        self.store.handle(&self.admin, request).unwrap()
    }

    async fn finish(&self, host: &Host) {
        for record in self.store.pending().unwrap() {
            host.submit(record.job).unwrap();
            host.run_pending().await.unwrap();
            let job = host.journal().unwrap().jobs[&record.public.operation_id].clone();
            self.store
                .progress(
                    &record.public.operation_id,
                    job.status,
                    &job.step,
                    job.error,
                )
                .unwrap();
        }
    }

    fn add_user(id: &str, disk: u64, memory: u64) -> Request {
        Request::CreateUser {
            request_id: id.into(),
            user_id: id.into(),
            budget_mib: disk,
            min_memory_budget_mib: memory,
            max_memory_budget_mib: memory.max(256),
            max_cores: 2,
            max_vms: 2,
        }
    }

    fn policy(&self, id: &str, policy: ResourcePolicy) -> Request {
        Request::UpdateUserPolicy {
            request_id: id.into(),
            user_id: "erin".into(),
            expected_revision: self.store.read().unwrap().users["erin"].policy_revision,
            policy,
        }
    }

    fn create() -> Request {
        Request::CreateVm {
            request_id: "vm".into(),
            name: "vm".into(),
            disk_mib: 32,
            memory_mib: 128,
            min_memory_mib: 128,
            cores: 1,
            owner: None,
            guest: None,
        }
    }
}

#[tokio::test]
async fn mixed_disk_contraction_and_ram_growth_keep_both_conservative_envelopes() {
    let (lab, host) = Lab::new().await;
    lab.accept(Lab::add_user("alice", 128, 128));
    lab.finish(&host).await;
    let mut target = lab.store.read().unwrap().users["erin"].policy();
    target.budget_mib = 64;
    target.min_memory_budget_mib = 512;
    lab.accept(lab.policy("mixed", target));

    // RAM fits exactly, but this would fit disk only if the unverified
    // contraction were prematurely released: 64 + 128 + 192 == 384.
    assert!(
        lab.store
            .handle(&lab.admin, Lab::add_user("disk-borrower", 192, 128))
            .is_err()
    );
    // Disk fits, but this would fit RAM only if the queued growth were ignored.
    assert!(
        lab.store
            .handle(&lab.admin, Lab::add_user("ram-borrower", 64, 256))
            .is_err()
    );
    lab.finish(&host).await;
    let current = &lab.store.read().unwrap().users["erin"];
    assert!(current.ready);
    assert_eq!(
        (current.budget_mib, current.min_memory_budget_mib),
        (64, 512)
    );
    // Capacity is usable only once the helper and controller verify contraction.
    lab.accept(Lab::add_user("disk-borrower", 192, 128));
}

#[tokio::test]
async fn partial_contraction_rechecks_disk_identity_before_touching_second_property() {
    let (lab, host) = Lab::new().await;
    lab.accept(Lab::create());
    lab.finish(&host).await;
    let mut target = lab.store.read().unwrap().users["erin"].policy();
    target.budget_mib = 64;
    let value = lab.accept(lab.policy("contract", target));
    let id = value["operation_id"].as_str().unwrap();
    let frozen = lab.store.read().unwrap().operations[id].job.clone();
    host.submit(frozen).unwrap();
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::PolicyFirst,
            point: FaultPoint::AfterEffect,
        })
    });
    assert!(host.run_pending().await.is_err());
    assert_eq!(
        lab.world().datasets["labpool/control/erin"]["reservation"],
        (64 * MIB).to_string()
    );
    assert_eq!(
        lab.world().datasets["labpool/control/erin"]["quota"],
        (128 * MIB).to_string()
    );
    drop(host);
    lab.edit(|world| {
        world.new_boot();
        world
            .datasets
            .get_mut("labpool/control/erin/vm-9001-disk-0")
            .unwrap()
            .insert("guid".into(), "9876543210".into());
    });
    let host = lab.host().await;
    lab.finish(&host).await;
    assert_eq!(
        lab.store.read().unwrap().operations[id].public.status,
        OperationStatus::Blocked
    );
    assert_eq!(
        lab.world().datasets["labpool/control/erin"]["quota"],
        (128 * MIB).to_string()
    );
    let current = &lab.store.read().unwrap().users["erin"];
    assert_eq!(current.budget_mib, 128);
    assert!(!current.ready);
}

#[tokio::test]
async fn nonstorage_policy_update_rejects_new_stale_jobs_but_keeps_historical_retries() {
    let (lab, host) = Lab::new().await;
    let created = lab.accept(Lab::create());
    lab.finish(&host).await;
    let original_job = lab.store.read().unwrap().operations
        [created["operation_id"].as_str().unwrap()]
    .job
    .clone();
    let old_user = lab.store.read().unwrap().users["erin"].clone();
    let vm = lab.store.read().unwrap().vms[&9001].clone();
    let mut target = old_user.policy();
    target.max_cores = 4;
    lab.accept(lab.policy("cpu-only", target));
    lab.finish(&host).await;
    assert_eq!(
        lab.store.read().unwrap().users["erin"].budget_mib,
        old_user.budget_mib
    );
    assert_eq!(lab.store.read().unwrap().users["erin"].policy_revision, 1);
    assert_eq!(
        host.submit(original_job).unwrap().status,
        OperationStatus::Verified
    );

    let stale = Job {
        operation_id: "late-stale-start".into(),
        kind: "start".into(),
        owner: old_user,
        vm: Some(vm),
        disk_shrink: None,
        extension: None,
        policy_change: None,
    };
    host.submit(stale).unwrap();
    host.run_pending().await.unwrap();
    let journal = host.journal().unwrap();
    assert_eq!(
        journal.jobs["late-stale-start"].status,
        OperationStatus::Blocked
    );
    assert!(
        journal.jobs["late-stale-start"]
            .error
            .as_ref()
            .unwrap()
            .contains("policy")
    );
    assert!(!lab.world().vms[&9001].running);
}

#[tokio::test]
async fn helper_rejects_forged_previous_nonstorage_policy_at_the_same_revision() {
    let (lab, host) = Lab::new().await;
    let before = lab.world();
    let mut forged = lab.store.read().unwrap().users["erin"].clone();
    forged.max_cores = 16; // Actual provisioned previous policy has max_cores=8.
    let mut requested = forged.policy();
    requested.max_cores = 4;
    let target = forged.with_policy(&requested).unwrap();
    let job = Job {
        operation_id: "forged-old-policy".into(),
        disk_shrink: None,
        extension: None,
        kind: "update_user_policy".into(),
        owner: forged.clone(),
        vm: None,
        policy_change: Some(Box::new(PolicyChange {
            previous: forged,
            target,
        })),
    };
    host.submit(job).unwrap();
    host.run_pending().await.unwrap();
    let journal = host.journal().unwrap();
    assert_eq!(
        journal.jobs["forged-old-policy"].status,
        OperationStatus::Blocked
    );
    assert_eq!(journal.owners["erin"].policy_revision, 0);
    assert_eq!(lab.world().datasets, before.datasets);
}
