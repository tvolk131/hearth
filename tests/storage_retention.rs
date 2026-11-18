//! Split storage, format versioning, pagination and retention.

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::bail;
use hearth::control::{
    host::{Host, HostConfig, HostRequest},
    maintenance::{self, Pins, PruneStage},
    persist,
    protocol::{OperationStatus, Request, VmAction},
    retention,
    store::{Binding, Store},
    transport::{self, Network, ServerConfig},
};
use iroh::SecretKey;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle};
use serde_json::{Value, json};
use tempfile::TempDir;

const ENV: &str = "0123456789abcdef0123456789abcdef";
const DAY: u64 = 86_400;

fn binding() -> Binding {
    Binding {
        environment: ENV.into(),
        backend: "simulated".into(),
        disk_capacity_mib: 2048,
        min_memory_capacity_mib: 1024,
        guest_images: Vec::new(),
        physical_trial: None,
        managed: None,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn tables(path: &Path) -> BTreeSet<String> {
    let db = Database::open(path).unwrap();
    let tx = db.begin_read().unwrap();
    tx.list_tables()
        .unwrap()
        .map(|t| t.name().to_owned())
        .collect()
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

/// A controller alone, with every bootstrap operation verified.
fn controller() -> (TempDir, Store, String) {
    let directory = tempfile::tempdir().unwrap();
    let admin = SecretKey::generate().public().to_string();
    let store = Store::create(&directory.path().join("state.redb"), binding(), &admin).unwrap();
    ready(&store);
    (directory, store, admin)
}

fn create_vm(request_id: &str, name: &str) -> Request {
    Request::CreateVm {
        request_id: request_id.into(),
        name: name.into(),
        disk_mib: 32,
        memory_mib: 128,
        min_memory_mib: 128,
        cores: 1,
        owner: None,
        guest: None,
    }
}

fn vm_action(request_id: &str, vm_id: u16, kind: VmAction) -> Request {
    Request::VmAction {
        request_id: request_id.into(),
        vm_id,
        kind,
        retire_storage: false,
    }
}

fn operation_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["operation_id"].as_str().unwrap().to_owned())
        .collect()
}

fn list(cursor: Option<&str>, limit: Option<u32>) -> Request {
    Request::ListOperations {
        cursor: cursor.map(str::to_owned),
        limit,
    }
}

/// Starts and stops a fresh VM until there are at least `count` operations.
fn history(store: &Store, admin: &str, count: usize) -> u16 {
    let accepted = store
        .handle(admin, create_vm("history-vm", "history"))
        .unwrap();
    ready(store);
    let vm_id = accepted["vm_id"].as_u64().map_or_else(
        || store.read().unwrap().vms.keys().copied().next().unwrap(),
        |id| id as u16,
    );
    let mut index = 0;
    while store.read().unwrap().operations.len() < count {
        let kind = if index % 2 == 0 {
            VmAction::Start
        } else {
            VmAction::Stop
        };
        store
            .handle(admin, vm_action(&format!("power-{index}"), vm_id, kind))
            .unwrap();
        ready(store);
        index += 1;
    }
    vm_id
}

#[test]
fn operation_listing_pages_by_cursor_and_keeps_the_unpaged_default() {
    let (_directory, store, admin) = controller();
    history(&store, &admin, 9);
    let legacy = store.handle(&admin, list(None, None)).unwrap();
    let everything = operation_ids(&legacy);
    assert!(everything.len() >= 9);

    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = store
            .handle(&admin, list(cursor.as_deref(), Some(2)))
            .unwrap();
        let ids = operation_ids(&page["operations"]);
        assert!(ids.len() <= 2);
        paged.extend(ids);
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(paged, everything);
    assert_eq!(pages, everything.len().div_ceil(2));

    // A cursor past the end yields an empty final page.
    let last = everything.last().unwrap();
    let tail = store.handle(&admin, list(Some(last), Some(5))).unwrap();
    assert_eq!(tail["operations"], json!([]));
    assert!(tail.get("next_cursor").is_none_or(Value::is_null));

    // A cursor alone pages with the default size.
    let rest = store
        .handle(&admin, list(Some(&everything[0]), None))
        .unwrap();
    assert_eq!(operation_ids(&rest["operations"]), everything[1..]);

    for bad in [0, 1001] {
        let error = store.handle(&admin, list(None, Some(bad))).unwrap_err();
        assert_eq!(error.code, "invalid");
    }

    // The wire default stays the bare action an older client sends.
    let request: Request = serde_json::from_value(json!({"action": "list_operations"})).unwrap();
    assert_eq!(store.handle(&admin, request).unwrap(), legacy);
    assert_eq!(
        serde_json::to_value(list(None, None)).unwrap(),
        json!({"action": "list_operations"})
    );
}

#[test]
fn audit_listing_is_admin_only_and_pages_in_sequence_order() {
    let (_directory, store, admin) = controller();
    let device = SecretKey::generate().public().to_string();
    let admin_user = store.read().unwrap().devices[&admin].user_id.clone();
    for label in ["one", "two", "three"] {
        let endpoint = SecretKey::generate().public().to_string();
        store
            .handle(
                &admin,
                Request::EnrollDevice {
                    user_id: admin_user.clone(),
                    endpoint_id: endpoint.clone(),
                    label: label.into(),
                },
            )
            .unwrap();
        store
            .handle(
                &admin,
                Request::RevokeDevice {
                    endpoint_id: endpoint,
                },
            )
            .unwrap();
    }
    let (all, more) = store.audit_page(None, 1000).unwrap();
    assert!(more.is_none());
    assert!(all.len() >= 6, "{all:?}");

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .handle(
                &admin,
                Request::ListAudit {
                    cursor,
                    limit: Some(4),
                },
            )
            .unwrap();
        for entry in page["entries"].as_array().unwrap() {
            seen.push((entry["seq"].as_u64().unwrap(), entry["entry"].clone()));
        }
        match page["next_cursor"].as_u64() {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, all);
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0));

    // Non-admin users cannot read the audit log.
    store
        .handle(
            &admin,
            Request::CreateUser {
                request_id: "new-frank".into(),
                user_id: "frank".into(),
                budget_mib: 128,
                min_memory_budget_mib: 256,
                max_memory_budget_mib: 1024,
                max_cores: 8,
                max_vms: 4,
            },
        )
        .unwrap();
    ready(&store);
    store
        .handle(
            &admin,
            Request::EnrollDevice {
                user_id: "frank".into(),
                endpoint_id: device.clone(),
                label: "frank".into(),
            },
        )
        .unwrap();
    let error = store
        .handle(
            &device,
            Request::ListAudit {
                cursor: None,
                limit: None,
            },
        )
        .unwrap_err();
    assert_eq!(error.code, "forbidden");
}

