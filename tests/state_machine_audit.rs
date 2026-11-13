//! Counterexamples and invariants from the independent server state-machine audit.
//! These use only a disposable simulated Proxmox/ZFS world.

use std::path::PathBuf;

use pve_dev::control::{
    host::{Fault, FaultPoint, Host, HostConfig, SimulatedWorld, Step, World},
    protocol::{OperationStatus, Role, User, Vm, VmState},
    store::Job,
};

const ENV: &str = "0123456789abcdef0123456789abcdef";

struct Lab {
    _root: tempfile::TempDir,
    state: PathBuf,
    world: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        Self {
            state: root.path().join("state"),
            world: root.path().join("world.json"),
            _root: root,
        }
    }

    async fn open(&self) -> Host {
        Host::open(HostConfig::simulated(
            self.state.clone(),
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

    async fn provisioned(&self) -> Host {
        let host = self.open().await;
        for (id, kind, vm) in [
            ("owner", "ensure_user", None),
            ("create", "create", Some(machine())),
        ] {
            host.submit(job(id, kind, vm)).unwrap();
            host.run_pending().await.unwrap();
            assert_eq!(
                host.journal().unwrap().jobs[id].status,
                OperationStatus::Verified
            );
        }
        host
    }
}

fn owner() -> User {
    User {
        user_id: "alice".into(),
        role: Role::Member,
        budget_mib: 128,
        min_memory_budget_mib: 256,
        max_memory_budget_mib: 1024,
        max_cores: 4,
        max_vms: 4,
        ready: false,
        policy_revision: 0,
        state: pve_dev::control::protocol::AccountState::Active,
    }
}

fn machine() -> Vm {
    Vm {
        vm_id: 9001,
        owner: "alice".into(),
        name: "one".into(),
        disk_mib: 32,
        memory_mib: 256,
        min_memory_mib: 128,
        cores: 1,
        state: VmState::Creating,
        guest: None,
    }
}

fn job(id: &str, kind: &str, vm: Option<Vm>) -> Job {
    Job {
        operation_id: id.into(),
        kind: kind.into(),
        owner: owner(),
        vm,
        disk_shrink: None,
        extension: None,
        policy_change: None,
    }
}

#[tokio::test]
async fn recovered_start_revalidates_vm_identity_before_effect() {
    for (point, reboot) in [
        (FaultPoint::BeforeEffect, true),
        (FaultPoint::AfterEffect, false),
        (FaultPoint::AfterReceipt, false),
    ] {
        let lab = Lab::new();
        let host = lab.provisioned().await;
        lab.edit(|world| {
            world.faults.push(Fault {
                step: Step::Start,
                point,
            })
        });
        host.submit(job("start", "start", Some(machine()))).unwrap();
        assert!(host.run_pending().await.is_err());
        drop(host);
        lab.edit(|world| {
            if reboot {
                world.new_boot();
            }
            world
                .vms
                .get_mut(&9001)
                .unwrap()
                .config
                .insert("smbios1".into(), "uuid=foreign-replacement".into());
        });
        let host = lab.open().await;
        host.run_pending().await.unwrap();
        let journal = host.journal().unwrap();
        assert_eq!(
            journal.jobs["start"].status,
            OperationStatus::Blocked,
            "a durable old Verify does not authorize a replacement VM after recovery: {point:?} reboot={reboot}"
        );
        if reboot {
            assert!(
                !lab.snapshot().vms[&9001].running,
                "foreign VM must remain untouched"
            );
        }
    }
}

#[tokio::test]
async fn recovered_delete_revalidates_disk_attachment_before_destroy() {
    for (point, reboot) in [
        (FaultPoint::BeforeEffect, true),
        (FaultPoint::AfterEffect, false),
    ] {
        let lab = Lab::new();
        let host = lab.provisioned().await;
        lab.edit(|world| {
            world.faults.push(Fault {
                step: Step::Unprotect,
                point,
            })
        });
        host.submit(job("delete", "delete", Some(machine())))
            .unwrap();
        assert!(host.run_pending().await.is_err());
        drop(host);
        lab.edit(|world| {
            if reboot {
                world.new_boot();
            }
            // Same VM UUID/profile, but scsi0 now points at unrelated data.
            let mut foreign = world.datasets["labpool/control/alice/vm-9001-disk-0"].clone();
            foreign.insert("guid".into(), "9876543210".into());
            world
                .datasets
                .insert("labpool/control/alice/vm-9999-disk-0".into(), foreign);
            world
                .vms
                .get_mut(&9001)
                .unwrap()
                .config
                .insert("scsi0".into(), "ctl-alice:vm-9999-disk-0".into());
        });
        let host = lab.open().await;
        host.run_pending().await.unwrap();
        let world = lab.snapshot();
        assert!(
            world
                .datasets
                .contains_key("labpool/control/alice/vm-9999-disk-0"),
            "recovery must not destroy a newly attached foreign volume: {point:?} reboot={reboot}"
        );
        assert!(
            world.vms.contains_key(&9001),
            "drifted VM must remain for investigation"
        );
        assert_eq!(
            host.journal().unwrap().jobs["delete"].status,
            OperationStatus::Blocked
        );
    }
}

#[tokio::test]
async fn recovered_stop_revalidates_replacement_even_when_postcondition_is_satisfied() {
    for (point, reboot) in [
        (FaultPoint::BeforeEffect, true),
        (FaultPoint::AfterEffect, false),
        (FaultPoint::AfterReceipt, false),
    ] {
        let lab = Lab::new();
        let host = lab.provisioned().await;
        host.submit(job("start", "start", Some(machine()))).unwrap();
        host.run_pending().await.unwrap();
        lab.edit(|world| {
            world.faults.push(Fault {
                step: Step::Stop,
                point,
            })
        });
        host.submit(job("stop", "stop", Some(machine()))).unwrap();
        assert!(host.run_pending().await.is_err());
        drop(host);
        lab.edit(|world| {
            if reboot {
                world.new_boot();
            }
            let vm = world.vms.get_mut(&9001).unwrap();
            vm.config
                .insert("smbios1".into(), "uuid=foreign-replacement".into());
            // Exercise both a pending stop and an already-satisfied stop.
            if reboot {
                vm.running = true;
            }
        });
        let host = lab.open().await;
        host.run_pending().await.unwrap();
        assert_eq!(
            host.journal().unwrap().jobs["stop"].status,
            OperationStatus::Blocked,
            "{point:?} reboot={reboot}"
        );
        assert_eq!(
            lab.snapshot().vms[&9001].running,
            reboot,
            "reconciliation must leave the replacement's runtime state untouched"
        );
    }
}

#[tokio::test]
async fn recovered_start_rejects_replaced_zvol_with_same_name() {
    let lab = Lab::new();
    let host = lab.provisioned().await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Start,
            point: FaultPoint::BeforeEffect,
        })
    });
    host.submit(job("start", "start", Some(machine()))).unwrap();
    assert!(host.run_pending().await.is_err());
    drop(host);
    lab.edit(|world| {
        world.new_boot();
        world
            .datasets
            .get_mut("labpool/control/alice/vm-9001-disk-0")
            .unwrap()
            .insert("guid".into(), "9876543210".into());
    });
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["start"].status,
        OperationStatus::Blocked
    );
    assert!(!lab.snapshot().vms[&9001].running);
}

