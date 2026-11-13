//! The controller catalog's per-entity table layout.

use iroh::SecretKey;
use pve_dev::control::{
    catalog_tables::{self, AUDIT, DEVICES, META, USERS},
    protocol::*,
    store::{Binding, Store},
};
use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableDefinition};
use tempfile::TempDir;

fn binding() -> Binding {
    Binding {
        environment: "0123456789abcdef0123456789abcdef".into(),
        backend: "simulated".into(),
        disk_capacity_mib: 2048,
        min_memory_capacity_mib: 1024,
        guest_images: Vec::new(),
        physical_trial: None,
        managed: None,
    }
}

fn setup() -> (TempDir, Store, String) {
    let directory = tempfile::tempdir().unwrap();
    let admin = SecretKey::generate().public().to_string();
    let store = Store::create(&directory.path().join("state.redb"), binding(), &admin).unwrap();
    (directory, store, admin)
}

fn raw<T>(directory: &TempDir, read: impl FnOnce(&redb::ReadTransaction) -> T) -> T {
    let database = Database::open(directory.path().join("state.redb")).unwrap();
    read(&database.begin_read().unwrap())
}

#[test]
fn catalog_round_trips_through_tables_and_rows_match_entities() {
    let (directory, store, admin) = setup();
    let peer = SecretKey::generate().public().to_string();
    store
        .handle(
            &admin,
            Request::EnrollDevice {
                user_id: "erin".into(),
                endpoint_id: peer.clone(),
                label: "laptop".into(),
            },
        )
        .unwrap();
    let catalog = store.read().unwrap();
    assert_eq!(catalog.devices.len(), 2);
    drop(store);
    let reopened = Store::open(&directory.path().join("state.redb"), &binding()).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.read().unwrap()).unwrap(),
        serde_json::to_value(&catalog).unwrap()
    );
    drop(reopened);
    raw(&directory, |tx| {
        let devices = tx.open_table(DEVICES).unwrap();
        assert_eq!(devices.len().unwrap(), 2);
        let row: pve_dev::control::store::Device =
            serde_json::from_slice(devices.get(peer.as_str()).unwrap().unwrap().value()).unwrap();
        assert_eq!(row.label, "laptop");
        assert_eq!(tx.open_table(USERS).unwrap().len().unwrap(), 1);
        assert_eq!(
            tx.open_table(AUDIT).unwrap().len().unwrap() as usize,
            catalog.audit.len()
        );
    });
}

#[test]
fn read_only_requests_write_nothing_and_keep_the_revision() {
    let (directory, store, admin) = setup();
    let before = store.read().unwrap();
    for request in [
        Request::Whoami {},
        Request::Dashboard {},
        Request::Budget {},
        Request::ListVms {},
        Request::ListOperations {},
    ] {
        store.handle(&admin, request).unwrap();
    }
    // A rejected mutation also leaves no trace.
    assert!(
        store
            .handle(
                &admin,
                Request::RevokeDevice {
                    endpoint_id: SecretKey::generate().public().to_string(),
                },
            )
            .is_err()
    );
    let after = store.read().unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    drop(store);
    let revision = raw(&directory, |tx| {
        let meta = tx.open_table(META).unwrap();
        let row = meta.get("revision").unwrap().unwrap();
        serde_json::from_slice::<u64>(row.value()).unwrap()
    });
    assert_eq!(revision, before.revision);
}

#[test]
fn removed_entities_and_audit_entries_lose_their_rows() {
    let (directory, store, admin) = setup();
    let peer = SecretKey::generate().public().to_string();
    store
        .handle(
            &admin,
            Request::EnrollDevice {
                user_id: "erin".into(),
                endpoint_id: peer.clone(),
                label: "laptop".into(),
            },
        )
        .unwrap();
    let revision = store.read().unwrap().revision;
    store
        .change(|catalog| {
            catalog.devices.remove(&peer);
            catalog.audit.truncate(1);
            Ok(())
        })
        .unwrap();
    let catalog = store.read().unwrap();
    assert_eq!(catalog.revision, revision + 1);
    assert!(!catalog.devices.contains_key(&peer));
    assert_eq!(catalog.audit.len(), 1);
    drop(store);
    raw(&directory, |tx| {
        assert!(
            tx.open_table(DEVICES)
                .unwrap()
                .get(peer.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(tx.open_table(AUDIT).unwrap().len().unwrap(), 1);
    });
}

#[test]
fn retired_and_foreign_layouts_are_refused() {
    for (name, expected) in [
        ("controller_catalog_v1", "retired single-record layout"),
        ("something_else", "foreign table something_else"),
    ] {
        let (directory, store, _) = setup();
        drop(store);
        let path = directory.path().join("state.redb");
        {
            let database = Database::open(&path).unwrap();
            let tx = database.begin_write().unwrap();
            tx.open_table(TableDefinition::<u8, &[u8]>::new(name))
                .unwrap();
            tx.commit().unwrap();
        }
        let error = Store::open(&path, &binding()).err().unwrap();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[test]
fn missing_meta_and_audit_gaps_are_refused() {
    let (directory, store, admin) = setup();
    store
        .handle(
            &admin,
            Request::EnrollDevice {
                user_id: "erin".into(),
                endpoint_id: SecretKey::generate().public().to_string(),
                label: "laptop".into(),
            },
        )
        .unwrap();
    assert!(store.read().unwrap().audit.len() >= 2);
    drop(store);
    let path = directory.path().join("state.redb");
    {
        let database = Database::open(&path).unwrap();
        let tx = database.begin_write().unwrap();
        tx.open_table(AUDIT).unwrap().remove(0).unwrap();
        tx.commit().unwrap();
    }
    let error = Store::open(&path, &binding()).err().unwrap();
    assert!(
        format!("{error:#}").contains("audit log has a gap"),
        "{error:#}"
    );
    {
        let database = Database::open(&path).unwrap();
        let tx = database.begin_write().unwrap();
        tx.open_table(META).unwrap().remove("binding").unwrap();
        tx.commit().unwrap();
    }
    assert!(Store::open(&path, &binding()).is_err());
    assert_eq!(catalog_tables::TABLE_NAMES.len(), 12);
}