#[test]
fn dashboard_includes_all_unfinished_work_and_bounded_history() {
    let (_directory, store, admin) = controller();
    let bound = hearth::control::store::DASHBOARD_TERMINAL_OPERATIONS;
    let vm_id = history(&store, &admin, bound + 5);
    // One unfinished operation must always be shown.
    store
        .handle(&admin, vm_action("unfinished", vm_id, VmAction::Start))
        .unwrap();
    let total = store.read().unwrap().operations.len();
    let dashboard = store.handle(&admin, Request::Dashboard {}).unwrap();
    let shown = dashboard["operations"].as_array().unwrap();
    assert_eq!(shown.len(), bound + 1);
    assert_eq!(
        dashboard["omitted_operations"].as_u64().unwrap() as usize,
        total - shown.len()
    );
    assert!(shown.iter().any(|o| o["request_id"] == "unfinished"));
}

// ---------------------------------------------------------------------------
// Retired format 1
// ---------------------------------------------------------------------------

const LEGACY_CATALOG: TableDefinition<u8, &[u8]> = TableDefinition::new("controller_catalog_v1");
const LEGACY_JOURNAL: TableDefinition<u8, &[u8]> = TableDefinition::new("host_journal_v1");
const META: TableDefinition<&str, u64> = TableDefinition::new("hearth_meta");

/// Writes `value` as a format-1 binary stored it: one JSON row, no marker.
fn write_format1(path: &Path, table: TableDefinition<u8, &[u8]>, value: &Value) {
    let db = Database::create(path).unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(table)
        .unwrap()
        .insert(0, serde_json::to_vec(value).unwrap().as_slice())
        .unwrap();
    tx.commit().unwrap();
}