#[tokio::test]
async fn recovered_start_rejects_a_running_replacement_as_false_success() {
    let lab = Lab::new();
    let host = lab.provisioned().await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Start,
            point: FaultPoint::BeforeEffect,
        })
    });
    host.submit(job("start", "start", Some(machine()))).unwrap();
    assert!(host.run_pending().await.is_err());
    drop(host);
    lab.edit(|world| {
        let vm = world.vms.get_mut(&9001).unwrap();
        vm.config
            .insert("smbios1".into(), "uuid=foreign-replacement".into());
        vm.running = true;
    });
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["start"].status,
        OperationStatus::Blocked
    );
    assert!(
        lab.snapshot().vms[&9001].running,
        "foreign running VM is not stopped or adopted"
    );
}

#[tokio::test]
async fn resumed_destroy_refuses_a_vm_restarted_after_stop_was_verified() {
    let lab = Lab::new();
    let host = lab.provisioned().await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Destroy,
            point: FaultPoint::BeforeEffect,
        })
    });
    host.submit(job("delete", "delete", Some(machine())))
        .unwrap();
    assert!(host.run_pending().await.is_err());
    drop(host);
    lab.edit(|world| {
        world.new_boot();
        world.vms.get_mut(&9001).unwrap().running = true;
    });
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    let journal = host.journal().unwrap();
    assert_eq!(journal.jobs["delete"].status, OperationStatus::Blocked);
    assert!(
        journal.jobs["delete"]
            .error
            .as_deref()
            .unwrap()
            .contains("restarted during deletion")
    );
    let world = lab.snapshot();
    assert!(world.vms[&9001].running);
    assert!(
        world
            .datasets
            .contains_key("labpool/control/alice/vm-9001-disk-0")
    );
}

