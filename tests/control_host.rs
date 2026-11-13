use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use pve_dev::control::{
    host::{
        CONTROL_DATASET, Fault, FaultPoint, Host, HostConfig, HostRequest, HostResponse, MIB,
        MachineState, Phase, SimulatedWorld, Step, World, parse_task, rpc, serve, valid_upid,
    },
    protocol::{OperationStatus, Role, User, Vm, VmState},
    store::Job,
};

const ENV: &str = "0123456789abcdef0123456789abcdef";

type Drift = Box<dyn Fn(&mut World)>;

fn bootable() -> Vm {
    let mut vm = machine(9001, "alice", "guest", 32);
    vm.guest = Some(Box::new(pve_dev::control::protocol::Guest {
        image_sha256: "a".repeat(64),
        ssh_public_key:
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"
                .into(),
    }));
    vm
}

#[tokio::test]
async fn guest_image_provenance_and_owner_key_survive_restart_and_lifecycle() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    let vm = bootable();
    verified(
        &host,
        job("guest", "create_guest", &alice, Some(vm.clone())),
    )
    .await;
    let journal = host.journal().unwrap();
    assert!(journal.machines[&9001].identities.contains_key("image"));
    assert_eq!(journal.machines[&9001].profile.guest, vm.guest);
    drop(host);
    let host = lab.open().await;
    for kind in ["start", "stop", "delete"] {
        verified(&host, job(kind, kind, &alice, Some(vm.clone()))).await;
    }
    assert!(lab.snapshot().vms.is_empty());
    assert_eq!(
        host.journal().unwrap().machines[&9001].state,
        MachineState::Deleted
    );
}

#[tokio::test]
async fn image_effect_recovery_requires_a_completion_marker_or_a_new_boot() {
    for point in [FaultPoint::BeforeEffect, FaultPoint::AfterEffect] {
        for new_boot in [false, true] {
            let lab = Lab::new();
            let alice = user("alice", 128);
            let host = lab.open().await;
            verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
            lab.edit(|world| {
                world.faults.push(Fault {
                    step: Step::Image,
                    point,
                })
            });
            submit(
                &host,
                job("guest", "create_guest", &alice, Some(bootable())),
            )
            .await;
            assert!(host.run_pending().await.is_err());
            assert!(!lab.snapshot().vms[&9001].config.contains_key("scsi0"));
            drop(host);
            if new_boot {
                lab.edit(World::new_boot);
            }
            let host = lab.open().await;
            host.run_pending().await.unwrap();
            let result = status(&host, "guest").await;
            let expected = if point == FaultPoint::BeforeEffect && !new_boot {
                OperationStatus::Blocked
            } else {
                OperationStatus::Verified
            };
            assert_eq!(
                result.status, expected,
                "{point:?} reboot={new_boot}: {result:?}"
            );
        }
    }
}

#[tokio::test]
async fn guest_key_image_or_network_drift_never_authorizes_start() {
    let mut mutations: Vec<Drift> = vec![
        Box::new(|world| {
            world
                .datasets
                .get_mut("labpool/control/alice/vm-9001-disk-0")
                .unwrap()
                .insert("org.pve-dev:image".into(), "b".repeat(64));
        }),
        Box::new(|world| {
            world
                .vms
                .get_mut(&9001)
                .unwrap()
                .config
                .insert("sshkeys".into(), "another-key".into());
        }),
        Box::new(|world| {
            world.vms.get_mut(&9001).unwrap().config.insert(
                "net0".into(),
                "virtio=02:50:00:00:00:01,bridge=vmbr0".into(),
            );
        }),
    ];
    for (key, value) in [
        ("ciuser", "dev"),
        ("ciupgrade", "1"),
        ("citype", "configdrive2"),
        ("cipassword", "secret"),
        ("nameserver", "1.1.1.1"),
        ("searchdomain", "example.org"),
        ("ipconfig1", "ip=dhcp"),
    ] {
        mutations.push(Box::new(move |world| {
            world
                .vms
                .get_mut(&9001)
                .unwrap()
                .config
                .insert(key.into(), value.into());
        }));
    }
    for mutation in mutations {
        let lab = Lab::new();
        let alice = user("alice", 128);
        let host = lab.open().await;
        verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
        verified(
            &host,
            job("guest", "create_guest", &alice, Some(bootable())),
        )
        .await;
        lab.edit(mutation);
        blocked(&host, job("start", "start", &alice, Some(bootable()))).await;
        assert!(!lab.snapshot().vms[&9001].running);
    }
}

