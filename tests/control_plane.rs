use std::{path::Path, sync::Arc, time::Duration};

use iroh::SecretKey;
use pve_dev::control::{
    protocol::*,
    store::{Binding, Store},
    transport::{self, Descriptor, Network, Profile},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const ENVIRONMENT: &str = "0123456789abcdef0123456789abcdef";

fn binding() -> Binding {
    Binding {
        environment: ENVIRONMENT.into(),
        backend: "simulated".into(),
        disk_capacity_mib: 2048,
        min_memory_capacity_mib: 1024,
        guest_images: Vec::new(),
        physical_trial: None,
        managed: None,
    }
}

fn setup() -> (TempDir, Arc<Store>, String) {
    let directory = tempfile::tempdir().unwrap();
    let admin = SecretKey::generate().public().to_string();
    let store =
        Arc::new(Store::create(&directory.path().join("state.redb"), binding(), &admin).unwrap());
    ready(&store);
    (directory, store, admin)
}

fn ready(store: &Store) {
    for record in store.pending().unwrap() {
        store
            .progress(
                &record.public.operation_id,
                OperationStatus::Verified,
                "test_observed",
                None,
            )
            .unwrap();
    }
}

fn person(store: &Store, admin: &str, name: &str) -> String {
    store
        .handle(
            admin,
            Request::CreateUser {
                request_id: format!("new-{name}"),
                user_id: name.into(),
                budget_mib: 128,
                min_memory_budget_mib: 256,
                max_memory_budget_mib: 1024,
                max_cores: 8,
                max_vms: 4,
            },
        )
        .unwrap();
    ready(store);
    let peer = SecretKey::generate().public().to_string();
    store
        .handle(
            admin,
            Request::EnrollDevice {
                user_id: name.into(),
                endpoint_id: peer.clone(),
                label: format!("{name} test device"),
            },
        )
        .unwrap();
    peer
}

fn create(request_id: &str, name: &str, owner: Option<&str>) -> Request {
    Request::CreateVm {
        request_id: request_id.into(),
        name: name.into(),
        disk_mib: 32,
        memory_mib: 128,
        min_memory_mib: 128,
        cores: 1,
        owner: owner.map(str::to_owned),
        guest: None,
    }
}

#[test]
fn authorization_is_derived_from_authenticated_device_not_request_owner() {
    let (_directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    let heidi = person(&store, &admin, "heidi");
    let foreign = store
        .handle(&heidi, create("heidi-one", "dev-heidi", None))
        .unwrap();
    let vm_id = foreign["vm_id"].as_u64().unwrap() as u16;
    let unknown = SecretKey::generate().public().to_string();
    assert_eq!(
        store
            .handle(&unknown, Request::Dashboard {})
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        store
            .handle(&frank, create("impersonation", "not-mine", Some("heidi")))
            .unwrap_err()
            .code,
        "forbidden"
    );
    for kind in [VmAction::Start, VmAction::Stop, VmAction::Delete] {
        assert_eq!(
            store
                .handle(
                    &frank,
                    Request::VmAction {
                        request_id: "foreign".into(),
                        vm_id,
                        kind
                    }
                )
                .unwrap_err()
                .code,
            "forbidden"
        );
    }
    assert_eq!(
        store
            .handle(
                &frank,
                Request::GetOperation {
                    operation_id: foreign["operation_id"].as_str().unwrap().into()
                }
            )
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        store
            .handle(
                &frank,
                Request::EnrollDevice {
                    user_id: "heidi".into(),
                    endpoint_id: unknown.clone(),
                    label: "attacker".into()
                }
            )
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        store
            .handle(
                &frank,
                Request::RevokeDevice {
                    endpoint_id: admin.clone()
                }
            )
            .unwrap_err()
            .code,
        "forbidden"
    );
    let dashboard = store.handle(&frank, Request::Dashboard {}).unwrap();
    assert!(dashboard["vms"].as_array().unwrap().is_empty());
    assert!(
        dashboard["operations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|operation| operation["owner"] == "frank")
    );
    assert_eq!(dashboard["users"].as_array().unwrap().len(), 1);
    assert_eq!(
        store.handle(&frank, Request::Whoami {}).unwrap()["user_id"],
        "frank"
    );
}

#[test]
fn concurrent_pending_allocations_cannot_exceed_shared_owner_budget() {
    let (_directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    let threads: Vec<_> = (0..12)
        .map(|index| {
            let store = store.clone();
            let peer = frank.clone();
            std::thread::spawn(move || {
                store.handle(
                    &peer,
                    create(&format!("request-{index}"), &format!("vm-{index}"), None),
                )
            })
        })
        .collect();
    let accepted = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(Result::is_ok)
        .count();
    assert_eq!(accepted, 2);
    let usage = store.handle(&frank, Request::Budget {}).unwrap();
    assert_eq!(usage["allocated_mib"], 88);
    assert_eq!(usage["min_memory_allocated_mib"], 256);
    assert_eq!(usage["remaining_mib"], 40);
}

#[test]
fn durable_retries_are_scoped_to_actor_and_reject_changed_payloads() {
    let (directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    let heidi = person(&store, &admin, "heidi");
    let first = store
        .handle(&frank, create("stable", "dev-frank", None))
        .unwrap();
    assert_eq!(
        store
            .handle(&frank, create("stable", "dev-frank", None))
            .unwrap(),
        first
    );
    assert_eq!(
        store
            .handle(&frank, create("stable", "changed", None))
            .unwrap_err()
            .code,
        "conflict"
    );
    let other = store
        .handle(&heidi, create("stable", "dev-heidi", None))
        .unwrap();
    assert_ne!(first["operation_id"], other["operation_id"]);
    drop(store);
    let store = Store::open(&directory.path().join("state.redb"), &binding()).unwrap();
    assert_eq!(
        store
            .handle(&frank, create("stable", "dev-frank", None))
            .unwrap(),
        first
    );
    let mut changed = binding();
    changed.environment = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
    drop(store);
    assert!(Store::open(&directory.path().join("state.redb"), &changed).is_err());
}

#[test]
fn deletion_retains_capacity_until_verified_and_vm_ids_are_never_reused() {
    let (_directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    let created = store
        .handle(&frank, create("first", "dev-frank", None))
        .unwrap();
    ready(&store);
    let vm_id = created["vm_id"].as_u64().unwrap() as u16;
    let deleted = store
        .handle(
            &frank,
            Request::VmAction {
                request_id: "delete".into(),
                vm_id,
                kind: VmAction::Delete,
            },
        )
        .unwrap();
    assert_eq!(
        store.handle(&frank, Request::Budget {}).unwrap()["allocated_mib"],
        44
    );
    assert!(
        store
            .handle(
                &frank,
                Request::VmAction {
                    request_id: "racing-start".into(),
                    vm_id,
                    kind: VmAction::Start
                }
            )
            .is_err()
    );
    store
        .progress(
            deleted["operation_id"].as_str().unwrap(),
            OperationStatus::Verified,
            "absence_verified",
            None,
        )
        .unwrap();
    assert_eq!(
        store.handle(&frank, Request::Budget {}).unwrap()["allocated_mib"],
        0
    );
    let next = store
        .handle(&frank, create("second", "dev-frank", None))
        .unwrap();
    assert_ne!(next["vm_id"], created["vm_id"]);
    assert_eq!(
        store
            .handle(&frank, create("first", "dev-frank", None))
            .unwrap(),
        store
            .handle(
                &frank,
                Request::GetOperation {
                    operation_id: created["operation_id"].as_str().unwrap().into()
                }
            )
            .unwrap()
    );
}

#[test]
fn blocked_uncertain_operations_keep_claims_and_prevent_conflicting_actions() {
    let (_directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    let created = store
        .handle(&frank, create("uncertain", "dev-frank", None))
        .unwrap();
    store
        .progress(
            created["operation_id"].as_str().unwrap(),
            OperationStatus::Blocked,
            "vm",
            Some("ambiguous external action; investigate".into()),
        )
        .unwrap();
    assert_eq!(
        store.handle(&frank, Request::Budget {}).unwrap()["allocated_mib"],
        44
    );
    assert!(
        store
            .handle(
                &frank,
                Request::VmAction {
                    request_id: "delete-unknown".into(),
                    vm_id: created["vm_id"].as_u64().unwrap() as u16,
                    kind: VmAction::Delete
                }
            )
            .is_err()
    );
    assert_eq!(
        store
            .handle(&frank, create("uncertain", "dev-frank", None))
            .unwrap()["status"],
        "blocked"
    );
}

#[test]
fn device_revocation_is_immediate_and_last_administrator_is_protected() {
    let (_directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    assert!(store.authorized(&frank));
    store
        .handle(
            &admin,
            Request::RevokeDevice {
                endpoint_id: frank.clone(),
            },
        )
        .unwrap();
    assert!(!store.authorized(&frank));
    assert!(store.handle(&frank, Request::Dashboard {}).is_err());
    assert!(
        store
            .handle(
                &admin,
                Request::EnrollDevice {
                    user_id: "frank".into(),
                    endpoint_id: frank,
                    label: "reuse revoked key".into()
                }
            )
            .is_err()
    );
    assert!(
        store
            .handle(
                &admin,
                Request::RevokeDevice {
                    endpoint_id: admin.clone()
                }
            )
            .is_err()
    );
}

#[test]
fn wire_schema_rejects_unknown_fields_and_admin_budget_changes_by_members() {
    assert!(!pve_dev::control::store::valid_name("bad-"));
    assert!(!pve_dev::control::store::valid_name(""));
    assert!(pve_dev::control::store::valid_name("dev-frank1"));
    assert!(serde_json::from_value::<Request>(json!({"action":"create_vm","request_id":"one","name":"one","disk_mib":32,"memory_mib":128,"min_memory_mib":128,"cores":1,"owner":null,"shell":"danger"})).is_err());
    assert!(
        serde_json::from_value::<Request>(json!({"action":"dashboard","owner":"heidi"})).is_err()
    );
    let (_directory, store, admin) = setup();
    let frank = person(&store, &admin, "frank");
    let request = Request::CreateUser {
        request_id: "raise-budget".into(),
        user_id: "other".into(),
        budget_mib: 512,
        min_memory_budget_mib: 256,
        max_memory_budget_mib: 1024,
        max_cores: 8,
        max_vms: 4,
    };
    assert_eq!(store.handle(&frank, request).unwrap_err().code, "forbidden");
}

fn profile(directory: &Path, name: &str, server: &Descriptor) -> Profile {
    let identity = directory.join(name);
    transport::generate_identity(&identity).unwrap();
    Profile {
        schema: 1,
        identity,
        server: server.clone(),
    }
}

#[tokio::test]
async fn real_iroh_transport_authenticates_devices_and_pins_environment() {
    let directory = tempfile::tempdir().unwrap();
    let key = SecretKey::generate();
    let endpoint = transport::endpoint(key, Network::Loopback, 0, true)
        .await
        .unwrap();
    let descriptor = Descriptor {
        schema: 1,
        endpoint: endpoint.addr(),
        environment: ENVIRONMENT.into(),
        network: Network::Loopback,
    };
    let admin_profile = profile(directory.path(), "admin.key", &descriptor);
    let admin = transport::load_identity(&admin_profile.identity)
        .unwrap()
        .public()
        .to_string();
    let store =
        Arc::new(Store::create(&directory.path().join("state.redb"), binding(), &admin).unwrap());
    ready(&store);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(transport::serve(endpoint, store.clone(), async {
        let _ = stopped.await;
    }));
    let response = transport::rpc(&admin_profile, Request::Whoami {})
        .await
        .unwrap();
    assert!(response.ok);
    assert_eq!(response.result.unwrap()["user_id"], "erin");
    let unknown = profile(directory.path(), "unknown.key", &descriptor);
    assert_eq!(
        transport::rpc(&unknown, Request::Dashboard {})
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        "forbidden"
    );
    let frank = person(&store, &admin, "frank");
    let enrolled = profile(directory.path(), "enrolled.key", &descriptor);
    let enrolled_peer = transport::load_identity(&enrolled.identity)
        .unwrap()
        .public()
        .to_string();
    store
        .handle(
            &admin,
            Request::EnrollDevice {
                user_id: "frank".into(),
                endpoint_id: enrolled_peer.clone(),
                label: "wire test".into(),
            },
        )
        .unwrap();
    assert_eq!(
        transport::rpc(&enrolled, Request::Whoami {})
            .await
            .unwrap()
            .result
            .unwrap()["user_id"],
        "frank"
    );
    let mut wrong_environment = enrolled.clone();
    wrong_environment.server.environment = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
    assert_eq!(
        transport::rpc(&wrong_environment, Request::Dashboard {})
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        "forbidden"
    );
    store
        .handle(
            &admin,
            Request::RevokeDevice {
                endpoint_id: enrolled_peer,
            },
        )
        .unwrap();
    assert!(
        !transport::rpc(&enrolled, Request::Dashboard {})
            .await
            .unwrap()
            .ok
    );
    assert!(store.authorized(&frank));
    let mut wrong_server = admin_profile;
    wrong_server.server.endpoint.id = SecretKey::generate().public();
    let wrong_identity = tokio::time::timeout(
        Duration::from_secs(3),
        transport::rpc(&wrong_server, Request::Dashboard {}),
    )
    .await;
    assert!(!matches!(wrong_identity, Ok(Ok(_))));
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[test]
fn controller_never_recreates_a_lost_database_on_open() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.redb");
    assert!(Store::open(&missing, &binding()).is_err());
    assert!(!missing.exists());
    let input: Value = json!({"action":"dashboard"});
    assert!(serde_json::from_value::<Request>(input).is_ok());
}