#[tokio::test]
async fn recorded_running_task_must_finish_before_recovery_authorizes_delete() {
    let lab = Lab::new();
    let host = lab.provisioned().await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Destroy,
            point: FaultPoint::AfterReceipt,
        })
    });
    host.submit(job("delete", "delete", Some(machine())))
        .unwrap();
    assert!(host.run_pending().await.is_err());
    let journal = host.journal().unwrap();
    let upid = journal.jobs["delete"]
        .steps
        .iter()
        .find(|step| step.step == Step::Destroy)
        .unwrap()
        .upid
        .clone()
        .unwrap();
    drop(host);
    lab.edit(|world| {
        world.tasks.insert(
            upid,
            serde_json::json!({"status":"stopped","exitstatus":"ERROR: incomplete destruction"}),
        );
    });
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    // Physical absence alone cannot replace a durable task failure receipt.
    assert_eq!(
        host.journal().unwrap().jobs["delete"].status,
        OperationStatus::Blocked
    );
    assert_eq!(
        host.journal().unwrap().machines[&9001].state,
        pve_dev::control::host::MachineState::Deleting
    );
}

async fn deliver_pending(store: &pve_dev::control::store::Store, host: &Host, acknowledge: bool) {
    for record in store.pending().unwrap() {
        host.submit(record.job).unwrap();
        host.run_pending().await.unwrap();
        let journal = host.journal().unwrap();
        let result = &journal.jobs[&record.public.operation_id];
        assert_eq!(result.status, OperationStatus::Verified);
        if acknowledge {
            store
                .progress(
                    &record.public.operation_id,
                    result.status.clone(),
                    &result.step,
                    result.error.clone(),
                )
                .unwrap();
        }
    }
}