#[test]
fn guest_plan_cannot_be_downgraded_to_blank_disk_creation() {
    let alice = user("alice", 128);
    assert!(
        pve_dev::control::host::validate(&job("bad", "create", &alice, Some(bootable()))).is_err()
    );
    assert!(
        pve_dev::control::host::validate(&job(
            "bad",
            "create_guest",
            &alice,
            Some(machine(9001, "alice", "blank", 32))
        ))
        .is_err()
    );
    assert!(
        !pve_dev::control::host::plan(pve_dev::control::JobKind::Create)
            .unwrap()
            .contains(&Step::Image)
    );
    assert!(
        pve_dev::control::host::plan(pve_dev::control::JobKind::CreateGuest)
            .unwrap()
            .contains(&Step::Image)
    );
}

#[tokio::test]
async fn orphan_cloud_init_volume_blocks_same_boot_and_reconciles_after_new_boot() {
    for reboot in [false, true] {
        let lab = Lab::new();
        let alice = user("alice", 128);
        let host = lab.open().await;
        verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
        lab.edit(|world| {
            world.faults.push(Fault {
                step: Step::CloudInit,
                point: FaultPoint::BeforeEffect,
            })
        });
        submit(
            &host,
            job(
                "c-9001",
                "create",
                &alice,
                Some(machine(9001, "alice", "one", 32)),
            ),
        )
        .await;
        assert!(host.run_pending().await.is_err());
        drop(host);
        lab.edit(|world| {
            world
                .zfs_create("labpool/control/alice/vm-9001-cloudinit", Some(4), &[])
                .unwrap();
            if reboot {
                world.new_boot();
            }
        });
        let host = lab.open().await;
        host.run_pending().await.unwrap();
        let result = status(&host, "c-9001").await;
        if reboot {
            assert_eq!(result.status, OperationStatus::Verified, "{result:?}");
            assert_eq!(
                lab.snapshot().vms[&9001].config["ide2"],
                "ctl-alice:vm-9001-cloudinit,media=cdrom"
            );
        } else {
            assert_eq!(result.status, OperationStatus::Blocked, "{result:?}");
            assert!(!lab.snapshot().vms[&9001].config.contains_key("ide2"));
        }
    }
}

struct Lab {
    _root: tempfile::TempDir,
    state: PathBuf,
    world: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let world = root.path().join("world.json");
        Self {
            _root: root,
            state,
            world,
        }
    }

    fn config(&self) -> HostConfig {
        HostConfig::simulated(self.state.clone(), ENV, self.world.clone())
    }

    async fn open(&self) -> Host {
        Host::open(self.config()).await.unwrap()
    }

    fn world(&self) -> SimulatedWorld {
        SimulatedWorld::new(self.world.clone())
    }

    fn edit(&self, action: impl FnOnce(&mut World)) {
        self.world()
            .edit(|world| {
                action(world);
                Ok(())
            })
            .unwrap();
    }

    fn snapshot(&self) -> World {
        self.world().load().unwrap()
    }
}

fn user(name: &str, budget_mib: u64) -> User {
    User {
        user_id: name.into(),
        role: Role::Member,
        budget_mib,
        min_memory_budget_mib: 256,
        max_memory_budget_mib: 1024,
        max_cores: 4,
        max_vms: 4,
        ready: false,
        policy_revision: 0,
        state: pve_dev::control::protocol::AccountState::Active,
    }
}

fn machine(vm_id: u16, owner: &str, name: &str, disk_mib: u64) -> Vm {
    Vm {
        vm_id,
        owner: owner.into(),
        name: name.into(),
        disk_mib,
        memory_mib: 256,
        min_memory_mib: 128,
        cores: 1,
        state: VmState::Creating,
        guest: None,
    }
}

fn job(operation_id: &str, kind: &str, owner: &User, vm: Option<Vm>) -> Job {
    Job {
        operation_id: operation_id.into(),
        kind: kind.parse().unwrap(),
        owner: owner.clone(),
        vm,
        disk_shrink: None,
        extension: None,
        policy_change: None,
    }
}

async fn submit(host: &Host, job: Job) -> HostResponse {
    host.handle(HostRequest::Submit {
        environment: ENV.into(),
        job,
    })
    .await
    .unwrap()
}

async fn status(host: &Host, operation_id: &str) -> HostResponse {
    host.handle(HostRequest::Status {
        environment: ENV.into(),
        operation_id: operation_id.into(),
    })
    .await
    .unwrap()
}

async fn complete(host: &Host, job: Job) -> HostResponse {
    let id = job.operation_id.clone();
    submit(host, job).await;
    host.run_pending().await.unwrap();
    status(host, &id).await
}

async fn verified(host: &Host, job: Job) {
    let id = job.operation_id.clone();
    let result = complete(host, job).await;
    assert_eq!(result.status, OperationStatus::Verified, "{id}: {result:?}");
}

