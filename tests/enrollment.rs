//! Enrollment qualification uses only the persistent simulated helper and catalog.
//! Fixtures are created externally to the controller, never by a managed VM job.
use std::{
    collections::BTreeMap, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration,
};

use iroh::SecretKey;
use pve_dev::control::{
    host::{
        self, Fault, FaultPoint, Host, HostConfig, HostRequest, MIB, SimulatedVm, SimulatedWorld,
        Step, World,
    },
    protocol::{OperationStatus, Request, VmState},
    services::Services,
    store::{Binding, Store, disk_charge},
    vm_change::EnrollmentPlan,
};
use serde_json::{Value, json};

const ENV: &str = "0123456789abcdef0123456789abcded";
const ROOT: &str = "labpool/control/alice/vm-9001-disk-0";
const CI: &str = "labpool/control/alice/vm-9001-cloudinit";

struct Lab {
    directory: tempfile::TempDir,
    store: Store,
    admin: String,
    member: String,
    outsider: String,
    world: PathBuf,
}
impl Lab {
    async fn new() -> (Self, Host) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let admin = SecretKey::generate().public().to_string();
        let store = Store::create(
            &directory.path().join("catalog.redb"),
            Binding {
                environment: ENV.into(),
                backend: "simulated".into(),
                disk_capacity_mib: 8192,
                min_memory_capacity_mib: 2048,
                guest_images: vec![],
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
            member: SecretKey::generate().public().to_string(),
            outsider: SecretKey::generate().public().to_string(),
            world,
        };
        let host = lab.open().await;
        lab.edit(|world| world.pool_size_mib = 16384);
        lab.flush(&host).await;
        for owner in ["alice", "bob"] {
            lab.send(json!({"action":"create_user","request_id":owner,"user_id":owner,"budget_mib":2048,
                "min_memory_budget_mib":512,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":2}));
            lab.flush(&host).await;
        }
        lab.send(json!({"action":"enroll_device","user_id":"alice","endpoint_id":lab.member,"label":"alice laptop"}));
        lab.edit(|world| {
            let inherited = format!("pve-dev-control:{ENV}:alice");
            world
                .zfs_create(
                    ROOT,
                    Some(512),
                    &[
                        ("origin", "-".into()),
                        ("readonly", "off".into()),
                        ("org.pve-dev:owner", inherited.clone()),
                    ],
                )
                .unwrap();
            world
                .zfs_create(
                    CI,
                    Some(4),
                    &[
                        ("origin", "-".into()),
                        ("readonly", "off".into()),
                        ("org.pve-dev:owner", inherited),
                    ],
                )
                .unwrap();
            let config = BTreeMap::from([
                ("name".into(), "dev-unmanaged".into()),
                (
                    "description".into(),
                    "Externally prepared disposable fixture".into(),
                ),
                (
                    "smbios1".into(),
                    "uuid=11111111-2222-4333-8444-555555555555".into(),
                ),
                (
                    "vmgenid".into(),
                    "22222222-3333-4444-8555-666666666666".into(),
                ),
                ("boot".into(), "order=scsi0".into()),
                ("scsi0".into(), "ctl-alice:vm-9001-disk-0,size=512M".into()),
                (
                    "ide2".into(),
                    "ctl-alice:vm-9001-cloudinit,media=cdrom".into(),
                ),
                ("memory".into(), "512".into()),
                ("balloon".into(), "128".into()),
                ("cores".into(), "2".into()),
                ("cpu".into(), "host".into()),
                ("scsihw".into(), "virtio-scsi-pci".into()),
                (
                    "net0".into(),
                    "virtio=02:50:00:00:00:99,bridge=vmbr77".into(),
                ),
                ("serial0".into(), "socket".into()),
                ("onboot".into(), "0".into()),
                ("protection".into(), "0".into()),
            ]);
            world.vms.insert(
                9001,
                SimulatedVm {
                    config,
                    ..Default::default()
                },
            );
        });
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
    fn reopen(mut self) -> Self {
        let binding = self.store.read().unwrap().binding;
        drop(self.store);
        self.store = Store::open(&self.directory.path().join("catalog.redb"), &binding).unwrap();
        self
    }
    fn edit(&self, edit: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|world| {
                edit(world);
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
    async fn plan(&self, host: &Host) -> EnrollmentPlan {
        let response = host
            .handle(HostRequest::PlanEnrollment {
                environment: ENV.into(),
                vm_id: 9001,
                owner: "alice".into(),
            })
            .await
            .unwrap();
        let plan: EnrollmentPlan = serde_json::from_value(response.enrollment.unwrap()).unwrap();
        self.store
            .remember_enrollment_plan(&self.admin, plan.clone())
            .unwrap();
        plan
    }
    fn request(&self, plan: &EnrollmentPlan, id: &str) -> Request {
        Request::EnrollVm {
            request_id: id.into(),
            plan_id: plan.plan_id.clone(),
            vm_id: 9001,
            owner: "alice".into(),
            confirm_enrollment: true,
        }
    }
    fn accept(&self, plan: &EnrollmentPlan, id: &str) -> String {
        self.store
            .handle(&self.admin, self.request(plan, id))
            .unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn flush(&self, host: &Host) {
        for pending in self.store.pending().unwrap() {
            host.submit(pending.job).unwrap();
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
    fn action(&self, request: &str, kind: &str) {
        self.send(json!({"action":"vm_action","request_id":request,"vm_id":9001,"kind":kind}));
    }
}

#[tokio::test]
async fn service_plan_is_read_only_and_admin_authorization_precedes_helper_contact() {
    let (lab, host) = Lab::new().await;
    let before_world = serde_json::to_value(lab.world()).unwrap();
    let before_journal = serde_json::to_value(host.journal().unwrap()).unwrap();
    let before = lab.store.read().unwrap();
    let socket = lab.directory.path().join("h.sock");
    let services = Services {
        host_socket: socket.clone(),
        migration: None,
    };
    for peer in [&lab.member, &lab.outsider] {
        let error = services
            .handle(
                &lab.store,
                peer,
                Request::PlanVmEnrollment {
                    vm_id: 9001,
                    owner: "alice".into(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "forbidden");
    }
    assert!(
        !socket.exists(),
        "authorization test must not need a running helper"
    );
    let host = Arc::new(host);
    let serving = host.clone();
    let address = socket.clone();
    let task = tokio::spawn(async move { host::serve(serving, &address).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let value = services
        .handle(
            &lab.store,
            &lab.admin,
            Request::PlanVmEnrollment {
                vm_id: 9001,
                owner: "alice".into(),
            },
        )
        .await
        .unwrap();
    let plan: EnrollmentPlan = serde_json::from_value(value).unwrap();
    assert!(plan.eligible && plan.blocked_reasons.is_empty());
    assert_eq!(plan.vm.disk_mib, 512);
    assert_eq!(plan.root_guid, lab.world().datasets[ROOT]["guid"]);
    assert_eq!(plan.cloud_init_guid, lab.world().datasets[CI]["guid"]);
    assert_eq!(serde_json::to_value(lab.world()).unwrap(), before_world);
    assert_eq!(
        serde_json::to_value(host.journal().unwrap()).unwrap(),
        before_journal
    );
    let after = lab.store.read().unwrap();
    assert!(after.vms.is_empty() && after.enrollments.is_empty());
    assert_eq!(after.operations.len(), before.operations.len());
    assert_eq!(
        after.users["alice"].budget_mib,
        before.users["alice"].budget_mib
    );
    assert!(after.enrollment_plans.contains_key(&plan.plan_id));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn exact_review_survives_restarts_accounts_once_and_supports_lifecycle_and_deletion() {
    let (lab, host) = Lab::new().await;
    let original = lab.world();
    let plan = lab.plan(&host).await;
    let request = lab.request(&plan, "enroll-once");
    let accepted = lab.store.handle(&lab.admin, request.clone()).unwrap();
    assert_eq!(
        accepted,
        lab.store.handle(&lab.admin, request.clone()).unwrap()
    );
    let id = accepted["operation_id"].as_str().unwrap().to_string();
    let budget = lab.send(json!({"action":"dashboard"}))["users"].clone();
    assert!(!budget.is_null());
    assert_eq!(
        lab.store.read().unwrap().vms[&9001].state,
        VmState::Creating
    );
    assert!(lab.store.read().unwrap().enrollments.is_empty());
    assert_eq!(
        serde_json::to_value(lab.world()).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    drop(host);
    let lab = lab.reopen();
    let host = lab.open().await;
    lab.flush(&host).await;
    let world = lab.world();
    let catalog = lab.store.read().unwrap();
    assert_eq!(catalog.vms[&9001].state, VmState::Stopped);
    assert_eq!(catalog.enrollments[&9001].plan.plan_id, plan.plan_id);
    assert_eq!(catalog.users["alice"].budget_mib, 2048);
    assert_eq!(world.datasets[ROOT]["volsize"], (512 * MIB).to_string());
    assert_eq!(
        world.vms[&9001].config["description"],
        format!("pve-dev-control:{ENV}:alice:9001")
    );
    assert_eq!(world.vms[&9001].config["protection"], "1");
    assert_eq!(
        world.datasets[ROOT]["org.pve-dev:owner"],
        world.vms[&9001].config["description"]
    );
    for key in ["guid", "createtxg", "volsize", "refreservation"] {
        assert_eq!(world.datasets[ROOT][key], original.datasets[ROOT][key]);
        assert_eq!(world.datasets[CI][key], original.datasets[CI][key]);
    }
    for (key, value) in &original.vms[&9001].config {
        if !["description", "protection", "digest"].contains(&key.as_str()) {
            assert_eq!(
                &world.vms[&9001].config[key], value,
                "unexpected change to {key}"
            );
        }
    }
    let machine = &host.journal().unwrap().machines[&9001];
    assert_eq!(machine.create_operation, id);
    assert!(machine.pinned_config.is_some());
    for key in ["vm", "volume", "attach", "cloud_init"] {
        assert!(machine.identities.contains_key(key), "missing {key}");
    }
    let retried = lab.store.handle(&lab.admin, request).unwrap();
    assert_eq!(retried["operation_id"], accepted["operation_id"]);
    assert_eq!(retried["status"], "verified");
    let current = lab.store.handle(&lab.member, Request::Budget {}).unwrap();
    assert_eq!(current["allocated_mib"], disk_charge(&plan.vm));
    assert_eq!(current["min_memory_allocated_mib"], 128);
    assert_eq!(current["memory_allocated_mib"], 512);
    assert_eq!(current["cores_allocated"], 2);
    drop(host);
    let lab = lab.reopen();
    let host = lab.open().await;
    lab.action("start", "start");
    lab.flush(&host).await;
    assert!(lab.world().vms[&9001].running);
    lab.action("shutdown", "shutdown");
    lab.flush(&host).await;
    assert!(!lab.world().vms[&9001].running);
    lab.action("delete", "delete");
    lab.flush(&host).await;
    assert!(
        !lab.world().datasets.contains_key(ROOT)
            && !lab.world().datasets.contains_key(CI)
            && !lab.world().vms.contains_key(&9001)
    );
    assert_eq!(lab.store.read().unwrap().vms[&9001].state, VmState::Deleted);
    assert_eq!(
        lab.store.handle(&lab.member, Request::Budget {}).unwrap()["allocated_mib"],
        0
    );
    assert!(
        host.handle(HostRequest::PlanEnrollment {
            environment: ENV.into(),
            vm_id: 9001,
            owner: "alice".into()
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn changed_config_disk_identity_and_running_state_block_before_any_adoption_effect() {
    for change in ["config", "root-guid", "ci-guid", "root-txg", "running"] {
        let (lab, host) = Lab::new().await;
        let plan = lab.plan(&host).await;
        let id = lab.accept(&plan, "enroll");
        lab.edit(|world| match change {
            "config" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("cores".into(), "3".into());
            }
            "root-guid" => {
                world
                    .datasets
                    .get_mut(ROOT)
                    .unwrap()
                    .insert("guid".into(), "44444".into());
            }
            "ci-guid" => {
                world
                    .datasets
                    .get_mut(CI)
                    .unwrap()
                    .insert("guid".into(), "55555".into());
            }
            "root-txg" => {
                world
                    .datasets
                    .get_mut(ROOT)
                    .unwrap()
                    .insert("createtxg".into(), "66666".into());
            }
            "running" => {
                world.vms.get_mut(&9001).unwrap().running = true;
            }
            _ => unreachable!(),
        });
        let before = serde_json::to_value(lab.world()).unwrap();
        host.submit(lab.store.pending().unwrap()[0].job.clone())
            .unwrap();
        host.run_pending().await.unwrap();
        let journal = host.journal().unwrap();
        let result = &journal.jobs[&id];
        assert_eq!(
            result.status,
            OperationStatus::Blocked,
            "{change}: {:?}",
            result.error
        );
        assert_eq!(
            serde_json::to_value(lab.world()).unwrap(),
            before,
            "pre-effect refusal changed {change}"
        );
        assert!(!journal.machines.contains_key(&9001));
        lab.store
            .progress(
                &id,
                result.status.clone(),
                &result.step,
                result.error.clone(),
            )
            .unwrap();
        assert!(lab.store.read().unwrap().enrollments.is_empty());
        assert_eq!(lab.store.read().unwrap().vms[&9001].state, VmState::Blocked);
    }
}

#[tokio::test]
async fn unmanaged_foreign_devices_claims_and_owners_are_refused_without_effects() {
    for change in [
        "extra-disk",
        "passthrough",
        "hook",
        "root-owner",
        "ci-owner",
        "vm-owner",
        "other-owner",
        "snapshot",
        "autostart",
        "thin-ci",
        "zero-cores",
        "oversized-memory",
    ] {
        let (lab, host) = Lab::new().await;
        lab.edit(|world| match change {
            "extra-disk" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("scsi1".into(), "other:disk".into());
            }
            "passthrough" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("hostpci0".into(), "0000:01:00.0".into());
            }
            "hook" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("hookscript".into(), "local:snippets/hook.sh".into());
            }
            "root-owner" => {
                world.datasets.get_mut(ROOT).unwrap().insert(
                    "org.pve-dev:owner".into(),
                    format!("pve-dev-control:{ENV}:bob:9002"),
                );
            }
            "ci-owner" => {
                world.datasets.get_mut(CI).unwrap().insert(
                    "org.pve-dev:owner".into(),
                    format!("pve-dev-control:{ENV}:bob"),
                );
            }
            "vm-owner" => {
                world.vms.get_mut(&9001).unwrap().config.insert(
                    "description".into(),
                    format!("pve-dev-control:{ENV}:bob:9001"),
                );
            }
            "snapshot" => {
                world
                    .datasets
                    .insert(format!("{ROOT}@manual"), BTreeMap::new());
            }
            "autostart" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("onboot".into(), "1".into());
            }
            "thin-ci" => {
                world
                    .datasets
                    .get_mut(CI)
                    .unwrap()
                    .insert("refreservation".into(), "0".into());
            }
            "zero-cores" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("cores".into(), "0".into());
            }
            "oversized-memory" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("memory".into(), "1024".into());
            }
            "other-owner" => {}
            _ => unreachable!(),
        });
        let before_world = serde_json::to_value(lab.world()).unwrap();
        let before_journal = serde_json::to_value(host.journal().unwrap()).unwrap();
        let owner = if change == "other-owner" {
            "bob"
        } else {
            "alice"
        };
        let result = host
            .handle(HostRequest::PlanEnrollment {
                environment: ENV.into(),
                vm_id: 9001,
                owner: owner.into(),
            })
            .await;
        assert!(result.is_err(), "accepted foreign fixture: {change}");
        assert_eq!(
            serde_json::to_value(lab.world()).unwrap(),
            before_world,
            "{change}"
        );
        assert_eq!(
            serde_json::to_value(host.journal().unwrap()).unwrap(),
            before_journal,
            "{change}"
        );
    }
}

#[tokio::test]
async fn later_lifecycle_refuses_pinned_configuration_and_backing_identity_drift_before_effect() {
    for change in ["smbios", "extra-device", "root-guid", "ci-guid"] {
        let (lab, host) = Lab::new().await;
        let plan = lab.plan(&host).await;
        lab.accept(&plan, "enroll");
        lab.flush(&host).await;
        let pinned = host.journal().unwrap().machines[&9001]
            .pinned_config
            .clone();
        lab.edit(|world| match change {
            "smbios" => {
                world.vms.get_mut(&9001).unwrap().config.insert(
                    "smbios1".into(),
                    "uuid=ffffffff-ffff-4fff-8fff-ffffffffffff".into(),
                );
            }
            "extra-device" => {
                world
                    .vms
                    .get_mut(&9001)
                    .unwrap()
                    .config
                    .insert("usb0".into(), "host=1234:5678".into());
            }
            "root-guid" => {
                world
                    .datasets
                    .get_mut(ROOT)
                    .unwrap()
                    .insert("guid".into(), "88888".into());
            }
            "ci-guid" => {
                world
                    .datasets
                    .get_mut(CI)
                    .unwrap()
                    .insert("guid".into(), "99999".into());
            }
            _ => unreachable!(),
        });
        let before = serde_json::to_value(lab.world()).unwrap();
        lab.action("start-drifted", "start");
        let pending = lab.store.pending().unwrap().pop().unwrap();
        host.submit(pending.job).unwrap();
        host.run_pending().await.unwrap();
        let journal = host.journal().unwrap();
        let result = &journal.jobs[&pending.public.operation_id];
        assert_eq!(
            result.status,
            OperationStatus::Blocked,
            "{change}: {:?}",
            result.error
        );
        assert_eq!(
            serde_json::to_value(lab.world()).unwrap(),
            before,
            "drifted {change} caused a lifecycle effect"
        );
        assert_eq!(journal.machines[&9001].pinned_config, pinned);
        lab.store
            .progress(
                &pending.public.operation_id,
                result.status.clone(),
                &result.step,
                result.error.clone(),
            )
            .unwrap();
        assert_eq!(lab.store.read().unwrap().vms[&9001].state, VmState::Blocked);
        assert_eq!(
            lab.store.handle(&lab.member, Request::Budget {}).unwrap()["allocated_mib"],
            disk_charge(&plan.vm)
        );
    }
}

#[tokio::test]
async fn catalog_rejects_unreviewed_unconfirmed_foreign_and_stale_enrollment() {
    let (lab, host) = Lab::new().await;
    let plan = lab.plan(&host).await;
    let mut requests = vec![lab.request(&plan, "member"), lab.request(&plan, "unknown")];
    for (key, value) in [
        ("owner", json!("bob")),
        ("vm_id", json!(9002)),
        ("confirm_enrollment", json!(false)),
        ("plan_id", json!("a".repeat(64))),
    ] {
        let mut value_request = serde_json::to_value(lab.request(&plan, key)).unwrap();
        value_request[key] = value;
        requests.push(serde_json::from_value(value_request).unwrap());
    }
    let before = serde_json::to_value(lab.store.read().unwrap()).unwrap();
    for (index, request) in requests.into_iter().enumerate() {
        let peer = if index == 0 {
            &lab.member
        } else if index == 1 {
            &lab.outsider
        } else {
            &lab.admin
        };
        assert!(lab.store.handle(peer, request).is_err());
        assert_eq!(
            serde_json::to_value(lab.store.read().unwrap()).unwrap(),
            before
        );
    }
    let mut stale = plan.clone();
    stale.observed_at = pve_dev::control::store::now().saturating_sub(301);
    lab.store
        .remember_enrollment_plan(&lab.admin, stale.clone())
        .unwrap();
    assert!(
        lab.store
            .handle(&lab.admin, lab.request(&stale, "stale"))
            .is_err()
    );
    assert!(lab.store.read().unwrap().vms.is_empty());
}

#[tokio::test]
async fn enrollment_crash_boundaries_reopen_conservatively_without_repeating_effects() {
    for point in [
        FaultPoint::BeforeEffect,
        FaultPoint::AfterEffect,
        FaultPoint::AfterReceipt,
    ] {
        for reboot in [false, true] {
            let (lab, host) = Lab::new().await;
            let plan = lab.plan(&host).await;
            let id = lab.accept(&plan, "enroll");
            let job = lab.store.pending().unwrap()[0].job.clone();
            lab.edit(|world| {
                world.faults.push(Fault {
                    step: Step::Enroll,
                    point,
                })
            });
            host.submit(job.clone()).unwrap();
            assert!(host.run_pending().await.is_err());
            drop(host);
            if reboot {
                lab.edit(World::new_boot);
            }
            let lab = lab.reopen();
            let host = lab.open().await;
            let before = serde_json::to_value(lab.world()).unwrap();
            host.run_pending().await.unwrap();
            let journal = host.journal().unwrap();
            let result = &journal.jobs[&id];
            let wanted = if point == FaultPoint::AfterReceipt {
                OperationStatus::Verified
            } else {
                OperationStatus::Blocked
            };
            assert_eq!(
                result.status, wanted,
                "{point:?}, reboot={reboot}: {:?}",
                result.error
            );
            assert_eq!(host.submit(job).unwrap().status, wanted);
            assert_eq!(
                serde_json::to_value(lab.world()).unwrap(),
                before,
                "uncertain enrollment replayed effects"
            );
            lab.store
                .progress(
                    &id,
                    result.status.clone(),
                    &result.step,
                    result.error.clone(),
                )
                .unwrap();
            assert_eq!(
                lab.store.read().unwrap().enrollments.contains_key(&9001),
                point == FaultPoint::AfterReceipt
            );
            assert_eq!(lab.world().datasets[ROOT]["guid"], plan.root_guid);
            if point == FaultPoint::AfterEffect {
                assert_eq!(lab.world().vms[&9001].config["protection"], "1");
                assert!(!journal.machines.contains_key(&9001));
                assert!(lab.store.handle(&lab.admin,serde_json::from_value(json!({"action":"vm_action","request_id":"unsafe-delete","vm_id":9001,"kind":"delete"})).unwrap()).is_err());
            }
        }
    }
}
