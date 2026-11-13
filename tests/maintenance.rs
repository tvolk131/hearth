use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
};

use iroh::SecretKey;
use pve_dev::control::{
    host::{Host, HostConfig, HostRequest, SimulatedWorld},
    maintenance::{self, Pins, QUARANTINE},
    protocol::{OperationStatus, Request},
    store::Binding,
    transport::{self, Descriptor, Network, Profile, ServerConfig},
};
use serde_json::Value;
use tempfile::TempDir;

const ENV: &str = "0123456789abcdef0123456789abcdef";

struct Fixture {
    _root: TempDir,
    root: PathBuf,
    controller: PathBuf,
    helper: PathBuf,
    world: PathBuf,
    pins: Pins,
    admin: String,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_guest(false).await
    }
    async fn with_guest(guest: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = root.join("controller");
        let helper = root.join("helper");
        let world = root.join("world.json");
        let admin = SecretKey::generate().public().to_string();
        let config = ServerConfig {
            schema: 1,
            binding: Binding {
                environment: ENV.into(),
                backend: "simulated".into(),
                disk_capacity_mib: if guest { 16384 } else { 2048 },
                min_memory_capacity_mib: 1024,
                guest_images: if guest { vec!["a".repeat(64)] } else { vec![] },
                physical_trial: None,
                managed: None,
            },
            host_socket: root.join("host.sock"),
            network: Network::Loopback,
            port: 0,
            migration_network: None,
        };
        transport::initialize(&controller, &config, &admin).unwrap();
        let host = Host::open(HostConfig::simulated(helper.clone(), ENV, world.clone()))
            .await
            .unwrap();
        let (_, store, identity) = transport::open(&controller).unwrap();
        if guest {
            SimulatedWorld::new(world.clone())
                .edit(|w| {
                    w.pool_size_mib = 24576;
                    Ok(())
                })
                .unwrap();
        }
        for operation in store.pending().unwrap() {
            host.handle(HostRequest::Submit {
                environment: ENV.into(),
                job: operation.job,
            })
            .await
            .unwrap();
        }
        host.run_pending().await.unwrap();
        for job in host.journal().unwrap().jobs.values() {
            store
                .progress(
                    &job.job.operation_id,
                    job.status.clone(),
                    &job.step,
                    job.error.clone(),
                )
                .unwrap();
        }
        let pins = Pins {
            environment: ENV.into(),
            server_id: identity.public().to_string(),
            pool_guid: host.journal().unwrap().binding.pool_guid,
        };
        drop(store);
        drop(host);
        Self {
            _root: temp,
            root,
            controller,
            helper,
            world,
            pins,
            admin,
        }
    }
    fn backup(&self, name: &str) -> maintenance::Receipt {
        maintenance::backup(
            &self.controller,
            &self.helper,
            Some(&self.world),
            &self.root.join(name),
            &self.pins,
            true,
        )
        .unwrap()
    }
}