async fn blocked(host: &Host, job: Job) -> String {
    let id = job.operation_id.clone();
    let result = complete(host, job).await;
    assert_eq!(result.status, OperationStatus::Blocked, "{id}: {result:?}");
    result.error.unwrap()
}

async fn provisioned(lab: &Lab, alice: &User) -> Host {
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", alice, None)).await;
    verified(
        &host,
        job(
            "c-9001",
            "create",
            alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    host
}

#[tokio::test]
async fn submissions_are_durable_idempotent_and_conflict_checked() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    let first = submit(&host, job("u-alice", "ensure_user", &alice, None)).await;
    assert_eq!(first.status, OperationStatus::Queued);
    assert_eq!(first.environment, ENV);
    assert_eq!(
        submit(&host, job("u-alice", "ensure_user", &alice, None)).await,
        first
    );
    let conflict = host
        .handle(HostRequest::Submit {
            environment: ENV.into(),
            job: job("u-alice", "ensure_user", &user("alice", 256), None),
        })
        .await;
    assert!(
        conflict
            .unwrap_err()
            .to_string()
            .contains("different payload")
    );
    assert!(
        host.handle(HostRequest::Status {
            environment: ENV.into(),
            operation_id: "missing".into()
        })
        .await
        .is_err()
    );
    let foreign = host
        .handle(HostRequest::Status {
            environment: "f".repeat(32),
            operation_id: "u-alice".into(),
        })
        .await;
    assert!(foreign.is_err());
    drop(host);

    let host = lab.open().await;
    assert_eq!(
        status(&host, "u-alice").await.status,
        OperationStatus::Queued
    );
    host.run_pending().await.unwrap();
    let done = status(&host, "u-alice").await;
    assert_eq!(done.status, OperationStatus::Verified);
    assert_eq!(done.step, "complete");
    assert_eq!(
        submit(&host, job("u-alice", "ensure_user", &alice, None)).await,
        done
    );
    let health = host.handle(HostRequest::Health).await.unwrap();
    assert_eq!(
        (health.environment.as_str(), health.status),
        (ENV, OperationStatus::Verified)
    );
}

#[tokio::test]
async fn invalid_jobs_are_durably_rejected_without_effects() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    let mut big = machine(9002, "alice", "big", 32);
    big.memory_mib = 1024;
    let cases = vec![
        job(
            "bad-range",
            "create",
            &alice,
            Some(machine(9100, "alice", "range", 32)),
        ),
        job(
            "bad-name",
            "create",
            &alice,
            Some(machine(9002, "alice", "Bad_Name", 32)),
        ),
        job("bad-memory", "create", &alice, Some(big)),
        job(
            "bad-owner",
            "create",
            &alice,
            Some(machine(9002, "bob", "owner", 32)),
        ),
        job("bad-user", "ensure_user", &user("Root", 128), None),
        job("bad-budget", "ensure_user", &user("carol", 8193), None),
    ];
    for case in cases {
        let id = case.operation_id.clone();
        let response = submit(&host, case.clone()).await;
        assert_eq!(response.status, OperationStatus::Blocked, "{id}");
        assert_eq!(response.step, "rejected");
        assert_eq!(submit(&host, case).await, response);
    }
    // An unknown kind cannot be decoded into a request, so it is refused at
    // the socket before it reaches the journal.
    let mut wire = serde_json::to_value(HostRequest::Submit {
        environment: ENV.into(),
        job: job(
            "bad-kind",
            "create",
            &alice,
            Some(machine(9002, "alice", "kind", 32)),
        ),
    })
    .unwrap();
    wire["job"]["kind"] = "resize".into();
    assert!(serde_json::from_value::<HostRequest>(wire).is_err());
    host.run_pending().await.unwrap();
    let world = lab.snapshot();
    assert!(world.vms.is_empty());
    assert!(!world.datasets.contains_key("labpool/control/carol"));
}