fn assert_format1_refusal(error: anyhow::Error) {
    let message = format!("{error:#}");
    assert!(
        message.contains("retired storage format 1") && message.contains("the last build"),
        "{message}"
    );
}

#[test]
fn format1_controller_is_refused_and_left_untouched() {
    let (directory, store, _) = controller();
    let catalog = serde_json::to_value(store.read_with_audit().unwrap()).unwrap();
    drop(store);
    let path = directory.path().join("legacy.redb");
    write_format1(&path, LEGACY_CATALOG, &catalog);
    assert_format1_refusal(Store::open(&path, &binding()).err().unwrap());
    let db = Database::open(&path).unwrap();
    assert_format1_refusal(persist::read_catalog(&db, true).unwrap_err());
    assert_format1_refusal(persist::verify_catalog(&db).unwrap_err());
    drop(db);
    assert_eq!(
        tables(&path),
        BTreeSet::from(["controller_catalog_v1".into()])
    );
}

#[tokio::test]
async fn format1_helper_journal_is_refused_and_left_untouched() {
    let f = Pair::new().await;
    let journal = {
        let host = f.host().await;
        serde_json::to_value(host.journal().unwrap()).unwrap()
    };
    let path = f.helper.join("host-journal.redb");
    let permissions = fs::metadata(&path).unwrap().permissions();
    fs::remove_file(&path).unwrap();
    write_format1(&path, LEGACY_JOURNAL, &journal);
    fs::set_permissions(&path, permissions).unwrap();
    assert_format1_refusal(Host::open(f.host_config()).await.err().unwrap());
    assert_eq!(tables(&path), BTreeSet::from(["host_journal_v1".into()]));
}

// ---------------------------------------------------------------------------
// Refusing newer state
// ---------------------------------------------------------------------------

fn set_format(path: &Path, format: u64) {
    let db = Database::open(path).unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(META)
        .unwrap()
        .insert("format", format)
        .unwrap();
    tx.commit().unwrap();
}

fn edit_core(path: &Path, name: &str, edit: impl FnOnce(&mut Value)) {
    let db = Database::open(path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<u8, &[u8]>::new(name))
            .unwrap();
        let mut value: Value =
            serde_json::from_slice(table.get(0).unwrap().unwrap().value()).unwrap();
        edit(&mut value);
        table
            .insert(0, serde_json::to_vec(&value).unwrap().as_slice())
            .unwrap();
    }
    tx.commit().unwrap();
}