#[tokio::test]
async fn paired_restore_preserves_accepted_intent_idempotency_identity_and_revocation_but_never_starts()
 {
    let f = Fixture::new().await;
    let (_, store, _) = transport::open(&f.controller).unwrap();
    let device = SecretKey::generate().public().to_string();
    store
        .handle(
            &f.admin,
            Request::EnrollDevice {
                user_id: "erin".into(),
                endpoint_id: device.clone(),
                label: "retired".into(),
            },
        )
        .unwrap();
    store
        .handle(
            &f.admin,
            Request::RevokeDevice {
                endpoint_id: device.clone(),
            },
        )
        .unwrap();
    let request: Request = serde_json::from_value(serde_json::json!({"action":"create_vm", "request_id":"pending-create",
        "name":"restore-me", "disk_mib":32, "memory_mib":128, "min_memory_mib":128, "cores":1, "owner":null})).unwrap();
    let accepted = store.handle(&f.admin, request.clone()).unwrap();
    let before = serde_json::to_value(store.read().unwrap()).unwrap();
    drop(store);
    let receipt = f.backup("backup");
    let restored = f.root.join("restored");
    let result = maintenance::restore(
        &f.root.join("backup"),
        &restored,
        &f.pins,
        &receipt.payload_sha256,
    )
    .unwrap();
    assert!(result.quarantined && !result.activates_workers);
    assert!(transport::open(&restored.join("controller")).is_err());
    assert!(
        Host::open(HostConfig::simulated(
            restored.join("helper"),
            ENV,
            restored.join("simulated-world.json")
        ))
        .await
        .is_err()
    );
    let restored_store = pve_dev::control::store::Store::open(
        &restored.join("controller/state.redb"),
        &serde_json::from_value(before["binding"].clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(restored_store.read().unwrap()).unwrap(),
        before
    );
    assert!(!restored_store.authorized(&device));
    assert_eq!(restored_store.handle(&f.admin, request).unwrap(), accepted);
    assert_eq!(
        transport::load_identity(&restored.join("controller/identity.key"))
            .unwrap()
            .public()
            .to_string(),
        f.pins.server_id
    );
    assert_eq!(
        fs::read(restored.join("simulated-world.json")).unwrap(),
        serde_json::to_vec(&SimulatedWorld::new(f.world).load().unwrap()).unwrap()
    );
    assert_eq!(
        restored_store.pending().unwrap()[0].public.status,
        OperationStatus::Queued
    );
}

#[tokio::test]
async fn schema5_restore_preserves_queued_and_prepared_shrink_holds_without_activation() {
    async fn flush(host: &Host, store: &pve_dev::control::store::Store) {
        for operation in store.pending().unwrap() {
            host.submit(operation.job).unwrap();
            host.run_pending().await.unwrap();
            let journal = host.journal().unwrap();
            let proof = &journal.jobs[&operation.public.operation_id];
            assert_eq!(proof.status, OperationStatus::Verified, "{:?}", proof.error);
            store
                .progress(
                    &operation.public.operation_id,
                    proof.status.clone(),
                    &proof.step,
                    proof.error.clone(),
                )
                .unwrap();
        }
    }
    for prepared in [false, true] {
        let f = Fixture::with_guest(true).await;
        let host = Host::open(HostConfig::simulated(
            f.helper.clone(),
            ENV,
            f.world.clone(),
        ))
        .await
        .unwrap();
        let (_, store, _) = transport::open(&f.controller).unwrap();
        let send = |value| {
            store
                .handle(&f.admin, serde_json::from_value(value).unwrap())
                .unwrap()
        };
        send(
            serde_json::json!({"action":"create_user","request_id":"owner","user_id":"alice","budget_mib":6144,
            "min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":1}),
        );
        flush(&host, &store).await;
        send(
            serde_json::json!({"action":"create_vm","request_id":"image","name":"dev-alice","owner":"alice","disk_mib":4096,
            "memory_mib":512,"min_memory_mib":128,"cores":1,"guest":{"image_sha256":"a".repeat(64),
            "ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}}),
        );
        flush(&host, &store).await;
        send(
            serde_json::json!({"action":"shrink_vm","request_id":"shrink","vm_id":9001,"expected_disk_mib":4096,
            "target_disk_mib":3072,"confirm_downtime":true}),
        );
        if prepared {
            flush(&host, &store).await;
        }
        let before = store.read().unwrap();
        assert_eq!(before.disk_shrinks[&9001].prepared, prepared);
        drop(store);
        drop(host);
        let receipt = f.backup("shrink-backup");
        let restored = f.root.join("shrink-restored");
        let result = maintenance::restore(
            &f.root.join("shrink-backup"),
            &restored,
            &f.pins,
            &receipt.payload_sha256,
        )
        .unwrap();
        assert!(result.quarantined);
        assert!(transport::open(&restored.join("controller")).is_err());
        assert!(
            Host::open(HostConfig::simulated(
                restored.join("helper"),
                ENV,
                restored.join("simulated-world.json")
            ))
            .await
            .is_err()
        );
        let snapshot = pve_dev::control::store::Store::open(
            &restored.join("controller/state.redb"),
            &before.binding,
        )
        .unwrap()
        .read()
        .unwrap();
        assert_eq!(snapshot.schema, 5);
        assert_eq!(
            serde_json::to_value(snapshot).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert_eq!(
            fs::read(restored.join("simulated-world.json")).unwrap(),
            serde_json::to_vec(&SimulatedWorld::new(f.world).load().unwrap()).unwrap()
        );
    }
}

#[tokio::test]
async fn schema6_growth_claim_and_partially_committed_profile_survive_quarantined_restore() {
    use pve_dev::control::host::{Fault, FaultPoint, Step};
    for interrupted in [false, true] {
        let f = Fixture::with_guest(true).await;
        let mut host = Host::open(HostConfig::simulated(
            f.helper.clone(),
            ENV,
            f.world.clone(),
        ))
        .await
        .unwrap();
        let (_, store, _) = transport::open(&f.controller).unwrap();
        async fn flush(host: &Host, store: &pve_dev::control::store::Store) {
            for r in store.pending().unwrap() {
                host.submit(r.job).unwrap();
                host.run_pending().await.unwrap();
                let proof = host.journal().unwrap().jobs[&r.public.operation_id].clone();
                assert_eq!(proof.status, OperationStatus::Verified, "{:?}", proof.error);
                store
                    .progress(
                        &r.public.operation_id,
                        proof.status,
                        &proof.step,
                        proof.error,
                    )
                    .unwrap();
            }
        }
        let send = |v| {
            store
                .handle(&f.admin, serde_json::from_value(v).unwrap())
                .unwrap()
        };
        send(
            serde_json::json!({"action":"create_user","request_id":"growth-owner","user_id":"alice","budget_mib":8192,
            "min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":1}),
        );
        flush(&host, &store).await;
        send(
            serde_json::json!({"action":"create_vm","request_id":"growth-guest","name":"dev-alice","owner":"alice","disk_mib":4096,
            "memory_mib":512,"min_memory_mib":128,"cores":1,"guest":{"image_sha256":"a".repeat(64),
            "ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}}),
        );
        flush(&host, &store).await;
        let accepted = send(
            serde_json::json!({"action":"grow_vm","request_id":"growth","vm_id":9001,
            "expected_disk_mib":4096,"target_disk_mib":6144,"confirm_downtime":true}),
        );
        if interrupted {
            SimulatedWorld::new(f.world.clone())
                .edit(|world| {
                    world.faults.push(Fault {
                        step: Step::GrowFilesystem,
                        point: FaultPoint::AfterReceipt,
                    });
                    Ok(())
                })
                .unwrap();
            for r in store.pending().unwrap() {
                host.submit(r.job).unwrap();
            }
            assert!(host.run_pending().await.is_err());
            drop(host);
            host = Host::open(HostConfig::simulated(
                f.helper.clone(),
                ENV,
                f.world.clone(),
            ))
            .await
            .unwrap();
            let proof =
                host.journal().unwrap().jobs[accepted["operation_id"].as_str().unwrap()].clone();
            // A fully receipted filesystem phase may continue normally later;
            // exporting this stopped pair must preserve the pending delta now.
            assert_eq!(proof.status, OperationStatus::Running);
            assert_eq!(
                host.journal().unwrap().machines[&9001].profile.disk_mib,
                6144
            );
            store
                .progress(
                    &proof.job.operation_id,
                    proof.status,
                    &proof.step,
                    proof.error,
                )
                .unwrap();
        }
        let before = store.read().unwrap();
        assert_eq!(before.schema, 6);
        assert_eq!(before.vms[&9001].disk_mib, 4096);
        assert_eq!(before.disk_growths[&9001].spec.target_disk_mib, 6144);
        drop(store);
        drop(host);
        let receipt = f.backup("growth-backup");
        let restored = f.root.join("growth-restored");
        let result = maintenance::restore(
            &f.root.join("growth-backup"),
            &restored,
            &f.pins,
            &receipt.payload_sha256,
        )
        .unwrap();
        assert!(result.quarantined);
        assert!(transport::open(&restored.join("controller")).is_err());
        assert!(
            Host::open(HostConfig::simulated(
                restored.join("helper"),
                ENV,
                restored.join("simulated-world.json")
            ))
            .await
            .is_err()
        );
        let recovered = pve_dev::control::store::Store::open(
            &restored.join("controller/state.redb"),
            &before.binding,
        )
        .unwrap()
        .read()
        .unwrap();
        assert_eq!(
            serde_json::to_value(recovered).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }
}

#[tokio::test]
async fn running_controller_or_helper_and_missing_quiescence_refuse_backup() {
    let f = Fixture::new().await;
    assert!(
        maintenance::backup(
            &f.controller,
            &f.helper,
            Some(&f.world),
            &f.root.join("no-assertion"),
            &f.pins,
            false
        )
        .is_err()
    );
    let (_, controller, _) = transport::open(&f.controller).unwrap();
    assert!(
        maintenance::backup(
            &f.controller,
            &f.helper,
            Some(&f.world),
            &f.root.join("active-controller"),
            &f.pins,
            true
        )
        .is_err()
    );
    drop(controller);
    let helper = Host::open(HostConfig::simulated(
        f.helper.clone(),
        ENV,
        f.world.clone(),
    ))
    .await
    .unwrap();
    assert!(
        maintenance::backup(
            &f.controller,
            &f.helper,
            Some(&f.world),
            &f.root.join("active-helper"),
            &f.pins,
            true
        )
        .is_err()
    );
    drop(helper);
    assert!(!f.root.join("active-helper").exists());
}

#[tokio::test]
async fn corrupt_partial_wrong_identity_and_existing_destinations_refused_before_restore() {
    let f = Fixture::new().await;
    let receipt = f.backup("backup");
    let input = f.root.join("backup");
    for field in ["environment", "server_id", "pool_guid"] {
        let mut pins = serde_json::to_value(&f.pins).unwrap();
        pins[field] = Value::String("a".repeat(32));
        assert!(
            maintenance::restore(
                &input,
                &f.root.join("bad"),
                &serde_json::from_value(pins).unwrap(),
                &receipt.payload_sha256
            )
            .is_err()
        );
    }
    assert!(!f.root.join("bad").exists());
    assert!(maintenance::restore(&input, &f.root, &f.pins, &receipt.payload_sha256).is_err());
    assert!(maintenance::verify(&input, &f.pins, &"0".repeat(64)).is_err());
    let bundle_path = input.join("bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["payload"]["catalog"]["devices"] = serde_json::json!({});
    fs::write(&bundle_path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    assert!(
        maintenance::restore(
            &input,
            &f.root.join("corrupt"),
            &f.pins,
            &receipt.payload_sha256
        )
        .is_err()
    );
    assert!(!f.root.join("corrupt").exists());
    fs::remove_file(bundle_path).unwrap();
    assert!(maintenance::verify(&input, &f.pins, &receipt.payload_sha256).is_err());
}

#[tokio::test]
async fn stale_helper_pair_missing_controller_terminal_proof_is_rejected() {
    let f = Fixture::new().await;
    let (_, store, _) = transport::open(&f.controller).unwrap();
    let accepted = store.handle(&f.admin, serde_json::from_value(serde_json::json!({"action":"create_vm", "request_id":"false-complete",
        "name":"stale", "disk_mib":32, "memory_mib":128, "min_memory_mib":128, "cores":1, "owner":null})).unwrap()).unwrap();
    store
        .progress(
            accepted["operation_id"].as_str().unwrap(),
            OperationStatus::Verified,
            "incorrect-test-proof",
            None,
        )
        .unwrap();
    drop(store);
    assert!(
        maintenance::backup(
            &f.controller,
            &f.helper,
            Some(&f.world),
            &f.root.join("stale"),
            &f.pins,
            true
        )
        .is_err()
    );
    assert!(!f.root.join("stale").exists());
}

#[tokio::test]
async fn symlink_hardlink_permissions_and_quarantine_types_are_refused() {
    let f = Fixture::new().await;
    let receipt = f.backup("backup");
    let link = f.root.join("link");
    symlink(f.root.join("backup"), &link).unwrap();
    assert!(maintenance::verify(&link, &f.pins, &receipt.payload_sha256).is_err());
    let bundle = f.root.join("backup/bundle.json");
    fs::hard_link(&bundle, f.root.join("hardlink")).unwrap();
    assert!(maintenance::verify(&f.root.join("backup"), &f.pins, &receipt.payload_sha256).is_err());
    fs::remove_file(f.root.join("hardlink")).unwrap();
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(maintenance::verify(&f.root.join("backup"), &f.pins, &receipt.payload_sha256).is_err());
    let marker = f.controller.join(QUARANTINE);
    symlink(f.root.join("does-not-exist"), &marker).unwrap();
    assert!(transport::open(&f.controller).is_err());
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    assert!(transport::open(&f.controller).is_err());
}

#[tokio::test]
async fn writable_output_parent_or_ancestor_cannot_redirect_private_bundle_or_restore() {
    let f = Fixture::new().await;
    let receipt = f.backup("backup");
    let unsafe_parent = f.root.join("unsafe-parent");
    fs::create_dir(&unsafe_parent).unwrap();
    fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        maintenance::backup(
            &f.controller,
            &f.helper,
            Some(&f.world),
            &unsafe_parent.join("export"),
            &f.pins,
            true
        )
        .is_err()
    );
    assert!(!unsafe_parent.join("export").exists());
    let private_child = unsafe_parent.join("private-child");
    fs::create_dir(&private_child).unwrap();
    fs::set_permissions(&private_child, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        maintenance::restore(
            &f.root.join("backup"),
            &private_child.join("restore"),
            &f.pins,
            &receipt.payload_sha256
        )
        .is_err()
    );
    assert!(!private_child.join("restore").exists());
    fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o700)).unwrap();
    let redirected = f.root.join("redirected");
    symlink(&private_child, &redirected).unwrap();
    assert!(
        maintenance::restore(
            &f.root.join("backup"),
            &redirected.join("restore"),
            &f.pins,
            &receipt.payload_sha256
        )
        .is_err()
    );
    assert!(!private_child.join("restore").exists());
}

#[tokio::test]
async fn interrupted_helper_intent_and_exact_fingerprint_are_preserved_in_pair() {
    let f = Fixture::new().await;
    let (_, store, _) = transport::open(&f.controller).unwrap();
    store.handle(&f.admin, serde_json::from_value(serde_json::json!({"action":"create_vm", "request_id":"submitted",
        "name":"submitted", "disk_mib":32, "memory_mib":128, "min_memory_mib":128, "cores":1, "owner":null})).unwrap()).unwrap();
    let host = Host::open(HostConfig::simulated(
        f.helper.clone(),
        ENV,
        f.world.clone(),
    ))
    .await
    .unwrap();
    let queued = store.pending().unwrap().pop().unwrap();
    host.handle(HostRequest::Submit {
        environment: ENV.into(),
        job: queued.job,
    })
    .await
    .unwrap();
    let journal = serde_json::to_value(host.journal().unwrap()).unwrap();
    drop(host);
    drop(store);
    let receipt = f.backup("submitted-backup");
    let bundle: Value =
        serde_json::from_slice(&fs::read(f.root.join("submitted-backup/bundle.json")).unwrap())
            .unwrap();
    assert_eq!(bundle["payload"]["journal"], journal);
    assert!(
        maintenance::verify(
            &f.root.join("submitted-backup"),
            &f.pins,
            &receipt.payload_sha256
        )
        .is_ok()
    );
}

#[tokio::test]
async fn real_direct_iroh_authenticated_rpc_survives_endpoint_restart_and_revocation() {
    let f = Fixture::new().await;
    let (_, store, key) = transport::open(&f.controller).unwrap();
    let endpoint = transport::endpoint(key.clone(), Network::Direct, 0, true)
        .await
        .unwrap();
    let identity = f.root.join("client.key");
    let client_key = transport::generate_identity(&identity).unwrap();
    store
        .handle(
            &f.admin,
            Request::EnrollDevice {
                user_id: "erin".into(),
                endpoint_id: client_key.public().to_string(),
                label: "direct".into(),
            },
        )
        .unwrap();
    let descriptor = Descriptor {
        schema: 1,
        endpoint: endpoint.addr(),
        environment: ENV.into(),
        network: Network::Direct,
    };
    let profile = Profile {
        schema: 1,
        identity,
        server: descriptor,
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(transport::serve(endpoint, Arc::clone(&store), async {
        let _ = rx.await;
    }));
    assert!(
        transport::rpc(&profile, Request::Whoami {})
            .await
            .unwrap()
            .ok
    );
    tx.send(()).unwrap();
    server.await.unwrap().unwrap();
    let endpoint = transport::endpoint(key, Network::Direct, 0, true)
        .await
        .unwrap();
    let mut reconnected = profile;
    reconnected.server.endpoint = endpoint.addr();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(transport::serve(endpoint, Arc::clone(&store), async {
        let _ = rx.await;
    }));
    assert!(
        transport::rpc(&reconnected, Request::Whoami {})
            .await
            .unwrap()
            .ok
    );
    store
        .handle(
            &f.admin,
            Request::RevokeDevice {
                endpoint_id: client_key.public().to_string(),
            },
        )
        .unwrap();
    assert!(
        !transport::rpc(&reconnected, Request::Whoami {})
            .await
            .unwrap()
            .ok
    );
    tx.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[test]
fn systemd_units_keep_daemon_unprivileged_and_helper_local_lab_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/systemd");
    let daemon = fs::read_to_string(root.join("pve-devd.service")).unwrap();
    let helper = fs::read_to_string(root.join("pve-dev-host.service")).unwrap();
    assert!(daemon.contains("User=pve-dev\n") && daemon.contains("NoNewPrivileges=yes"));
    assert!(daemon.contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6"));
    assert!(
        helper.contains("--lab-config /etc/pve-dev-host/lab.json")
            && helper.contains("Group=pve-dev")
    );
    assert!(
        helper.contains("KillMode=control-group") && helper.contains("StateDirectoryMode=0700")
    );
    assert!(!helper.contains("--simulate") && !helper.contains("CAP_SYS_ADMIN"));
}

#[tokio::test]
async fn schema_two_policy_edits_survive_paired_backup_and_quarantined_restore() {
    for verified in [false, true] {
        let f = Fixture::new().await;
        let (_, store, _) = transport::open(&f.controller).unwrap();
        let previous = store.read().unwrap().users["erin"].clone();
        let mut policy = previous.policy();
        policy.budget_mib = 256;
        let request = Request::UpdateUserPolicy {
            request_id: "policy-edit".into(),
            user_id: "erin".into(),
            expected_revision: previous.policy_revision,
            policy,
        };
        let accepted = store.handle(&f.admin, request.clone()).unwrap();
        if verified {
            let host = Host::open(HostConfig::simulated(
                f.helper.clone(),
                ENV,
                f.world.clone(),
            ))
            .await
            .unwrap();
            let job = store.pending().unwrap()[0].job.clone();
            let id = job.operation_id.clone();
            host.submit(job).unwrap();
            host.run_pending().await.unwrap();
            let proof = &host.journal().unwrap().jobs[&id];
            assert_eq!(proof.status, OperationStatus::Verified);
            store
                .progress(&id, proof.status.clone(), &proof.step, proof.error.clone())
                .unwrap();
            drop(host);
        }
        let before = serde_json::to_value(store.read().unwrap()).unwrap();
        assert_eq!(before["schema"], 2);
        drop(store);
        let receipt = f.backup("policy-backup");
        let restored = f.root.join("policy-restore");
        maintenance::restore(
            &f.root.join("policy-backup"),
            &restored,
            &f.pins,
            &receipt.payload_sha256,
        )
        .unwrap();
        assert!(transport::open(&restored.join("controller")).is_err());
        let restored_store = pve_dev::control::store::Store::open(
            &restored.join("controller/state.redb"),
            &serde_json::from_value(before["binding"].clone()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(restored_store.read().unwrap()).unwrap(),
            before
        );
        let retried = restored_store.handle(&f.admin, request).unwrap();
        assert_eq!(retried["operation_id"], accepted["operation_id"]);
        assert_eq!(
            retried["status"],
            if verified { "verified" } else { "queued" }
        );
    }
}

#[tokio::test]
async fn schema_three_pending_and_completed_empty_user_deletion_remain_quarantined_and_revoked() {
    for verified in [false, true] {
        let f = Fixture::new().await;
        let (_, store, _) = transport::open(&f.controller).unwrap();
        let host = Host::open(HostConfig::simulated(
            f.helper.clone(),
            ENV,
            f.world.clone(),
        ))
        .await
        .unwrap();
        store
            .handle(
                &f.admin,
                Request::CreateUser {
                    request_id: "new-alice".into(),
                    user_id: "alice".into(),
                    budget_mib: 128,
                    min_memory_budget_mib: 128,
                    max_memory_budget_mib: 1024,
                    max_cores: 4,
                    max_vms: 4,
                },
            )
            .unwrap();
        for record in store.pending().unwrap() {
            host.submit(record.job.clone()).unwrap();
            host.run_pending().await.unwrap();
            let proof = &host.journal().unwrap().jobs[&record.public.operation_id];
            store
                .progress(
                    &record.public.operation_id,
                    proof.status.clone(),
                    &proof.step,
                    proof.error.clone(),
                )
                .unwrap();
        }
        let device = SecretKey::generate().public().to_string();
        store
            .handle(
                &f.admin,
                Request::EnrollDevice {
                    user_id: "alice".into(),
                    endpoint_id: device.clone(),
                    label: "Alice".into(),
                },
            )
            .unwrap();
        let request = Request::DeleteUser {
            request_id: "delete-alice".into(),
            user_id: "alice".into(),
            expected_revision: 0,
        };
        let accepted = store.handle(&f.admin, request.clone()).unwrap();
        if verified {
            for record in store.pending().unwrap() {
                host.submit(record.job.clone()).unwrap();
                host.run_pending().await.unwrap();
                let proof = &host.journal().unwrap().jobs[&record.public.operation_id];
                store
                    .progress(
                        &record.public.operation_id,
                        proof.status.clone(),
                        &proof.step,
                        proof.error.clone(),
                    )
                    .unwrap();
            }
        }
        let before = serde_json::to_value(store.read().unwrap()).unwrap();
        assert_eq!(before["schema"], 3);
        assert!(!store.authorized(&device));
        drop(store);
        drop(host);
        let receipt = f.backup("user-backup");
        let restored = f.root.join("user-restore");
        maintenance::restore(
            &f.root.join("user-backup"),
            &restored,
            &f.pins,
            &receipt.payload_sha256,
        )
        .unwrap();
        assert!(transport::open(&restored.join("controller")).is_err());
        let restored_store = pve_dev::control::store::Store::open(
            &restored.join("controller/state.redb"),
            &serde_json::from_value(before["binding"].clone()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(restored_store.read().unwrap()).unwrap(),
            before
        );
        assert!(!restored_store.authorized(&device));
        assert_eq!(
            restored_store.handle(&f.admin, request).unwrap()["operation_id"],
            accepted["operation_id"]
        );
    }
}