#[tokio::test]
async fn owner_storage_and_vms_share_one_reserved_dataset() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = provisioned(&lab, &alice).await;
    verified(
        &host,
        job(
            "c-9002",
            "create",
            &alice,
            Some(machine(9002, "alice", "two", 16)),
        ),
    )
    .await;
    let world = lab.snapshot();
    let dataset = &world.datasets["labpool/control/alice"];
    assert_eq!(dataset["quota"], (128 * MIB).to_string());
    assert_eq!(dataset["reservation"], (128 * MIB).to_string());
    assert_eq!(
        dataset["org.pve-dev:owner"],
        format!("pve-dev-control:{ENV}:alice")
    );
    assert_eq!(
        world.datasets[CONTROL_DATASET]["org.pve-dev:control"],
        format!("pve-dev-control:{ENV}")
    );
    let storage = &world.storages["ctl-alice"];
    assert_eq!(
        (
            storage["type"].as_str(),
            storage["pool"].as_str(),
            storage["sparse"].as_str()
        ),
        ("zfspool", "labpool/control/alice", "0")
    );
    for (vm_id, disk) in [(9001u16, 32u64), (9002, 16)] {
        let root = format!("labpool/control/alice/vm-{vm_id}-disk-0");
        assert_eq!(world.datasets[&root]["volsize"], (disk * MIB).to_string());
        assert_eq!(
            world.datasets[&root]["org.pve-dev:owner"],
            format!("pve-dev-control:{ENV}:alice:{vm_id}")
        );
        assert!(
            world
                .datasets
                .contains_key(&format!("labpool/control/alice/vm-{vm_id}-cloudinit"))
        );
        let vm = &world.vms[&vm_id];
        assert!(!vm.running, "VMs are created stopped");
        assert_eq!(
            vm.config["description"],
            format!("pve-dev-control:{ENV}:alice:{vm_id}")
        );
        assert_eq!(
            vm.config["scsi0"],
            format!("ctl-alice:vm-{vm_id}-disk-0,size={disk}M")
        );
        assert_eq!(
            vm.config["ide2"],
            format!("ctl-alice:vm-{vm_id}-cloudinit,media=cdrom")
        );
        assert_eq!(vm.config["boot"], "order=scsi0");
        assert_eq!(
            (
                vm.config["protection"].as_str(),
                vm.config["kvm"].as_str(),
                vm.config["cpu"].as_str()
            ),
            ("1", "0", "qemu64")
        );
    }
    let journal = host.journal().unwrap();
    assert!(journal.owners["alice"].ready);
    assert_eq!(journal.machines[&9001].state, MachineState::Present);
    assert!(journal.machines[&9001].identities.contains_key("volume"));
    assert!(
        journal.jobs["c-9001"]
            .steps
            .iter()
            .all(|step| step.phase == Phase::Verified && step.boot_id.is_some())
    );
    let upid = journal.jobs["c-9001"]
        .steps
        .iter()
        .find(|step| step.step == Step::Vm)
        .unwrap()
        .upid
        .clone()
        .unwrap();
    assert!(valid_upid(&upid));

    drop(host);
    let host = lab.open().await;
    verified(&host, job("u-alice-again", "ensure_user", &alice, None)).await;
    assert_eq!(
        lab.snapshot().datasets,
        world.datasets,
        "reruns do not touch verified resources"
    );
}

#[tokio::test]
async fn shared_owner_quota_refuses_overcommit() {
    let lab = Lab::new();
    let alice = user("alice", 64);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    verified(
        &host,
        job(
            "c-9001",
            "create",
            &alice,
            Some(machine(9001, "alice", "one", 48)),
        ),
    )
    .await;
    let error = blocked(
        &host,
        job(
            "c-9002",
            "create",
            &alice,
            Some(machine(9002, "alice", "two", 16)),
        ),
    )
    .await;
    assert!(error.contains("quota"), "{error}");
    assert!(
        !lab.snapshot()
            .datasets
            .contains_key("labpool/control/alice/vm-9002-disk-0")
    );
    let reuse = blocked(
        &host,
        job(
            "c-9002b",
            "create",
            &alice,
            Some(machine(9002, "alice", "two", 16)),
        ),
    )
    .await;
    assert!(reuse.contains("never reused"), "{reuse}");
}

#[tokio::test]
async fn pool_safety_floor_is_enforced_before_owner_reservation() {
    let lab = Lab::new();
    SimulatedWorld::create(&lab.world, ENV).unwrap();
    lab.edit(|world| world.pool_size_mib = 700);
    let host = lab.open().await;
    let error = blocked(
        &host,
        job("u-alice", "ensure_user", &user("alice", 256), None),
    )
    .await;
    assert!(error.contains("safety floor"), "{error}");
    assert!(
        !lab.snapshot()
            .datasets
            .contains_key("labpool/control/alice")
    );
}

