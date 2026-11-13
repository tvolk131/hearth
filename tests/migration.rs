use iroh::SecretKey;
use pve_dev::control::migration::*;
use serde_json::json;
use std::collections::BTreeMap;

struct Pair {
    source_key: SecretKey,
    target_key: SecretKey,
    source: TransferRecord,
    target: TransferRecord,
}
fn pair() -> Pair {
    let source_key = SecretKey::generate();
    let target_key = SecretKey::generate();
    let actor = SecretKey::generate().public().to_string();
    let config: BTreeMap<String, String> = BTreeMap::from([
        ("name".into(), "dev-test".into()),
        (
            "smbios1".into(),
            "uuid=11111111-2222-3333-4444-555555555555".into(),
        ),
    ]);
    let plan = Plan {
        schema: 1,
        migration_id: "a".repeat(64),
        source: Peer {
            server_id: source_key.public().to_string(),
            environment: "1".repeat(32),
        },
        destination: Peer {
            server_id: target_key.public().to_string(),
            environment: "2".repeat(32),
        },
        source_vm_id: 9001,
        target_vm_id: 9002,
        source_owner: "alice".into(),
        target_owner: "bob".into(),
        source_machine_sha256: "3".repeat(64),
        source_config_sha256: digest(&config),
        name: "dev-test".into(),
        disk_mib: 64,
        memory_mib: 512,
        min_memory_mib: 128,
        cores: 1,
        guest: None,
        byte_ceiling: 68 * 1024 * 1024,
        target_bridge: "vmbr77".into(),
    };
    let sg = Signed::sign(
        Grant {
            schema: 1,
            plan: plan.clone(),
            role: Role::Source,
            issuer: plan.source.clone(),
            recipient: plan.destination.clone(),
            actor: actor.clone(),
            nonce: "a".repeat(64),
            source_grant_sha256: None,
            issued_at: 1000,
            expires_at: 2000,
        },
        &source_key,
        "grant",
    )
    .unwrap();
    let dg = Signed::sign(
        Grant {
            schema: 1,
            plan: plan.clone(),
            role: Role::Destination,
            issuer: plan.destination.clone(),
            recipient: plan.source.clone(),
            actor: actor.clone(),
            nonce: "b".repeat(64),
            source_grant_sha256: Some(digest(&sg)),
            issued_at: 1000,
            expires_at: 1900,
        },
        &target_key,
        "grant",
    )
    .unwrap();
    let route = pve_dev::control::transport::Descriptor {
        schema: 1,
        endpoint: iroh::EndpointAddr::new(target_key.public()),
        environment: plan.destination.environment.clone(),
        network: pve_dev::control::transport::Network::Loopback,
    };
    let mut source = TransferRecord::source(plan, sg.clone(), actor.clone(), route, 1001).unwrap();
    source.set_destination_grant(dg.clone(), 1001).unwrap();
    let target = TransferRecord::destination(sg, dg, actor, 1001).unwrap();
    Pair {
        source_key,
        target_key,
        source,
        target,
    }
}
fn stopped(p: &mut Pair) {
    let manifest = Manifest {
        plan_sha256: digest(&p.source.plan),
        source_fence: json!({"lock":"migrate","onboot":0,"stopped":true}),
        config: BTreeMap::from([
            ("name".into(), "dev-test".into()),
            (
                "smbios1".into(),
                "uuid=11111111-2222-3333-4444-555555555555".into(),
            ),
        ]),
        volumes: BTreeMap::from([
            (
                "root".into(),
                VolumeManifest {
                    bytes: 64 * 1024 * 1024,
                    sha256: "4".repeat(64),
                    guid: "11".into(),
                    createtxg: "101".into(),
                },
            ),
            (
                "cloudinit".into(),
                VolumeManifest {
                    bytes: 4 * 1024 * 1024,
                    sha256: "5".repeat(64),
                    guid: "12".into(),
                    createtxg: "102".into(),
                },
            ),
        ]),
        snippets: BTreeMap::new(),
    };
    let manifest = Signed::sign(manifest, &p.source_key, "manifest").unwrap();
    p.source.set_manifest(manifest.clone(), 1002).unwrap();
    p.target.set_manifest(manifest, 1002).unwrap();
}
fn ready(p: &mut Pair) {
    stopped(p);
    let receipt=Signed::sign(ReadyReceipt{migration_id:p.source.plan.migration_id.clone(),plan_sha256:digest(&p.source.plan),manifest_sha256:digest(&p.source.manifest.as_ref().unwrap().value),destination:p.source.plan.destination.clone(),target_identities:json!({"root":{"guid":"21","createtxg":"201"},"cloudinit":{"guid":"22","createtxg":"202"},"inactive":true}),destination_grant_sha256:digest(p.target.destination_grant.as_ref().unwrap())},&p.target_key,"ready").unwrap();
    p.source.set_ready(receipt.clone(), 1003).unwrap();
    p.target.set_ready(receipt, 1003).unwrap();
}
#[test]
fn both_administrators_authorize_exact_peers_resources_and_lifetime() {
    let p = pair();
    p.source.validate().unwrap();
    p.target.validate().unwrap();
    let mut grant = p.target.destination_grant.clone().unwrap();
    grant.value.plan.target_vm_id = 9003;
    assert!(grant.validate_grant(1004, false).is_err());
    let mut grant = p.target.destination_grant.clone().unwrap();
    grant.value.recipient.environment = "f".repeat(32);
    assert!(grant.validate_grant(1004, false).is_err());
    assert!(p.source.source_grant.validate_grant(2001, false).is_err());
    let mut grant = p.source.source_grant.clone();
    grant.value.expires_at = grant.value.issued_at + MAX_GRANT_SECONDS + 1;
    grant = Signed::sign(grant.value, &p.source_key, "grant").unwrap();
    assert!(grant.validate_grant(1004, false).is_err());
    let mut grant = p.source.source_grant.clone();
    grant.signature = p.target_key.sign(b"unrelated").to_string();
    assert!(grant.validate_grant(1004, false).is_err());
}
#[test]
fn signatures_are_domain_separated_and_source_cannot_forge_destination_ready() {
    let mut p = pair();
    stopped(&mut p);
    let before = serde_json::to_value(&p.source).unwrap();
    let ready = ReadyReceipt {
        migration_id: p.source.plan.migration_id.clone(),
        plan_sha256: digest(&p.source.plan),
        manifest_sha256: digest(&p.source.manifest.as_ref().unwrap().value),
        destination: p.source.plan.destination.clone(),
        target_identities: json!({"inactive":true}),
        destination_grant_sha256: digest(p.target.destination_grant.as_ref().unwrap()),
    };
    let forged = Signed::sign(ready.clone(), &p.source_key, "ready").unwrap();
    assert!(p.source.set_ready(forged, 1004).is_err());
    assert_eq!(
        serde_json::to_value(&p.source).unwrap(),
        before,
        "rejected readiness must leave the state unchanged"
    );
    let wrong_domain = Signed::sign(ready, &p.target_key, "manifest").unwrap();
    assert!(
        wrong_domain
            .verify(&p.target_key.public().to_string(), "ready")
            .is_err()
    );
}
#[test]
fn lost_handoff_reply_and_expiry_keep_source_fenced_and_allow_same_proof_retry() {
    let mut p = pair();
    ready(&mut p);
    let proof = p.source.relinquish(&p.source_key, 1004).unwrap();
    let serialized = serde_json::to_vec(&p.source).unwrap();
    p.source = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(p.source.phase, Phase::Relinquished);
    assert!(p.source.holds_vm("alice", 9001));
    assert!(p.source.abort(5000).is_err());
    assert_eq!(p.source.relinquish(&p.source_key, 5000).unwrap(), proof);
    p.target.accept(proof.clone(), 5000).unwrap();
    p.target.accept(proof.clone(), 5001).unwrap();
    assert_eq!(p.target.phase, Phase::Accepted);
    assert!(p.target.holds_vm("bob", 9002));
    let acceptance = Signed::sign(
        Acceptance {
            migration_id: p.source.plan.migration_id.clone(),
            plan_sha256: digest(&p.source.plan),
            handoff_sha256: digest(&proof.value),
            destination: p.source.plan.destination.clone(),
            accepted_at: 5000,
        },
        &p.target_key,
        "accepted",
    )
    .unwrap();
    p.source.acknowledge(acceptance.clone(), 5001).unwrap();
    p.target.acknowledge(acceptance, 5001).unwrap();
    p.target.phase = Phase::Complete;
    p.target.validate().unwrap();
    assert!(!p.target.holds_vm("bob", 9002));
    assert!(p.source.holds_vm("alice", 9001));
}
#[test]
fn expiration_before_commit_never_grants_a_late_ownership_decision() {
    let mut p = pair();
    ready(&mut p);
    assert!(p.source.relinquish(&p.source_key, 2000).is_err());
    assert!(p.source.handoff.is_none());
    assert!(p.source.holds_vm("alice", 9001));
}
#[test]
fn precommit_abort_excludes_later_handoff_and_keeps_opposite_endpoint_independent() {
    let mut p = pair();
    ready(&mut p);
    p.source.abort(1004).unwrap();
    assert!(p.source.relinquish(&p.source_key, 1005).is_err());
    assert_eq!(p.target.phase, Phase::Ready);
    assert!(p.target.holds_vm("bob", 9002));
    let mut r = p.target.clone();
    r.phase = Phase::Cleaning;
    r.abort_proof = Some(
        Signed::sign(
            AbortReceipt {
                migration_id: r.plan.migration_id.clone(),
                plan_sha256: digest(&r.plan),
                source: r.plan.source.clone(),
                aborted_at: 1004,
            },
            &p.source_key,
            "abort",
        )
        .unwrap(),
    );
    r.abort(1006).unwrap();
    assert_eq!(r.phase, Phase::Aborted);
}
#[test]
fn readiness_and_handoff_reject_crossed_manifests_peers_and_staging_identities() {
    let mut p = pair();
    ready(&mut p);
    let good = p.source.relinquish(&p.source_key, 1004).unwrap();
    let mut wrong = good.value.clone();
    wrong.destination.environment = "f".repeat(32);
    let wrong = Signed::sign(wrong, &p.source_key, "handoff").unwrap();
    assert!(p.target.accept(wrong, 1005).is_err());
    let mut wrong = good.value.clone();
    wrong.ready_sha256 = "0".repeat(64);
    let wrong = Signed::sign(wrong, &p.source_key, "handoff").unwrap();
    assert!(p.target.accept(wrong, 1005).is_err());
    let mut wrong = good.value.clone();
    wrong.committed_at = 1900;
    let wrong = Signed::sign(wrong, &p.source_key, "handoff").unwrap();
    assert!(p.target.accept(wrong, 5000).is_err());
    let mut crossed = p.target.clone();
    crossed
        .manifest
        .as_mut()
        .unwrap()
        .value
        .volumes
        .get_mut("root")
        .unwrap()
        .sha256 = "f".repeat(64);
    assert!(crossed.accept(good, 1005).is_err());
}
#[test]
fn no_source_relinquishment_without_signed_final_manifest_and_ready_target() {
    let mut p = pair();
    assert!(p.source.relinquish(&p.source_key, 1002).is_err());
    stopped(&mut p);
    assert!(p.source.relinquish(&p.source_key, 1003).is_err());
    let mut p = pair();
    ready(&mut p);
    let proof = p.source.relinquish(&p.source_key, 1004).unwrap();
    let mut target = p.target;
    target.ready = None;
    target.phase = Phase::Receiving;
    assert!(target.accept(proof, 1005).is_err());
}
#[test]
fn catalog_reopen_rejects_duplicate_local_holds_missing_acceptance_and_bad_scope() {
    let p = pair();
    let records = BTreeMap::from([(p.source.plan.migration_id.clone(), p.source.clone())]);
    validate_records(&records, &p.source.plan.source.environment).unwrap();
    assert!(validate_records(&records, &p.source.plan.destination.environment).is_err());
    let mut completed = p.source.clone();
    completed.phase = Phase::Complete;
    assert!(completed.validate().is_err());
    let mut expired = p.source.clone();
    expired.source_grant.value.expires_at = expired.source_grant.value.issued_at;
    assert!(expired.validate().is_err());
}
#[test]
fn every_recorded_transition_round_trips_before_next_ownership_action() {
    let mut p = pair();
    let mut states = vec![p.source.clone(), p.target.clone()];
    stopped(&mut p);
    states.extend([p.source.clone(), p.target.clone()]);
    let mut p = pair();
    ready(&mut p);
    states.extend([p.source.clone(), p.target.clone()]);
    let proof = p.source.relinquish(&p.source_key, 1004).unwrap();
    states.push(p.source.clone());
    p.target.accept(proof, 1005).unwrap();
    states.push(p.target.clone());
    for state in states {
        let bytes = serde_json::to_vec(&state).unwrap();
        let restored: TransferRecord = serde_json::from_slice(&bytes).unwrap();
        restored.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            serde_json::to_value(&restored).unwrap()
        );
    }
}