#[tokio::test]
async fn bounded_lifecycle_model_preserves_claims_and_identity_across_lost_acks_and_reopens() {
    use pve_dev::control::{
        host::MachineState,
        protocol::{Request, VmAction},
        store::{Binding, Store, disk_charge},
    };
    for seed in [1u64, 17, 0xdecafbad] {
        let lab = Lab::new();
        let binding = Binding {
            environment: ENV.into(),
            backend: "simulated".into(),
            disk_capacity_mib: 2048,
            min_memory_capacity_mib: 1024,
            guest_images: Vec::new(),
            physical_trial: None,
            managed: None,
        };
        let peer = iroh::SecretKey::generate().public().to_string();
        let state = lab._root.path().join("controller.redb");
        let mut store = Store::create(&state, binding.clone(), &peer).unwrap();
        let mut host = lab.open().await;
        deliver_pending(&store, &host, true).await;
        let mut random = seed;
        let mut accepted = Vec::<Request>::new();
        for index in 0..96u64 {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let catalog = store.read().unwrap();
            let live: Vec<_> = catalog
                .vms
                .values()
                .filter(|vm| vm.state != VmState::Deleted)
                .map(|vm| vm.vm_id)
                .collect();
            let request = if random % 5 == 0 && !accepted.is_empty() {
                accepted[random as usize % accepted.len()].clone()
            } else if random % 3 == 0 && !live.is_empty() {
                Request::VmAction {
                    request_id: format!("a-{index}"),
                    vm_id: live[random as usize % live.len()],
                    kind: match random % 4 {
                        0 => VmAction::Delete,
                        1 => VmAction::Start,
                        _ => VmAction::Stop,
                    },
                }
            } else {
                Request::CreateVm {
                    request_id: format!("c-{index}"),
                    name: format!("vm-{index}"),
                    disk_mib: 32,
                    memory_mib: 128,
                    min_memory_mib: 128,
                    cores: 1,
                    owner: None,
                    guest: None,
                }
            };
            if store.handle(&peer, request.clone()).is_ok() {
                accepted.push(request);
            }
            if random % 4 != 0 {
                // Some helper completions are deliberately not acknowledged;
                // the next delivery repeats the original immutable operation.
                deliver_pending(&store, &host, random % 7 != 0).await;
            }
            if index % 11 == 0 {
                drop(host);
                drop(store);
                host = lab.open().await;
                store = Store::open(&state, &binding).unwrap();
            }
            let catalog = store.read().unwrap();
            let journal = host.journal().unwrap();
            let world = lab.snapshot();
            let charged: Vec<_> = catalog
                .vms
                .values()
                .filter(|vm| vm.state != VmState::Deleted)
                .collect();
            assert!(
                charged.iter().map(|vm| disk_charge(vm)).sum::<u64>() <= 128,
                "seed={seed} step={index}"
            );
            assert!(charged.iter().map(|vm| vm.min_memory_mib).sum::<u64>() <= 256);
            for vmid in world.vms.keys() {
                assert!(catalog.vms.contains_key(vmid));
                assert_ne!(
                    catalog.vms[vmid].state,
                    VmState::Deleted,
                    "claims cannot precede physical release"
                );
            }
            for vm in catalog
                .vms
                .values()
                .filter(|vm| vm.state == VmState::Deleted)
            {
                assert_eq!(journal.machines[&vm.vm_id].state, MachineState::Deleted);
                assert!(!world.vms.contains_key(&vm.vm_id));
            }
            assert_eq!(
                catalog.requests.len(),
                catalog.operations.len(),
                "request identity remains bijective"
            );
        }
        deliver_pending(&store, &host, true).await;
        assert!(store.pending().unwrap().is_empty());
        assert!(
            store
                .read()
                .unwrap()
                .operations
                .values()
                .all(|record| record.public.status == OperationStatus::Verified)
        );
    }
}

#[tokio::test]
async fn overlapping_helper_executors_do_not_execute_the_same_operation_concurrently() {
    use std::{sync::Arc, time::Duration};
    let lab = Lab::new();
    let host = Arc::new(lab.provisioned().await);
    lab.edit(|world| world.stall_tasks = true);
    host.submit(job("start", "start", Some(machine()))).unwrap();
    let first_host = host.clone();
    let first = tokio::spawn(async move { first_host.run_pending().await });
    let upid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let journal = host.journal().unwrap();
            if let Some(upid) = journal.jobs["start"]
                .steps
                .iter()
                .find(|record| record.step == Step::Start)
                .and_then(|record| record.upid.clone())
            {
                break upid;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("first executor reaches a durable task receipt");
    let (entered, ready) = tokio::sync::oneshot::channel();
    let second_host = host.clone();
    let second = tokio::spawn(async move {
        entered.send(()).unwrap();
        second_host.run_pending().await
    });
    ready.await.unwrap();
    // The first executor is blocked on a simulated external task; the second
    // must wait for exclusive execution, rather than reopen its Running job.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let attempts_during_overlap = host.journal().unwrap().jobs["start"].attempts;
    lab.edit(|world| {
        world.stall_tasks = false;
        world.tasks.insert(
            upid,
            serde_json::json!({"status":"stopped","exitstatus":"OK"}),
        );
    });
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(
        attempts_during_overlap, 1,
        "a Running job has exactly one active executor"
    );
    let journal = host.journal().unwrap();
    assert_eq!(journal.jobs["start"].attempts, 1);
    assert_eq!(journal.jobs["start"].status, OperationStatus::Verified);
}