fn edit_first_row(path: &Path, name: &str, edit: impl FnOnce(&mut Value)) {
    let db = Database::open(path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &[u8]>::new(name))
            .unwrap();
        let (key, mut value) = {
            let first = table.first().unwrap().unwrap();
            let value: Value = serde_json::from_slice(first.1.value()).unwrap();
            (first.0.value().to_owned(), value)
        };
        edit(&mut value);
        table
            .insert(key.as_str(), serde_json::to_vec(&value).unwrap().as_slice())
            .unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn controller_refuses_a_newer_format_and_unknown_fields() {
    let (directory, store, _) = controller();
    drop(store);
    let path = directory.path().join("state.redb");
    let pristine = fs::read(&path).unwrap();

    set_format(&path, 3);
    let error = Store::open(&path, &binding()).err().unwrap();
    assert!(format!("{error:#}").contains("newer Hearth"), "{error:#}");
    assert!(persist::read_catalog(&Database::open(&path).unwrap(), false).is_err());

    for name in ["core", "operation"] {
        fs::write(&path, &pristine).unwrap();
        if name == "core" {
            edit_core(&path, "controller_core_v2", |v| {
                v["introduced_later"] = json!({"x": 1})
            });
        } else {
            edit_first_row(&path, "controller_operations_v2", |v| {
                v["public"]["introduced_later"] = json!(true)
            });
        }
        let error = Store::open(&path, &binding()).err().unwrap();
        assert!(
            format!("{error:#}").contains("introduced_later"),
            "{name}: {error:#}"
        );
    }
    // A missing marker on split tables is refused rather than guessed.
    fs::write(&path, &pristine).unwrap();
    {
        let db = Database::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        tx.delete_table(META).unwrap();
        tx.commit().unwrap();
    }
    assert!(Store::open(&path, &binding()).is_err());
    fs::write(&path, &pristine).unwrap();
    Store::open(&path, &binding()).unwrap();
}

#[tokio::test]
async fn helper_refuses_a_newer_format_and_unknown_fields() {
    let f = Pair::new().await;
    let path = f.helper.join("host-journal.redb");
    let pristine = fs::read(&path).unwrap();
    set_format(&path, 3);
    let error = Host::open(f.host_config()).await.err().unwrap();
    assert!(format!("{error:#}").contains("newer Hearth"), "{error:#}");

    fs::write(&path, &pristine).unwrap();
    edit_first_row(&path, "host_jobs_v2", |v| v["introduced_later"] = json!(1));
    let error = Host::open(f.host_config()).await.err().unwrap();
    assert!(
        format!("{error:#}").contains("introduced_later"),
        "{error:#}"
    );

    fs::write(&path, &pristine).unwrap();
    f.host().await;
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// A stopped controller/helper pair on the simulated backend.
struct Pair {
    _root: TempDir,
    root: PathBuf,
    controller: PathBuf,
    helper: PathBuf,
    world: PathBuf,
    pins: Pins,
    admin: String,
}

impl Pair {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = root.join("controller");
        let helper = root.join("helper");
        let world = root.join("world.json");
        let admin = SecretKey::generate().public().to_string();
        let config = ServerConfig {
            schema: 1,
            binding: binding(),
            host_socket: root.join("host.sock"),
            network: Network::Loopback,
            port: 0,
            migration_network: None,
        };
        transport::initialize(&controller, &config, &admin).unwrap();
        let pair = Self {
            _root: temp,
            pins: Pins {
                environment: ENV.into(),
                server_id: String::new(),
                pool_guid: String::new(),
            },
            root,
            controller,
            helper,
            world,
            admin,
        };
        let host = pair.host().await;
        let (_, store, identity) = transport::open(&pair.controller).unwrap();
        flush(&store, &host).await;
        let pins = Pins {
            environment: ENV.into(),
            server_id: identity.public().to_string(),
            pool_guid: host.journal().unwrap().binding.pool_guid,
        };
        Self { pins, ..pair }
    }
    fn host_config(&self) -> HostConfig {
        HostConfig::simulated(self.helper.clone(), ENV, self.world.clone())
    }
    async fn host(&self) -> Host {
        Host::open(self.host_config()).await.unwrap()
    }
    fn store(&self) -> std::sync::Arc<Store> {
        transport::open(&self.controller).unwrap().1
    }
    fn prune(
        &self,
        archive: Option<&str>,
        now: u64,
        hook: impl FnMut(PruneStage) -> anyhow::Result<()>,
    ) -> anyhow::Result<maintenance::PruneReceipt> {
        maintenance::prune_with(
            &self.controller,
            &self.helper,
            archive.map(|name| self.root.join(name)).as_deref(),
            retention::DEFAULT_RETENTION_DAYS,
            now,
            &self.pins,
            hook,
        )
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

/// Runs every pending controller operation through the helper to completion.
async fn flush(store: &Store, host: &Host) {
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
}

/// History on the pair: a VM with finished power operations, a device
/// enrolled and revoked (audit entries), and one operation left queued.
/// Returns (finished power operation IDs, queued operation ID).
async fn populate(f: &Pair) -> (Vec<String>, String) {
    let store = f.store();
    let host = f.host().await;
    let accepted = store
        .handle(&f.admin, create_vm("keep-vm", "keep"))
        .unwrap();
    flush(&store, &host).await;
    let vm_id = store.read().unwrap().vms.keys().copied().next().unwrap();
    let _ = accepted;
    let mut power = Vec::new();
    for (index, kind) in [
        VmAction::Start,
        VmAction::Stop,
        VmAction::Start,
        VmAction::Stop,
    ]
    .into_iter()
    .enumerate()
    {
        let accepted = store
            .handle(&f.admin, vm_action(&format!("power-{index}"), vm_id, kind))
            .unwrap();
        flush(&store, &host).await;
        power.push(accepted["operation_id"].as_str().unwrap().to_owned());
    }
    let admin_user = store.read().unwrap().devices[&f.admin].user_id.clone();
    let device = SecretKey::generate().public().to_string();
    store
        .handle(
            &f.admin,
            Request::EnrollDevice {
                user_id: admin_user,
                endpoint_id: device.clone(),
                label: "retired".into(),
            },
        )
        .unwrap();
    store
        .handle(
            &f.admin,
            Request::RevokeDevice {
                endpoint_id: device,
            },
        )
        .unwrap();
    let queued = store
        .handle(&f.admin, vm_action("queued-start", vm_id, VmAction::Start))
        .unwrap();
    (power, queued["operation_id"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn prune_archives_then_removes_only_old_unreferenced_finished_history() {
    let f = Pair::new().await;
    let (power, queued) = populate(&f).await;
    let later = now() + 200 * DAY;
    let before = f.store().read_with_audit().unwrap();
    let journal_before = f.host().await.journal().unwrap();
    let audit_before = before.audit.len();
    assert!(audit_before >= 2);

    // Nothing is old enough yet.
    let receipt = f.prune(Some("too-early.jsonl"), now(), |_| Ok(())).unwrap();
    assert_eq!(
        (
            receipt.pruned_operations,
            receipt.pruned_audit_entries,
            receipt.pruned_helper_jobs
        ),
        (0, 0, 0)
    );
    assert!(!f.root.join("too-early.jsonl").exists());

    // A dry run reports without writing.
    let dry = f.prune(None, later, |_| Ok(())).unwrap();
    assert!(dry.dry_run && dry.archive.is_none() && !dry.activates_workers);
    assert_eq!(dry.pruned_operations, power.len());
    assert_eq!(dry.pruned_helper_jobs, power.len());
    assert_eq!(dry.pruned_audit_entries, audit_before);
    assert_eq!(
        serde_json::to_value(f.store().read_with_audit().unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap()
    );

    let receipt = f.prune(Some("archive.jsonl"), later, |_| Ok(())).unwrap();
    assert!(!receipt.dry_run);
    assert_eq!(receipt.pruned_operations, power.len());
    assert_eq!(receipt.pruned_helper_jobs, power.len());
    assert_eq!(receipt.pruned_audit_entries, audit_before);

    // The archive is private, complete and self-verifying.
    let path = f.root.join("archive.jsonl");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let archive = retention::read_archive(fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(Some(archive.sha256.clone()), receipt.archive_sha256);
    let header = archive.header.clone().unwrap();
    assert_eq!(header.format, retention::ARCHIVE_FORMAT);
    assert_eq!(header.environment, ENV);
    assert_eq!(
        header.cutoff,
        later - retention::DEFAULT_RETENTION_DAYS * DAY
    );
    let archived: BTreeSet<_> = archive
        .operations
        .iter()
        .map(|r| r.public.operation_id.clone())
        .collect();
    assert_eq!(archived, power.iter().cloned().collect());
    for record in &archive.operations {
        assert_eq!(
            serde_json::to_value(record).unwrap(),
            serde_json::to_value(&before.operations[&record.public.operation_id]).unwrap()
        );
    }
    assert_eq!(
        archive
            .helper_jobs
            .iter()
            .map(|j| j.job.operation_id.clone())
            .collect::<BTreeSet<_>>(),
        archived
    );
    for job in &archive.helper_jobs {
        assert_eq!(
            serde_json::to_value(job).unwrap(),
            serde_json::to_value(&journal_before.jobs[&job.job.operation_id]).unwrap()
        );
    }
    assert_eq!(
        archive
            .audit
            .iter()
            .map(|(_, e)| e.clone())
            .collect::<Vec<_>>(),
        before.audit
    );
    // Truncation is detected.
    let bytes = fs::read(&path).unwrap();
    let cut = bytes[..bytes.len() - 2]
        .rsplitn(2, |b| *b == b'\n')
        .nth(1)
        .unwrap();
    assert!(retention::read_archive(cut).is_err());

    // What remains: unfinished and referenced work, and every request ID.
    let store = f.store();
    let after = store.read_with_audit().unwrap();
    assert!(after.audit.is_empty());
    assert!(after.operations.contains_key(&queued));
    for id in &power {
        assert!(!after.operations.contains_key(id));
    }
    for (id, record) in &before.operations {
        if !power.contains(id) {
            assert_eq!(
                serde_json::to_value(&after.operations[id]).unwrap(),
                serde_json::to_value(record).unwrap()
            );
        }
    }
    assert_eq!(after.requests, before.requests);
    assert_eq!(receipt.retained_request_ids, before.requests.len());
    let vm = after.vms.values().next().unwrap();
    assert_eq!(vm.state, before.vms.values().next().unwrap().state);
    let create = before
        .operations
        .values()
        .find(|r| r.public.request_id == "keep-vm")
        .unwrap();
    assert!(after.operations.contains_key(&create.public.operation_id));

    // An archived request ID cannot be reused to replay the operation.
    let vm_id = *after.vms.keys().next().unwrap();
    let error = store
        .handle(&f.admin, vm_action("power-0", vm_id, VmAction::Start))
        .unwrap_err();
    assert_eq!(error.code, "conflict");
    assert!(error.message.contains("archived"), "{}", error.message);
    let operation = store
        .handle(
            &f.admin,
            Request::GetOperation {
                operation_id: power[0].clone(),
            },
        )
        .unwrap_err();
    assert_eq!(operation.code, "forbidden");

    // Audit sequence numbers continue rather than restart.
    let last = before.audit.len() as u64 - 1;
    let admin_user = after.devices[&f.admin].user_id.clone();
    store
        .handle(
            &f.admin,
            Request::EnrollDevice {
                user_id: admin_user,
                endpoint_id: SecretKey::generate().public().to_string(),
                label: "fresh".into(),
            },
        )
        .unwrap();
    let (audit, _) = store.audit_page(None, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].0 > last, "{audit:?}");
    drop(store);

    // The helper kept only what it may still be asked about, and the pair
    // still opens, backs up and finishes the queued work.
    let host = f.host().await;
    let journal = host.journal().unwrap();
    for id in &power {
        assert!(!journal.jobs.contains_key(id));
    }
    assert_eq!(
        serde_json::to_value(&journal.owners).unwrap(),
        serde_json::to_value(&journal_before.owners).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&journal.machines).unwrap(),
        serde_json::to_value(&journal_before.machines).unwrap()
    );
    drop(host);
    f.backup("after-prune");

    // Converged: a second run finds no operations or jobs, only the audit
    // entry written above (old relative to the simulated clock).
    let again = f.prune(Some("again.jsonl"), later, |_| Ok(())).unwrap();
    assert_eq!(
        (
            again.pruned_operations,
            again.pruned_helper_jobs,
            again.pruned_audit_entries
        ),
        (0, 0, 1)
    );
    let third = f.prune(Some("third.jsonl"), later, |_| Ok(())).unwrap();
    assert_eq!(
        (
            third.pruned_operations,
            third.pruned_helper_jobs,
            third.pruned_audit_entries
        ),
        (0, 0, 0)
    );
    assert!(!f.root.join("third.jsonl").exists());
    {
        let store = f.store();
        let host = f.host().await;
        flush(&store, &host).await;
        assert_eq!(
            store.read().unwrap().operations[&queued].public.status,
            OperationStatus::Verified
        );
    }
    // Existing archives are never overwritten.
    let store = f.store();
    store
        .handle(&f.admin, vm_action("power-late", vm_id, VmAction::Stop))
        .unwrap();
    let host = f.host().await;
    flush(&store, &host).await;
    drop((store, host));
    let error = f
        .prune(Some("archive.jsonl"), later + 365 * DAY, |_| Ok(()))
        .unwrap_err();
    assert!(format!("{error:#}").contains("archive"), "{error:#}");
}

#[tokio::test]
async fn interrupted_prune_leaves_a_valid_pair_and_a_rerun_converges() {
    let f = Pair::new().await;
    let (power, _) = populate(&f).await;
    let later = now() + 200 * DAY;

    // Failing after the archive is durable removes nothing.
    let before = serde_json::to_value(f.store().read_with_audit().unwrap()).unwrap();
    let error = f
        .prune(Some("first.jsonl"), later, |stage| match stage {
            PruneStage::ArchiveWritten => bail!("stopped after export"),
            _ => Ok(()),
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("stopped after export"));
    assert!(f.root.join("first.jsonl").exists());
    assert_eq!(
        serde_json::to_value(f.store().read_with_audit().unwrap()).unwrap(),
        before
    );

    // Failing between the two databases leaves finished helper jobs whose
    // controller operation survives only as a request-ID tombstone.
    let error = f
        .prune(Some("second.jsonl"), later, |stage| match stage {
            PruneStage::ControllerPruned => bail!("stopped between databases"),
            _ => Ok(()),
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("stopped between databases"));
    let catalog = f.store().read().unwrap();
    let journal = f.host().await.journal().unwrap();
    for id in &power {
        assert!(!catalog.operations.contains_key(id));
        assert!(journal.jobs.contains_key(id));
    }
    f.backup("interrupted");

    let rerun = f.prune(Some("third.jsonl"), later, |_| Ok(())).unwrap();
    assert_eq!(rerun.pruned_operations, 0);
    assert_eq!(rerun.pruned_helper_jobs, power.len());
    let archive =
        retention::read_archive(fs::File::open(f.root.join("third.jsonl")).unwrap()).unwrap();
    assert_eq!(archive.helper_jobs.len(), power.len());
    let journal = f.host().await.journal().unwrap();
    for id in &power {
        assert!(!journal.jobs.contains_key(id));
    }
    f.backup("converged");
}

#[tokio::test]
async fn retention_keeps_unfinished_work_and_everything_connected_to_it() {
    let f = Pair::new().await;
    let (power, queued) = populate(&f).await;
    let cutoff = now() + 20 * DAY;
    let catalog = f.store().read_with_audit().unwrap();
    let audit: Vec<(u64, Value)> = (0u64..).zip(catalog.audit.iter().cloned()).collect();
    let journal = f.host().await.journal().unwrap();

    let baseline = retention::plan(&catalog, &audit, &journal, cutoff).unwrap();
    assert_eq!(baseline.operations, power.iter().cloned().collect());
    assert!(!baseline.operations.contains(&queued));

    // A blocked operation is unfinished and stays.
    let mut blocked = catalog.clone();
    blocked.operations.get_mut(&power[0]).unwrap().public.status = OperationStatus::Blocked;
    let plan = retention::plan(&blocked, &audit, &journal, cutoff).unwrap();
    assert!(!plan.operations.contains(&power[0]));
    assert!(!plan.helper_jobs.contains(&power[0]));

    // An operation an unfinished one refers to stays, and so does anything
    // that refers to an unfinished one, transitively.
    let mut linked = catalog.clone();
    // References are exact ID strings anywhere in a record (recovery specs,
    // shrink finishes); `error` stands in for such a field here.
    linked.operations.get_mut(&queued).unwrap().public.error = Some(power[1].clone());
    linked.operations.get_mut(&power[2]).unwrap().public.error = Some(power[1].clone());
    let plan = retention::plan(&linked, &audit, &journal, cutoff).unwrap();
    assert!(!plan.operations.contains(&power[1]));
    assert!(!plan.operations.contains(&power[2]));
    assert!(plan.operations.contains(&power[0]) && plan.operations.contains(&power[3]));

    // Core state that names an operation (holds, records) keeps it.
    let mut held = journal.clone();
    held.control = Some(json!({"hold": power[3]}));
    let plan = retention::plan(&catalog, &audit, &held, cutoff).unwrap();
    assert!(!plan.operations.contains(&power[3]));
    assert!(!plan.helper_jobs.contains(&power[3]));

    // A helper job still running keeps its controller operation.
    let mut running = journal.clone();
    running.jobs.get_mut(&power[0]).unwrap().status = OperationStatus::Running;
    let plan = retention::plan(&catalog, &audit, &running, cutoff).unwrap();
    assert!(!plan.operations.contains(&power[0]));

    // Helper jobs without a request-ID tombstone are never pruned.
    let mut untracked = catalog.clone();
    untracked.requests.retain(|_, id| *id != power[0]);
    let plan = retention::plan(&untracked, &audit, &journal, cutoff).unwrap();
    assert!(!plan.helper_jobs.contains(&power[0]));

    // Recent history stays.
    let plan = retention::plan(&catalog, &audit, &journal, now() - DAY).unwrap();
    assert!(plan.is_empty());
}