mod process {
    use super::*;
    use pve_dev::control::{
        host::{self, Host, HostConfig, SimulatedWorld},
        migration_transport::Service,
        protocol::{OperationStatus, Request, VmAction, VmState},
        store::{Binding, Store},
        transport::Network,
    };
    use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
    struct Node {
        _directory: tempfile::TempDir,
        store: Arc<Store>,
        host: Arc<Host>,
        admin: SecretKey,
        world: PathBuf,
        service: Arc<Service>,
        helper_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }
    impl Node {
        async fn new(environment: &str, owner: &str, create: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let admin = SecretKey::generate();
            let store = Arc::new(
                Store::create(
                    &directory.path().join("catalog.redb"),
                    Binding {
                        environment: environment.into(),
                        backend: "simulated".into(),
                        disk_capacity_mib: 8192,
                        min_memory_capacity_mib: 2048,
                        guest_images: Vec::new(),
                        physical_trial: None,
                        managed: None,
                    },
                    &admin.public().to_string(),
                )
                .unwrap(),
            );
            let world = directory.path().join("world.json");
            let host = Arc::new(
                Host::open(HostConfig::simulated(
                    directory.path().join("helper"),
                    environment,
                    world.clone(),
                ))
                .await
                .unwrap(),
            );
            let socket = directory.path().join("helper.sock");
            let helper = host.clone();
            let socket2 = socket.clone();
            let helper_task = tokio::spawn(async move { host::serve(helper, &socket2).await });
            for _ in 0..100 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if helper_task.is_finished() {
                panic!("helper stopped during startup: {:?}", helper_task.await);
            }
            assert!(socket.exists(), "helper socket never appeared");
            let service = Service::bind(
                store.clone(),
                SecretKey::generate(),
                socket,
                Network::Loopback,
                0,
            )
            .await
            .unwrap();
            let n = Self {
                _directory: directory,
                store,
                host,
                admin,
                world,
                service,
                helper_task,
            };
            n.flush().await;
            n.request(json!({"action":"create_user","request_id":"member","user_id":owner,"budget_mib":2048,"min_memory_budget_mib":512,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":2}));
            n.flush().await;
            if create {
                n.request(json!({"action":"create_vm","request_id":"source","owner":owner,"name":"dev-test","disk_mib":64,"memory_mib":256,"min_memory_mib":128,"cores":1}));
                n.flush().await;
            }
            n
        }
        fn request(&self, value: serde_json::Value) -> serde_json::Value {
            self.store
                .handle(
                    &self.admin.public().to_string(),
                    serde_json::from_value(value).unwrap(),
                )
                .unwrap()
        }
        async fn flush(&self) {
            for r in self.store.pending().unwrap() {
                self.host.submit(r.job.clone()).unwrap();
                self.host.run_pending().await.unwrap();
                let j = self.host.journal().unwrap();
                let progress = &j.jobs[&r.public.operation_id];
                assert_eq!(
                    progress.status,
                    OperationStatus::Verified,
                    "{:?}",
                    progress.error
                );
                self.store
                    .progress(
                        &r.public.operation_id,
                        progress.status.clone(),
                        &progress.step,
                        progress.error.clone(),
                    )
                    .unwrap();
            }
        }
        async fn admin(&self, request: AdminRequest) -> serde_json::Value {
            self.service
                .handle_admin(&self.admin.public().to_string(), request)
                .await
                .unwrap()
        }
    }
    impl Drop for Node {
        fn drop(&mut self) {
            self.helper_task.abort();
        }
    }
    async fn effect(
        node: &Node,
        request: HostMigrationRequest,
    ) -> anyhow::Result<serde_json::Value> {
        let response = node
            .host
            .handle(host::HostRequest::Migration {
                environment: node.store.read()?.binding.environment,
                request: Box::new(request),
            })
            .await?;
        anyhow::ensure!(
            response.status == OperationStatus::Verified,
            "{:?}",
            response.error
        );
        response
            .migration
            .ok_or_else(|| anyhow::anyhow!("missing migration reply"))
    }
    #[tokio::test]
    async fn committed_chunk_retry_checks_bytes_and_partial_copy_cancels_source_first() {
        let source = Node::new(&"a".repeat(32), "alice", true).await;
        let target = Node::new(&"b".repeat(32), "bob", false).await;
        let grant: Signed<Grant> = serde_json::from_value(
            source
                .admin(AdminRequest::Plan {
                    request_id: "partial-copy".into(),
                    vm_id: 9001,
                    destination: target.service.describe(),
                    target_owner: "bob".into(),
                    target_vm_id: 9002,
                    expires_in_seconds: 600,
                })
                .await,
        )
        .unwrap();
        let plan = grant.value.plan.clone();
        let id = plan.migration_id.clone();
        target
            .admin(AdminRequest::AuthorizeDestination {
                source_grant: grant,
                expires_in_seconds: 600,
            })
            .await;
        target.flush().await;
        effect(
            &source,
            HostMigrationRequest::Prepare {
                plan: plan.clone(),
                role: Role::Source,
                manifest: None,
            },
        )
        .await
        .unwrap();
        let manifest: Manifest = serde_json::from_value(
            effect(
                &source,
                HostMigrationRequest::Fence {
                    migration_id: id.clone(),
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        effect(
            &target,
            HostMigrationRequest::Prepare {
                plan,
                role: Role::Destination,
                manifest: Some(manifest.clone()),
            },
        )
        .await
        .unwrap();
        let chunk = effect(
            &source,
            HostMigrationRequest::ReadChunk {
                migration_id: id.clone(),
                slot: "root".into(),
                offset: 0,
            },
        )
        .await
        .unwrap();
        let request = || HostMigrationRequest::WriteChunk {
            migration_id: id.clone(),
            slot: "root".into(),
            offset: 0,
            bytes_base64: chunk["bytes_base64"].as_str().unwrap().into(),
            sha256: chunk["sha256"].as_str().unwrap().into(),
            zero_bytes: chunk["zero_bytes"].as_u64(),
        };
        let first = effect(&target, request()).await.unwrap();
        let replay = effect(&target, request()).await.unwrap();
        assert_eq!(first, replay);
        use sha2::{Digest, Sha256};
        let changed = vec![1u8; 128 * 1024];
        assert!(
            effect(
                &target,
                HostMigrationRequest::WriteChunk {
                    migration_id: id.clone(),
                    slot: "root".into(),
                    offset: 0,
                    bytes_base64: base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &changed
                    ),
                    sha256: format!("{:x}", Sha256::digest(&changed)),
                    zero_bytes: None
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            target.host.journal().unwrap().migrations[&id].offsets["root"],
            first["next_offset"].as_u64().unwrap()
        );
        assert!(
            effect(
                &target,
                HostMigrationRequest::Seal {
                    migration_id: id.clone(),
                    manifest
                }
            )
            .await
            .is_err(),
            "partial target must never become ready"
        );
        source
            .admin(AdminRequest::Abort {
                migration_id: id.clone(),
                confirm_cleanup: true,
                source_abort: None,
            })
            .await;
        assert!(
            source.store.read().unwrap().migrations[&id]
                .abort_proof
                .is_none()
        );
        source.service.work_once().await.unwrap();
        let proof = source.store.read().unwrap().migrations[&id]
            .abort_proof
            .clone()
            .expect("only restored source grants cancellation");
        let world = SimulatedWorld::new(source.world.clone()).load().unwrap();
        assert!(!world.vms[&9001].config.contains_key("lock"));
        assert_eq!(
            world.datasets["labpool/control/alice/vm-9001-disk-0"]["readonly"],
            "off"
        );
        target
            .admin(AdminRequest::Abort {
                migration_id: id.clone(),
                confirm_cleanup: true,
                source_abort: Some(proof),
            })
            .await;
        target.service.work_once().await.unwrap();
        assert_eq!(
            target.store.read().unwrap().migrations[&id].phase,
            Phase::Aborted
        );
        assert!(
            !SimulatedWorld::new(target.world.clone())
                .load()
                .unwrap()
                .vms
                .contains_key(&9002)
        );
    }
    #[tokio::test]
    async fn actual_iroh_peer_copy_owns_only_destination_and_requires_explicit_source_cleanup() {
        let source = Node::new(&"6".repeat(32), "alice", true).await;
        let target = Node::new(&"7".repeat(32), "bob", false).await;
        let sentinel = b"initialized guest bytes / unchanged source".to_vec();
        SimulatedWorld::new(source.world.clone())
            .edit(|w| {
                w.datasets
                    .get_mut("labpool/control/alice/vm-9001-disk-0")
                    .unwrap()
                    .insert(
                        "migration_data".into(),
                        base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &sentinel,
                        ),
                    );
                Ok(())
            })
            .unwrap();
        let grant: Signed<Grant> = serde_json::from_value(
            source
                .admin(AdminRequest::Plan {
                    request_id: "copy-one".into(),
                    vm_id: 9001,
                    destination: target.service.describe(),
                    target_owner: "bob".into(),
                    target_vm_id: 9002,
                    expires_in_seconds: 600,
                })
                .await,
        )
        .unwrap();
        let id = grant.value.plan.migration_id.clone();
        let retry = source
            .admin(AdminRequest::Plan {
                request_id: "copy-one".into(),
                vm_id: 9001,
                destination: target.service.describe(),
                target_owner: "bob".into(),
                target_vm_id: 9002,
                expires_in_seconds: 600,
            })
            .await;
        assert_eq!(serde_json::to_value(&grant).unwrap(), retry);
        assert!(
            target
                .service
                .handle_admin(
                    &source.admin.public().to_string(),
                    AdminRequest::AuthorizeDestination {
                        source_grant: grant.clone(),
                        expires_in_seconds: 600
                    }
                )
                .await
                .is_err()
        );
        let destination: Signed<Grant> = serde_json::from_value(
            target
                .admin(AdminRequest::AuthorizeDestination {
                    source_grant: grant,
                    expires_in_seconds: 600,
                })
                .await,
        )
        .unwrap();
        target.flush().await;
        assert!(
            target
                .store
                .handle(
                    &target.admin.public().to_string(),
                    Request::VmAction {
                        request_id: "early-start".into(),
                        vm_id: 9002,
                        kind: VmAction::Start
                    }
                )
                .is_err()
        );
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let service = target.service.clone();
        let task = tokio::spawn(async move {
            service
                .serve(async {
                    let _ = stop_rx.await;
                })
                .await
        });
        source
            .admin(AdminRequest::Start {
                migration_id: id.clone(),
                destination_grant: destination.clone(),
            })
            .await;
        for _ in 0..20 {
            source.service.work_once().await.unwrap();
            if source.store.read().unwrap().migrations[&id]
                .acceptance
                .is_some()
            {
                break;
            }
            target.service.work_once().await.unwrap();
        }
        let s = source.store.read().unwrap().migrations[&id].clone();
        let d = target.store.read().unwrap().migrations[&id].clone();
        assert!(s.acceptance.is_some(), "source error: {:?}", s.error);
        assert_eq!(s.phase, Phase::Relinquished);
        assert_eq!(d.phase, Phase::Complete);
        assert!(
            source
                .store
                .handle(
                    &source.admin.public().to_string(),
                    Request::VmAction {
                        request_id: "old-start".into(),
                        vm_id: 9001,
                        kind: VmAction::Start
                    }
                )
                .is_err()
        );
        assert!(
            source
                .service
                .handle_admin(
                    &source.admin.public().to_string(),
                    AdminRequest::Abort {
                        migration_id: id.clone(),
                        confirm_cleanup: true,
                        source_abort: None
                    }
                )
                .await
                .is_err()
        );
        let current = SimulatedWorld::new(source.world.clone()).load().unwrap();
        assert_eq!(current.vms[&9001].config["lock"], "migrate");
        assert!(!current.vms[&9001].running);
        target.request(
            json!({"action":"vm_action","request_id":"target-start","vm_id":9002,"kind":"start"}),
        );
        target.flush().await;
        assert_eq!(
            target.store.read().unwrap().vms[&9002].state,
            VmState::Running
        );
        target.request(
            json!({"action":"vm_action","request_id":"target-stop","vm_id":9002,"kind":"stop"}),
        );
        target.flush().await;
        let queued = source
            .admin(AdminRequest::Finish {
                migration_id: id.clone(),
                confirm_guest_verified: true,
                confirm_delete_recovery: true,
            })
            .await;
        assert_eq!(queued["phase"], "cleaning");
        source.service.work_once().await.unwrap();
        assert_eq!(
            source.store.read().unwrap().migrations[&id].phase,
            Phase::Complete
        );
        assert_eq!(
            source.store.read().unwrap().vms[&9001].state,
            VmState::Deleted
        );
        let before =
            serde_json::to_value(source.store.read().unwrap().migrations[&id].clone()).unwrap();
        assert_eq!(
            source
                .admin(AdminRequest::Finish {
                    migration_id: id,
                    confirm_guest_verified: true,
                    confirm_delete_recovery: true
                })
                .await,
            before
        );
        let _ = stop_tx.send(());
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn renewal_returns_exact_receipt_on_retry_and_destination_abort_requires_source_proof() {
        let source = Node::new(&"8".repeat(32), "alice", true).await;
        let target = Node::new(&"9".repeat(32), "bob", false).await;
        let grant: Signed<Grant> = serde_json::from_value(
            source
                .admin(AdminRequest::Plan {
                    request_id: "renewable".into(),
                    vm_id: 9001,
                    destination: target.service.describe(),
                    target_owner: "bob".into(),
                    target_vm_id: 9001,
                    expires_in_seconds: 60,
                })
                .await,
        )
        .unwrap();
        let id = grant.value.plan.migration_id.clone();
        let destination: Signed<Grant> = serde_json::from_value(
            target
                .admin(AdminRequest::AuthorizeDestination {
                    source_grant: grant,
                    expires_in_seconds: 60,
                })
                .await,
        )
        .unwrap();
        target.flush().await;
        let request = || AdminRequest::Renew {
            migration_id: id.clone(),
            request_id: "renew-once".into(),
            expires_in_seconds: 600,
            source_grant: None,
        };
        let first = source.admin(request()).await;
        let second = source.admin(request()).await;
        assert_eq!(first, second);
        let renewed: Signed<Grant> = serde_json::from_value(first).unwrap();
        assert_eq!(renewed.value.role, Role::Source);
        assert!(
            source
                .service
                .handle_admin(
                    &source.admin.public().to_string(),
                    AdminRequest::Start {
                        migration_id: id.clone(),
                        destination_grant: destination.clone()
                    }
                )
                .await
                .is_err(),
            "old destination consent must not authorize renewed source approval"
        );
        let target_renew = || AdminRequest::Renew {
            migration_id: id.clone(),
            request_id: "target-renew-once".into(),
            expires_in_seconds: 600,
            source_grant: Some(renewed.clone()),
        };
        let a = target.admin(target_renew()).await;
        let b = target.admin(target_renew()).await;
        assert_eq!(a, b);
        assert!(
            target
                .service
                .handle_admin(
                    &target.admin.public().to_string(),
                    AdminRequest::Abort {
                        migration_id: id.clone(),
                        confirm_cleanup: true,
                        source_abort: None
                    }
                )
                .await
                .is_err()
        );
        let pending: TransferRecord = serde_json::from_value(
            source
                .admin(AdminRequest::Abort {
                    migration_id: id.clone(),
                    confirm_cleanup: true,
                    source_abort: None,
                })
                .await,
        )
        .unwrap();
        assert_eq!(pending.phase, Phase::Cleaning);
        assert!(pending.abort_proof.is_none());
        source.service.work_once().await.unwrap();
        let canceled = source.store.read().unwrap().migrations[&id].clone();
        assert!(canceled.abort_proof.is_some());
        let target_pending: TransferRecord = serde_json::from_value(
            target
                .admin(AdminRequest::Abort {
                    migration_id: id.clone(),
                    confirm_cleanup: true,
                    source_abort: canceled.abort_proof.clone(),
                })
                .await,
        )
        .unwrap();
        assert_eq!(target_pending.phase, Phase::Cleaning);
        assert_eq!(target_pending.abort_proof, canceled.abort_proof);
        target.service.work_once().await.unwrap();
        let target_canceled = target.store.read().unwrap().migrations[&id].clone();
        assert_eq!(target_canceled.phase, Phase::Aborted);
        assert_eq!(
            target.store.read().unwrap().vms[&9001].state,
            VmState::Deleted
        );
        assert!(destination.validate_grant(0, false).is_err());
    }
}