#[tokio::test]
async fn preflight_refuses_unknown_vms_and_volume_collisions() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    lab.edit(|world| {
        let mut config = std::collections::BTreeMap::new();
        config.insert(
            "description".to_string(),
            format!("pve-dev-control:{ENV}:alice:9001"),
        );
        world.vms.insert(
            9001,
            pve_dev::control::host::SimulatedVm {
                config,
                pending_config: Default::default(),
                running: false,
                pid: None,
            },
        );
        world.zfs_create("labpool/stray", None, &[]).unwrap();
        world
            .zfs_create("labpool/stray/vm-9002-disk-0", Some(16), &[])
            .unwrap();
    });
    let error = blocked(
        &host,
        job(
            "c-9001",
            "create",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(error.contains("without helper provenance"), "{error}");
    let error = blocked(
        &host,
        job(
            "c-9002",
            "create",
            &alice,
            Some(machine(9002, "alice", "two", 32)),
        ),
    )
    .await;
    assert!(error.contains("already exist"), "{error}");
    let world = lab.snapshot();
    assert!(!world.vms.contains_key(&9002));
    assert_eq!(world.vms[&9001].config.len(), 1, "foreign VM untouched");
}

#[tokio::test]
async fn ownership_and_device_drift_block_actions() {
    let drifts: Vec<(&str, Drift)> = vec![
        (
            "quota",
            Box::new(|world: &mut World| {
                world
                    .datasets
                    .get_mut("labpool/control/alice")
                    .unwrap()
                    .insert("quota".into(), (144 * MIB).to_string());
            }),
        ),
        (
            "description",
            Box::new(|world: &mut World| {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("description".into(), "external-owner".into());
            }),
        ),
        (
            "device",
            Box::new(|world: &mut World| {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("net0".into(), "virtio,bridge=vmbr0".into());
            }),
        ),
        (
            "unused disk",
            Box::new(|world: &mut World| {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("unused0".into(), "ctl-alice:vm-9001-disk-1".into());
            }),
        ),
        (
            "lock",
            Box::new(|world: &mut World| {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("lock".into(), "backup".into());
            }),
        ),
        (
            "volume",
            Box::new(|world: &mut World| {
                world
                    .datasets
                    .remove("labpool/control/alice/vm-9001-disk-0");
            }),
        ),
        (
            "recreated volume",
            Box::new(|world: &mut World| {
                let name = "labpool/control/alice/vm-9001-disk-0";
                let properties = world.datasets.remove(name).unwrap();
                world
                    .zfs_create(
                        name,
                        Some(32),
                        &[("org.pve-dev:owner", properties["org.pve-dev:owner"].clone())],
                    )
                    .unwrap();
            }),
        ),
        (
            "storage",
            Box::new(|world: &mut World| {
                world
                    .storages
                    .get_mut("ctl-alice")
                    .unwrap()
                    .insert("pool".into(), "labpool".into());
            }),
        ),
        (
            "vanished",
            Box::new(|world: &mut World| {
                world.vms.remove(&9001);
            }),
        ),
    ];
    let alice = user("alice", 128);
    for (label, drift) in drifts {
        let lab = Lab::new();
        let host = provisioned(&lab, &alice).await;
        lab.edit(|world| drift(world));
        let before = lab.snapshot();
        let error = blocked(
            &host,
            job(
                "s-9001",
                "start",
                &alice,
                Some(machine(9001, "alice", "one", 32)),
            ),
        )
        .await;
        assert!(
            error.contains("refus")
                || error.contains("investigate")
                || error.contains("disappeared"),
            "{label}: {error}"
        );
        let error = blocked(
            &host,
            job(
                "d-9001",
                "delete",
                &alice,
                Some(machine(9001, "alice", "one", 32)),
            ),
        )
        .await;
        assert!(!error.is_empty(), "{label}");
        let after = lab.snapshot();
        assert_eq!(
            after.vms.keys().collect::<Vec<_>>(),
            before.vms.keys().collect::<Vec<_>>(),
            "{label}: drift is never overwritten"
        );
        assert_eq!(after.datasets, before.datasets, "{label}");
        if let Some(vm) = after.vms.get(&9001) {
            assert!(!vm.running, "{label}");
        }
    }
}

#[tokio::test]
async fn same_boot_ambiguous_intent_blocks_pending_investigation() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Vm,
            point: FaultPoint::BeforeEffect,
        })
    });
    submit(
        &host,
        job(
            "c-9001",
            "create",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(host.run_pending().await.is_err(), "simulated crash");
    drop(host);

    let host = lab.open().await;
    assert_eq!(
        status(&host, "c-9001").await.status,
        OperationStatus::Running
    );
    let intent = host.journal().unwrap().jobs["c-9001"]
        .steps
        .iter()
        .find(|step| step.step == Step::Vm)
        .unwrap()
        .clone();
    assert_eq!(intent.phase, Phase::Intent);
    assert!(intent.upid.is_none());
    host.run_pending().await.unwrap();
    let result = status(&host, "c-9001").await;
    assert_eq!(result.status, OperationStatus::Blocked);
    assert!(result.error.unwrap().contains("pending investigation"));
    assert!(lab.snapshot().vms.is_empty(), "no blind retry");
}

#[tokio::test]
async fn confirmed_new_boot_permits_deterministic_reconciliation() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Vm,
            point: FaultPoint::BeforeEffect,
        });
        world.faults.push(Fault {
            step: Step::CloudInit,
            point: FaultPoint::AfterEffect,
        });
    });
    submit(
        &host,
        job(
            "c-9001",
            "create",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(host.run_pending().await.is_err());
    drop(host);
    lab.edit(World::new_boot);

    let host = lab.open().await;
    assert!(
        host.run_pending().await.is_err(),
        "second crash after cloud-init attachment"
    );
    drop(host);
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    assert_eq!(
        status(&host, "c-9001").await.status,
        OperationStatus::Verified
    );
    let world = lab.snapshot();
    assert_eq!(world.vms.keys().copied().collect::<Vec<_>>(), vec![9001]);
    assert_eq!(
        world
            .datasets
            .keys()
            .filter(|name| name.contains("vm-9001-"))
            .count(),
        2
    );
    let steps = &host.journal().unwrap().jobs["c-9001"].steps;
    let vm = steps.iter().find(|step| step.step == Step::Vm).unwrap();
    assert_eq!(vm.boot_id.as_deref(), Some("simulated-boot-2"));
}

#[tokio::test]
async fn lost_receipt_with_exact_present_resource_converges_and_receipts_resume() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    verified(&host, job("u-alice", "ensure_user", &alice, None)).await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Vm,
            point: FaultPoint::AfterEffect,
        });
        world.faults.push(Fault {
            step: Step::Volume,
            point: FaultPoint::AfterEffect,
        });
    });
    submit(
        &host,
        job(
            "c-9001",
            "create",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    for _ in 0..2 {
        assert!(host.run_pending().await.is_err());
    }
    host.run_pending().await.unwrap();
    assert_eq!(
        status(&host, "c-9001").await.status,
        OperationStatus::Verified
    );

    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Start,
            point: FaultPoint::AfterReceipt,
        })
    });
    submit(
        &host,
        job(
            "s-9001",
            "start",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(host.run_pending().await.is_err());
    drop(host);
    let host = lab.open().await;
    let pending = host.journal().unwrap().jobs["s-9001"]
        .steps
        .iter()
        .find(|step| step.step == Step::Start)
        .unwrap()
        .clone();
    assert!(pending.upid.is_some() && pending.phase == Phase::Intent);
    host.run_pending().await.unwrap();
    assert_eq!(
        status(&host, "s-9001").await.status,
        OperationStatus::Verified
    );
    let world = lab.snapshot();
    assert!(world.vms[&9001].running);
    assert_eq!(
        world
            .tasks
            .keys()
            .filter(|upid| upid.contains(":qmstart:"))
            .count(),
        1,
        "exact receipt resumed, no resubmission"
    );
}

#[tokio::test]
async fn uncertain_task_outcome_blocks() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = provisioned(&lab, &alice).await;
    lab.edit(|world| world.stall_tasks = true);
    let error = blocked(
        &host,
        job(
            "s-9001",
            "start",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(error.contains("uncertain"), "{error}");
    assert_eq!(
        lab.snapshot()
            .tasks
            .keys()
            .filter(|upid| upid.contains(":qmstart:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn lifecycle_delete_releases_only_the_vm_and_keeps_a_tombstone() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = provisioned(&lab, &alice).await;
    verified(
        &host,
        job(
            "c-9002",
            "create",
            &alice,
            Some(machine(9002, "alice", "two", 16)),
        ),
    )
    .await;
    let one = || Some(machine(9001, "alice", "one", 32));
    verified(&host, job("s-9001", "start", &alice, one())).await;
    assert!(lab.snapshot().vms[&9001].running);
    verified(&host, job("s-9001b", "start", &alice, one())).await;
    verified(&host, job("p-9001", "stop", &alice, one())).await;
    verified(&host, job("s-9001c", "start", &alice, one())).await;
    verified(&host, job("d-9001", "delete", &alice, one())).await;
    let world = lab.snapshot();
    assert!(!world.vms.contains_key(&9001));
    assert!(world.vms.contains_key(&9002));
    assert!(!world.datasets.keys().any(|name| name.contains("vm-9001-")));
    assert!(
        world
            .datasets
            .contains_key("labpool/control/alice/vm-9002-disk-0")
    );
    assert!(world.datasets.contains_key("labpool/control/alice"));
    assert!(world.storages.contains_key("ctl-alice"));
    let journal = host.journal().unwrap();
    assert_eq!(journal.machines[&9001].state, MachineState::Deleted);
    assert_eq!(
        journal.machines[&9001].delete_operation.as_deref(),
        Some("d-9001")
    );
    let error = blocked(&host, job("c-9001b", "create", &alice, one())).await;
    assert!(error.contains("never reused"), "{error}");
    let error = blocked(&host, job("s-9001d", "start", &alice, one())).await;
    assert!(error.contains("Deleted"), "{error}");
}

#[tokio::test]
async fn delete_converges_only_for_its_own_proven_pending_destroy() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = provisioned(&lab, &alice).await;
    lab.edit(|world| {
        world.faults.push(Fault {
            step: Step::Destroy,
            point: FaultPoint::AfterEffect,
        })
    });
    submit(
        &host,
        job(
            "d-9001",
            "delete",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(host.run_pending().await.is_err());
    drop(host);
    assert!(lab.snapshot().vms.is_empty());
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    assert_eq!(
        status(&host, "d-9001").await.status,
        OperationStatus::Verified
    );
    assert_eq!(
        host.journal().unwrap().machines[&9001].state,
        MachineState::Deleted
    );

    let lab = Lab::new();
    let host = provisioned(&lab, &alice).await;
    lab.edit(|world| {
        world.vms.remove(&9001);
    });
    let error = blocked(
        &host,
        job(
            "d-9001",
            "delete",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    assert!(error.contains("disappeared"), "{error}");
    assert_eq!(
        host.journal().unwrap().machines[&9001].state,
        MachineState::Present
    );
    assert!(
        lab.snapshot()
            .datasets
            .contains_key("labpool/control/alice/vm-9001-disk-0"),
        "no orphan cleanup"
    );
}

#[tokio::test]
async fn new_boot_does_not_restart_completed_start_operations() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = provisioned(&lab, &alice).await;
    verified(
        &host,
        job(
            "s-9001",
            "start",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
    drop(host);
    lab.edit(World::new_boot);
    let host = lab.open().await;
    host.run_pending().await.unwrap();
    assert!(!lab.snapshot().vms[&9001].running);
    assert_eq!(
        status(&host, "s-9001").await.status,
        OperationStatus::Verified
    );
    verified(
        &host,
        job(
            "p-9001",
            "stop",
            &alice,
            Some(machine(9001, "alice", "one", 32)),
        ),
    )
    .await;
}

#[tokio::test]
async fn non_lab_hosts_and_lost_journals_fail_closed() {
    let lab = Lab::new();
    let alice = user("alice", 128);
    let host = lab.open().await;
    submit(&host, job("u-alice", "ensure_user", &alice, None)).await;
    lab.edit(|world| world.hostname = "production".into());
    assert!(host.handle(HostRequest::Health).await.is_err());
    assert!(host.run_pending().await.is_err());
    assert_eq!(
        status(&host, "u-alice").await.status,
        OperationStatus::Queued,
        "identity failure is not recorded as a job outcome"
    );
    lab.edit(|world| world.hostname = "pve-lab".into());
    host.run_pending().await.unwrap();
    assert_eq!(
        status(&host, "u-alice").await.status,
        OperationStatus::Verified
    );
    drop(host);

    lab.edit(|world| world.lab_id = "f".repeat(32));
    assert!(Host::open(lab.config()).await.is_err());
    lab.edit(|world| world.lab_id = ENV.into());
    std::fs::remove_file(lab.state.join("host-journal.redb")).unwrap();
    let error = Host::open(lab.config()).await.err().unwrap();
    assert!(
        error.to_string().contains("refusing to reinitialize"),
        "{error}"
    );

    let fresh = lab.state.with_file_name("fresh-state");
    let error = Host::open(HostConfig::simulated(fresh, ENV, lab.world.clone()))
        .await
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("prior initialization"),
        "{error}"
    );

    let other = Lab::new();
    other.open().await;
    assert!(
        Host::open(HostConfig::simulated(
            other.state.clone(),
            &"e".repeat(32),
            other.world.clone()
        ))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn second_helper_cannot_open_the_same_journal() {
    let lab = Lab::new();
    let _host = lab.open().await;
    assert!(Host::open(lab.config()).await.is_err());
}

#[test]
fn task_receipts_are_parsed_from_the_final_json_line() {
    let upid = "UPID:pve-lab:00000642:00002741:6AF1F85D:qmstart:9001:root@pam:";
    assert_eq!(
        parse_task(&format!("generating cloud-init ISO\n\"{upid}\"\n")).unwrap(),
        upid
    );
    assert!(parse_task("\"not-a-task\"").is_err());
    assert!(parse_task("update VM 9001: -scsi0 lab").is_err());
    assert!(parse_task("\"UPID:other-node:1:2:3:qmstart:9001:root@pam:\"").is_err());
    assert!(!valid_upid("UPID:pve-lab:1/../../x"));
}

#[test]
fn lab_config_must_name_a_nonce_and_pool_guid() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("lab.json");
    std::fs::write(
        &file,
        format!(r#"{{"environment":"{ENV}","pool_guid":"1234567890"}}"#),
    )
    .unwrap();
    assert!(HostConfig::lab(root.path().join("state"), &file).is_ok());
    std::fs::write(&file, r#"{"environment":"short","pool_guid":"1"}"#).unwrap();
    assert!(HostConfig::lab(root.path().join("state"), &file).is_err());
    std::fs::write(
        &file,
        format!(r#"{{"environment":"{ENV}","pool_guid":"abc"}}"#),
    )
    .unwrap();
    assert!(HostConfig::lab(root.path().join("state"), &file).is_err());
    std::fs::write(
        &file,
        format!(r#"{{"environment":"{ENV}","pool_guid":"1","extra":1}}"#),
    )
    .unwrap();
    assert!(HostConfig::lab(root.path().join("state"), &file).is_err());
}

async fn wait_for(socket: &Path, operation_id: &str) -> HostResponse {
    for _ in 0..400 {
        let response = rpc(
            socket,
            &HostRequest::Status {
                environment: ENV.into(),
                operation_id: operation_id.into(),
            },
        )
        .await
        .unwrap();
        if matches!(
            response.status,
            OperationStatus::Verified | OperationStatus::Blocked
        ) {
            return response;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("operation {operation_id} did not finish");
}

#[tokio::test]
async fn unix_socket_rpc_serves_bounded_single_responses() {
    let lab = Lab::new();
    let runtime = lab.state.with_file_name("run");
    let socket = runtime.join("host.sock");
    let host = Arc::new(lab.open().await);
    let server = tokio::spawn({
        let socket = socket.clone();
        async move { serve(host, &socket).await }
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660);
    let runtime_meta = std::fs::metadata(&runtime).unwrap();
    assert_eq!(runtime_meta.permissions().mode() & 0o777, 0o750);
    assert_eq!(
        runtime_meta.uid(),
        std::fs::symlink_metadata(&socket).unwrap().uid()
    );

    let health = rpc(&socket, &HostRequest::Health).await.unwrap();
    assert_eq!(health.environment, ENV);
    let alice = user("alice", 128);
    let submitted = rpc(
        &socket,
        &HostRequest::Submit {
            environment: ENV.into(),
            job: job("u-alice", "ensure_user", &alice, None),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        submitted.status,
        OperationStatus::Queued | OperationStatus::Running | OperationStatus::Verified
    ));
    assert_eq!(
        wait_for(&socket, "u-alice").await.status,
        OperationStatus::Verified
    );
    rpc(
        &socket,
        &HostRequest::Submit {
            environment: ENV.into(),
            job: job(
                "c-9001",
                "create",
                &alice,
                Some(machine(9001, "alice", "one", 32)),
            ),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        wait_for(&socket, "c-9001").await.status,
        OperationStatus::Verified
    );
    let error = rpc(
        &socket,
        &HostRequest::Status {
            environment: ENV.into(),
            operation_id: "missing".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("unknown helper operation"),
        "{error}"
    );
    let conflict = rpc(
        &socket,
        &HostRequest::Submit {
            environment: ENV.into(),
            job: job("u-alice", "ensure_user", &user("alice", 64), None),
        },
    )
    .await;
    assert!(conflict.is_err());

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream
        .write_all(b"{\"action\":\"shell\",\"command\":\"id\"}")
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.contains("\"ok\":false"), "{reply}");

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let oversized = vec![b' '; 300 * 1024];
    let _ = stream.write_all(&oversized).await;
    let _ = stream.shutdown().await;
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply).await;
    assert!(
        reply.is_empty() || reply.contains("\"ok\":false"),
        "{reply}"
    );

    server.abort();
    let _ = server.await;
}

#[test]
fn job_fingerprint_is_canonical_and_versioned() {
    // Canonical JSON relies on serde_json's sorted map; enabling the
    // `preserve_order` feature anywhere in the build would silently break it.
    assert_eq!(
        serde_json::to_string(&serde_json::json!({"b": 1, "a": 2})).unwrap(),
        r#"{"a":2,"b":1}"#
    );
    let alice = user("alice", 128);
    let job = job(
        "fp",
        "create",
        &alice,
        Some(machine(9002, "alice", "fp", 32)),
    );
    let canonical = serde_json::to_vec(&serde_json::to_value(&job).unwrap()).unwrap();
    let mut expected = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut expected, b"hearth-job-v1\n");
    sha2::Digest::update(&mut expected, &canonical);
    let fingerprint = pve_dev::control::host::fingerprint(&job).unwrap();
    assert_eq!(
        fingerprint,
        format!("{:x}", sha2::Digest::finalize(expected))
    );
    // The domain tag keeps it distinct from the unversioned hash of the same bytes.
    assert_ne!(
        fingerprint,
        format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&canonical))
    );
}
