//! Explicit cold repair of one identified schema-marker downgrade. This path
//! never opens workers, probes guests, initializes state, or discards a record.
use super::*;
use redb::{
    ReadableTableMetadata, StorageBackend, TableHandle,
    backends::{FileBackend, InMemoryBackend},
};
use std::path::Component;

const RAW_LIMIT: u64 = 256 * 1024 * 1024;
const ROW_LIMIT: usize = 64 * 1024 * 1024;
const REDB_MAGIC: &[u8] = b"redb\x1a\x0a\xa9\x0d\x0a";

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Debug, Serialize)]
pub struct ManagedSchemaRepairReceipt {
    pub schema: u32,
    pub environment: String,
    pub previous_schema: u32,
    pub repaired_schema: u32,
    pub journal_sha256_before: String,
    pub journal_record_sha256_before: String,
    pub journal_record_sha256_after: String,
    pub helper_sequence: u64,
    pub retained_jobs: usize,
    pub retained_imports: usize,
    pub queued_acknowledgments: Vec<String>,
    pub native_recovery_validated: bool,
    pub activates_workers: bool,
}

#[derive(Clone, Copy)]
struct PathPolicy {
    uid: u32,
    sticky_ancestors: bool,
}

fn canonical(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && path.canonicalize()? == path,
        "use an existing absolute symlink-free canonical path"
    );
    Ok(())
}
fn ancestry(path: &Path, policy: PathPolicy) -> Result<()> {
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        ensure!(
            meta.uid() == 0 || meta.uid() == policy.uid,
            "untrusted repair path owner"
        );
        ensure!(
            meta.mode() & 0o022 == 0 || policy.sticky_ancestors && meta.mode() & 0o1000 != 0,
            "writable repair path ancestor refused"
        );
    }
    Ok(())
}
fn private_directory(path: &Path, policy: PathPolicy) -> Result<()> {
    canonical(path)?;
    ancestry(path, policy)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == policy.uid && meta.mode() & 0o077 == 0,
        "repair state and backup directories must be private and operator-owned"
    );
    Ok(())
}
fn private_file(path: &Path, policy: PathPolicy) -> Result<fs::File> {
    canonical(path)?;
    ancestry(path, policy)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.uid() == policy.uid && meta.nlink() == 1 && meta.mode() & 0o077 == 0,
        "repair inputs must be private single-link regular files owned by the operator"
    );
    Ok(fs::File::open(path)?)
}
fn bytes(path: &Path, policy: PathPolicy, limit: u64) -> Result<Vec<u8>> {
    let file = private_file(path, policy)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "repair input exceeds size limit"
    );
    let mut value = Vec::new();
    file.take(limit + 1).read_to_end(&mut value)?;
    ensure!(
        value.len() as u64 <= limit,
        "repair input exceeds size limit"
    );
    Ok(value)
}
fn hash(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn entries(path: &Path, expected: &[&str]) -> Result<()> {
    let actual = fs::read_dir(path)?
        .map(|entry| {
            Ok(entry?
                .file_name()
                .to_str()
                .context("non-UTF8 repair input")?
                .to_owned())
        })
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        actual == expected.iter().map(|v| (*v).to_owned()).collect(),
        "unexpected files in repair state or cold backup directory"
    );
    Ok(())
}

/// This operation intentionally stays separate from normal helper open/serve.
/// Both services must be stopped; the exclusive journal lock is authoritative.
/// The cold copy is evidence, never a source to restore or activate.
pub fn repair_managed_schema(
    state_dir: &Path,
    managed_config: &Path,
    expected_journal_sha256: &str,
    backup_directory: &Path,
    allow_native_recovery: bool,
) -> Result<ManagedSchemaRepairReceipt> {
    // SAFETY: POSIX geteuid has no arguments or memory preconditions.
    ensure!(
        unsafe { geteuid() } == 0,
        "managed schema repair must run as root"
    );
    let config = ManagedConfig::read(managed_config)?;
    repair(
        state_dir,
        managed_config,
        expected_journal_sha256,
        backup_directory,
        &config,
        PathPolicy {
            uid: 0,
            sticky_ancestors: false,
        },
        allow_native_recovery,
    )
}

fn repair(
    state_dir: &Path,
    managed_config: &Path,
    expected_hash: &str,
    backup: &Path,
    config: &ManagedConfig,
    policy: PathPolicy,
    allow_native_recovery: bool,
) -> Result<ManagedSchemaRepairReceipt> {
    ensure!(
        super::super::protocol::valid_image_digest(expected_hash),
        "an exact lowercase journal SHA256 is required"
    );
    private_directory(state_dir, policy)?;
    private_directory(backup, policy)?;
    ensure!(
        state_dir != backup && !backup.starts_with(state_dir) && !state_dir.starts_with(backup),
        "cold backup must be separate from active helper state"
    );
    super::super::maintenance::ensure_not_quarantined(state_dir)?;
    entries(state_dir, &[JOURNAL_FILE, SENTINEL_FILE])?;
    entries(backup, &[JOURNAL_FILE, SENTINEL_FILE, "managed.json"])?;
    let config_bytes = bytes(managed_config, policy, 65536)?;
    let parsed: ManagedConfig = serde_json::from_slice(&config_bytes)?;
    ensure!(
        serde_json::to_value(&parsed)? == serde_json::to_value(config)?,
        "root config changed before repair"
    );
    config.binding.validate(&config.environment)?;
    ensure!(
        bytes(&backup.join("managed.json"), policy, 65536)? == config_bytes,
        "cold config copy differs"
    );
    let sentinel = bytes(&state_dir.join(SENTINEL_FILE), policy, 128)?;
    ensure!(
        sentinel == format!("{}\n", config.environment).as_bytes(),
        "initialization sentinel differs from root environment"
    );
    ensure!(
        bytes(&backup.join(SENTINEL_FILE), policy, 128)? == sentinel,
        "cold initialization sentinel differs"
    );
    let path = state_dir.join(JOURNAL_FILE);
    let metadata = private_file(&path, policy)?.metadata()?;
    ensure!(
        (512..=RAW_LIMIT).contains(&metadata.len()),
        "journal must be an existing nonempty bounded database"
    );
    let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    ensure!(
        file.metadata()?.dev() == metadata.dev() && file.metadata()?.ino() == metadata.ino(),
        "journal file changed before locking"
    );
    // Refuse filesystems without real lock support. FileBackend then acquires
    // and retains the same exclusive redb lock through validation and commit.
    file.try_lock()
        .context("journal is in use; both owned services must be stopped")?;
    file.unlock()?;
    let backend = FileBackend::new(file).context("acquiring exclusive helper database lock")?;
    let len = backend.len()?;
    ensure!(
        len == metadata.len(),
        "journal size changed before exclusive locking"
    );
    let mut raw = vec![0; len as usize];
    backend.read(0, &mut raw)?;
    ensure!(
        raw.starts_with(REDB_MAGIC),
        "existing redb database magic missing; never initialize a repair target"
    );
    ensure!(
        hash(&raw) == expected_hash,
        "journal byte digest differs from reviewed pin"
    );
    ensure!(
        bytes(&backup.join(JOURNAL_FILE), policy, RAW_LIMIT)? == raw,
        "cold journal copy differs from pinned original"
    );
    // Validate a memory copy before opening the original database for writing.
    // Even invalid managed state or an unclean native DB cannot alter its bytes.
    let memory = InMemoryBackend::new();
    memory.set_len(len)?;
    memory.write(0, &raw)?;
    let mut builder = Database::builder();
    let native_recovery = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_recovery = native_recovery.clone();
    builder.set_repair_callback(move |session| {
        if allow_native_recovery {
            observed_recovery.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            session.abort();
        }
    });
    let snapshot = builder.create_with_backend(memory).context(
        "cold journal needs native recovery; explicit --allow-native-recovery is required",
    )?;
    let record = only_record(&snapshot)?;
    let (promoted, journal, acknowledgments) = promote(&record, config)?;
    drop(snapshot);
    // Original bytes remain exclusively locked and unchanged until this point.
    let mut current = vec![0; len as usize];
    backend.read(0, &mut current)?;
    ensure!(current == raw, "journal changed during offline validation");
    let database = builder
        .create_with_backend(backend)
        .context("opening pinned existing helper database")?;
    ensure!(
        only_record(&database)? == record,
        "original journal record differs from cold validation"
    );
    let mut transaction = database.begin_write()?;
    transaction.set_durability(Durability::Immediate)?;
    {
        let mut table = transaction.open_table(JOURNAL)?;
        ensure!(
            table.get(0)?.context("journal record vanished")?.value() == record,
            "journal changed before schema promotion"
        );
        table.insert(0, promoted.as_slice())?;
    }
    transaction.commit()?;
    ensure!(
        only_record(&database)? == promoted,
        "schema promotion readback differs"
    );
    let after = read_journal(&database)?;
    validate_scope(&after)?;
    ensure!(
        serde_json::to_value(&after)? == serde_json::to_value(&journal)?,
        "schema promotion changed decoded records"
    );
    drop(database);
    sync_parent(&path)?;
    Ok(ManagedSchemaRepairReceipt {
        schema: 1,
        environment: config.environment.clone(),
        previous_schema: 6,
        repaired_schema: 7,
        journal_sha256_before: expected_hash.into(),
        journal_record_sha256_before: hash(&record),
        journal_record_sha256_after: hash(&promoted),
        helper_sequence: journal.sequence,
        retained_jobs: journal.jobs.len(),
        retained_imports: journal.imported_vms.len(),
        queued_acknowledgments: acknowledgments,
        native_recovery_validated: native_recovery.load(std::sync::atomic::Ordering::SeqCst),
        activates_workers: false,
    })
}

fn only_record(database: &Database) -> Result<Vec<u8>> {
    let tx = database.begin_read()?;
    let tables = tx
        .list_tables()?
        .map(|t| t.name().to_owned())
        .collect::<Vec<_>>();
    ensure!(
        tables == ["host_journal_v1"] && tx.list_multimap_tables()?.next().is_none(),
        "unexpected database tables"
    );
    let table = tx.open_table(JOURNAL)?;
    ensure!(table.len()? == 1, "unexpected helper journal rows");
    let row = table
        .get(0)?
        .context("existing helper journal record missing")?;
    ensure!(
        row.value().len() <= ROW_LIMIT,
        "helper journal row exceeds size limit"
    );
    Ok(row.value().to_vec())
}

fn promote(record: &[u8], config: &ManagedConfig) -> Result<(Vec<u8>, Journal, Vec<String>)> {
    ensure!(
        record.starts_with(b"{\"schema\":6,"),
        "only the identified canonical managed6 marker can be promoted"
    );
    let before: Journal = serde_json::from_slice(record)?;
    let expected = HostBinding {
        environment: config.environment.clone(),
        backend: "managed".into(),
        pool_guid: config.binding.pool_guid.clone(),
        physical_trial: None,
        managed: Some(config.binding.clone()),
    };
    ensure!(
        before.schema == 6
            && before.binding == expected
            && before.imported_vms == config.binding.imported_vms,
        "managed binding, imports or schema differ from exact root config"
    );
    let mut promoted = record.to_vec();
    promoted[10] = b'7';
    let journal: Journal = serde_json::from_slice(&promoted)?;
    validate_scope(&journal)?;
    let mut normalized = serde_json::to_value(&journal)?;
    normalized["schema"] = json!(6);
    ensure!(
        normalized == serde_json::to_value(&before)?,
        "promotion changed records beyond schema marker"
    );
    let mut acknowledgments = Vec::new();
    let mut targets = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    for (id, job) in &journal.jobs {
        ensure!(
            id == &job.job.operation_id
                && job.fingerprint == fingerprint(&job.job)?
                && job.sequence > 0
                && job.sequence <= journal.sequence
                && sequences.insert(job.sequence),
            "job identity, fingerprint or sequence differs"
        );
        if let Some(extension) = job.job.extension.as_deref() {
            let crate::control::vm_change::JobExtension::Recover(spec) = extension else {
                bail!("unknown extended job in marker repair")
            };
            ensure!(
                spec.kind == crate::control::vm_change::RecoveryKind::AcknowledgeFailedStart
                    && job.job.kind == "recover_operation"
                    && job.status == OperationStatus::Queued
                    && job.step == "queued"
                    && job.attempts == 0
                    && job.started_at.is_none()
                    && job.finished_at.is_none()
                    && job.error.is_none()
                    && job.steps.len() == 1
                    && job.steps[0].step == Step::Recover
                    && job.steps[0].phase == Phase::Planned
                    && job.steps[0].boot_id.is_none()
                    && job.steps[0].upid.is_none()
                    && job.steps[0].identity.is_none()
                    && job.steps[0].witness.is_none()
                    && job.steps[0].started_at.is_none()
                    && job.steps[0].finished_at.is_none(),
                "only queued, unattempted acknowledgment with no effect intent is repairable"
            );
            let original = journal
                .jobs
                .get(&spec.operation_id)
                .context("acknowledgment original missing")?;
            let vm = original
                .job
                .vm
                .as_ref()
                .context("failed-start VM missing")?;
            recovery::failed_start_receipt(original, &config.binding.scope.node, vm.vm_id)?;
            ensure!(
                original.job.owner == job.job.owner && original.job.vm == job.job.vm,
                "acknowledgment frozen payload differs"
            );
            let machine = journal
                .machines
                .get(&vm.vm_id)
                .context("failed-start machine history missing")?;
            ensure!(
                machine.owner == vm.owner
                    && machine.profile == profile(vm)
                    && machine.state == MachineState::Present,
                "failed-start managed machine differs"
            );
            acknowledgments.push(id.clone());
            targets.insert(spec.operation_id.clone());
        } else {
            ensure!(
                job.status.is_terminal()
                    || job.status == OperationStatus::Blocked && job.job.kind == "start",
                "running or unrelated pending helper work forbids marker repair"
            );
            if job.status == OperationStatus::Verified {
                ensure!(
                    plan(&job.job.kind).is_some_and(|steps| steps.len() == job.steps.len()
                        && steps
                            .iter()
                            .zip(&job.steps)
                            .all(|(step, record)| step == &record.step
                                && record.phase == Phase::Verified)),
                    "verified helper work has incomplete phases"
                );
            }
        }
    }
    ensure!(
        !acknowledgments.is_empty(),
        "no queued acknowledgment explains this marker downgrade"
    );
    ensure!(
        journal
            .jobs
            .iter()
            .all(|(id, job)| job.status != OperationStatus::Blocked || targets.contains(id)),
        "unrelated blocked helper work forbids marker repair"
    );
    Ok((promoted, journal, acknowledgments))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        protocol::{User, VmState},
        vm_change::{JobExtension, RecoveryKind, RecoverySpec},
    };
    const UPID: &str = "UPID:pve1:0000DA05:031CA796:6AF6225F:qmstart:9201:root@pam:";

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        state: PathBuf,
        backup: PathBuf,
        config_path: PathBuf,
        config: ManagedConfig,
        ack: Job,
    }
    fn step(step: Step, phase: Phase) -> StepRecord {
        StepRecord {
            step,
            phase,
            boot_id: None,
            upid: None,
            identity: None,
            witness: None,
            started_at: None,
            finished_at: None,
        }
    }
    fn record(job: Job, sequence: u64, status: OperationStatus) -> HostJob {
        let steps = plan(&job.kind)
            .unwrap()
            .iter()
            .map(|s| step(*s, Phase::Verified))
            .collect();
        HostJob {
            sequence,
            fingerprint: fingerprint(&job).unwrap(),
            job,
            status,
            step: "complete".into(),
            error: None,
            attempts: 1,
            steps,
            started_at: Some(1),
            updated_at: Some(2),
            finished_at: Some(2),
        }
    }
    fn write_private(path: &Path, value: &[u8]) {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(value).unwrap();
        f.sync_all().unwrap();
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let state = root.join("helper");
            let backup = root.join("cold-copy");
            fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
            fs::DirBuilder::new().mode(0o700).create(&backup).unwrap();
            let config: ManagedConfig =
                serde_json::from_str(include_str!("../../../tests/fixtures/managed-config.json"))
                    .unwrap();
            let config_path = root.join("managed.json");
            write_private(&config_path, &serde_json::to_vec(&config).unwrap());
            write_private(
                &state.join(SENTINEL_FILE),
                format!("{}\n", config.environment).as_bytes(),
            );
            let p = &config.binding.admin_policy;
            let owner:User=serde_json::from_value(json!({"user_id":"erin","role":"admin","budget_mib":p.budget_mib,
                "min_memory_budget_mib":p.min_memory_budget_mib,"max_memory_budget_mib":p.max_memory_budget_mib,
                "max_cores":p.max_cores,"max_vms":p.max_vms,"ready":true})).unwrap();
            let vm:Vm=serde_json::from_value(json!({"vm_id":9201,"owner":"erin","name":"verify-hdd","disk_mib":4096,
                "memory_mib":1024,"min_memory_mib":512,"cores":1,"state":"stopped","guest":{
                "image_sha256":crate::control::managed::UBUNTU_2404_SHA256,
                "ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}})).unwrap();
            assert_eq!(vm.state, VmState::Stopped);
            let job = |id: &str, kind: &str, vm: Option<Vm>| Job {
                operation_id: id.into(),
                kind: kind.into(),
                owner: owner.clone(),
                vm,
                disk_shrink: None,
                extension: None,
                policy_change: None,
            };
            let bootstrap = record(
                job("bootstrap", "ensure_user", None),
                1,
                OperationStatus::Verified,
            );
            let create = record(
                job("create-guest", "create_guest", Some(vm.clone())),
                2,
                OperationStatus::Verified,
            );
            let mut start = record(
                job("failed-start", "start", Some(vm.clone())),
                3,
                OperationStatus::Blocked,
            );
            start.step = "start".into();
            start.error = Some("start failed: QEMU exited with code 1".into());
            start.steps[1] = step(Step::Start, Phase::Intent);
            start.steps[1].upid = Some(UPID.into());
            start.steps[1].boot_id = Some("boot-one".into());
            let mut ack = job("ack-start", "recover_operation", Some(vm.clone()));
            ack.extension = Some(Box::new(JobExtension::Recover(RecoverySpec {
                operation_id: "failed-start".into(),
                inspection_id: "a".repeat(64),
                kind: RecoveryKind::AcknowledgeFailedStart,
            })));
            let binding = HostBinding {
                environment: config.environment.clone(),
                backend: "managed".into(),
                pool_guid: config.binding.pool_guid.clone(),
                physical_trial: None,
                managed: Some(config.binding.clone()),
            };
            let journal = Journal {
                schema: 7,
                binding: binding.clone(),
                imported_vms: config.binding.imported_vms.clone(),
                sequence: 3,
                control: Some(json!({"guid":"control-guid"})),
                owners: BTreeMap::from([(
                    "erin".into(),
                    OwnerRecord {
                        user_id: "erin".into(),
                        budget_mib: owner.budget_mib,
                        operation_id: "bootstrap".into(),
                        ready: true,
                        identities: BTreeMap::from([(
                            "dataset".into(),
                            json!({"guid":"owner-guid"}),
                        )]),
                        policy_revision: 0,
                        policy: Some(owner.policy()),
                        state: AccountState::Active,
                        delete_operation: None,
                    },
                )]),
                machines: BTreeMap::from([(
                    9201,
                    MachineRecord {
                        vm_id: 9201,
                        owner: "erin".into(),
                        profile: profile(&vm),
                        create_operation: "create-guest".into(),
                        delete_operation: None,
                        state: MachineState::Present,
                        identities: BTreeMap::from([
                            ("vm".into(), json!({"smbios1":"uuid=existing-managed"})),
                            ("volume".into(), json!({"guid":"root-guid"})),
                        ]),
                        pinned_config: Some(json!({"scsi0":"managed-root"})),
                    },
                )]),
                jobs: BTreeMap::from([
                    ("bootstrap".into(), bootstrap),
                    ("create-guest".into(), create),
                    ("failed-start".into(), start),
                ]),
                disk_shrinks: BTreeMap::new(),
                migrations: BTreeMap::new(),
            };
            validate_scope(&journal).unwrap();
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(state.join(JOURNAL_FILE))
                .unwrap();
            let database = Database::builder().create_file(file).unwrap();
            write_journal(&database, &journal).unwrap();
            let result = Self {
                _temp: temp,
                root,
                state,
                backup,
                config_path,
                config,
                ack,
            };
            let host = result.host(database, binding);
            assert_eq!(
                host.submit(result.ack.clone()).unwrap().status,
                OperationStatus::Queued
            );
            drop(host);
            result
        }
        fn host(&self, database: Database, binding: HostBinding) -> Host {
            Host {
                database,
                state_dir: self.state.clone(),
                binding,
                backend: Backend::Simulated(SimulatedWorld::new(
                    self.root.join("unused-world.json"),
                )),
                task_timeout: Duration::from_secs(1),
                poll_interval: Duration::from_millis(1),
                notify: Notify::new(),
                execution: tokio::sync::Mutex::new(()),
            }
        }
        fn edit(&self, change: impl FnOnce(&mut Journal)) {
            let db = Database::open(self.state.join(JOURNAL_FILE)).unwrap();
            let mut j = read_journal(&db).unwrap();
            change(&mut j);
            write_journal(&db, &j).unwrap();
        }
        fn reproduce_downgrade(&self) {
            self.edit(|j| j.schema = 6);
        }
        fn cold_copy(&self) -> String {
            for (source, name) in [
                (self.state.join(JOURNAL_FILE), JOURNAL_FILE),
                (self.state.join(SENTINEL_FILE), SENTINEL_FILE),
                (self.config_path.clone(), "managed.json"),
            ] {
                fs::copy(source, self.backup.join(name)).unwrap();
            }
            hash(&fs::read(self.state.join(JOURNAL_FILE)).unwrap())
        }
        fn repair(&self, pin: &str) -> Result<ManagedSchemaRepairReceipt> {
            self.repair_with_native_recovery(pin, false)
        }
        fn repair_with_native_recovery(
            &self,
            pin: &str,
            allow_native_recovery: bool,
        ) -> Result<ManagedSchemaRepairReceipt> {
            // Production fixes owner0 and forbids writable ancestors. Tests use
            // their own private temp subtree and exercise the same locked I/O,
            // cold-copy verification, exact boundary and atomic transaction.
            let uid = fs::metadata(&self.root).unwrap().uid();
            repair(
                &self.state,
                &self.config_path,
                pin,
                &self.backup,
                &self.config,
                PathPolicy {
                    uid,
                    sticky_ancestors: true,
                },
                allow_native_recovery,
            )
        }
        fn raw(&self) -> Vec<u8> {
            fs::read(self.state.join(JOURNAL_FILE)).unwrap()
        }
    }

    #[test]
    fn genuine_managed_host_submit_preserves_schema7_imports_and_extended_payload() {
        let f = Fixture::new();
        let db = Database::open(f.state.join(JOURNAL_FILE)).unwrap();
        let j = read_journal(&db).unwrap();
        let host = f.host(db, j.binding.clone());
        let before = host.journal().unwrap();
        assert_eq!(before.schema, 7);
        assert_eq!(before.imported_vms, f.config.binding.imported_vms);
        let count = before.jobs.len();
        let sequence = before.sequence;
        assert_eq!(
            host.submit(f.ack.clone()).unwrap().status,
            OperationStatus::Queued
        );
        let after = host.journal().unwrap();
        assert_eq!(after.jobs.len(), count);
        assert_eq!(after.sequence, sequence);
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
    }

    #[test]
    fn byte_pinned_cold_repair_promotes_only_one_marker_and_reopens_for_exact_pending_retry() {
        let f = Fixture::new();
        f.reproduce_downgrade();
        let before = {
            let db = Database::open(f.state.join(JOURNAL_FILE)).unwrap();
            only_record(&db).unwrap()
        };
        // Close cleanly and repin after the deliberate read-only test observation.
        let pin = f.cold_copy();
        let original = f.raw();
        let receipt = f.repair(&pin).unwrap();
        assert!(!receipt.activates_workers);
        assert_eq!(receipt.previous_schema, 6);
        assert_eq!(receipt.repaired_schema, 7);
        assert_eq!(receipt.retained_jobs, 4);
        assert_eq!(
            receipt.retained_imports,
            f.config.binding.imported_vms.len()
        );
        assert_eq!(receipt.queued_acknowledgments, ["ack-start"]);
        let db = Database::open(f.state.join(JOURNAL_FILE)).unwrap();
        let after = only_record(&db).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.iter().zip(&after).filter(|(a, b)| a != b).count(), 1);
        assert_eq!(before[10], b'6');
        assert_eq!(after[10], b'7');
        let journal = read_journal(&db).unwrap();
        validate_scope(&journal).unwrap();
        let host = f.host(db, journal.binding.clone());
        assert_eq!(
            host.submit(f.ack.clone()).unwrap().status,
            OperationStatus::Queued
        );
        let reopened = host.journal().unwrap();
        assert_eq!(reopened.schema, 7);
        assert_eq!(reopened.jobs.len(), 4);
        assert_eq!(
            reopened.jobs["failed-start"].steps[1].upid.as_deref(),
            Some(UPID)
        );
        assert_eq!(reopened.jobs["ack-start"].attempts, 0);
        assert_eq!(reopened.jobs["ack-start"].steps[0].phase, Phase::Planned);
        assert_eq!(
            hash(&fs::read(f.backup.join(JOURNAL_FILE)).unwrap()),
            hash(&original)
        );
        assert_eq!(pin, hash(&original));
    }

    #[test]
    fn leave_database_unclean_for_crash_fixture() {
        let Some(path) = std::env::var_os("PVE_SCHEMA_REPAIR_CRASH_FIXTURE") else {
            return;
        };
        let database = Database::open(path).unwrap();
        let journal = read_journal(&database).unwrap();
        assert_eq!(journal.schema, 6);
        write_journal(&database, &journal).unwrap();
        // Match a service terminated by SIGTERM: no Rust destructors run.
        std::process::exit(0);
    }

    #[test]
    fn unclean_shutdown_requires_explicit_native_recovery_and_preserves_every_record() {
        let f = Fixture::new();
        f.reproduce_downgrade();
        let original_record = {
            let db = Database::open(f.state.join(JOURNAL_FILE)).unwrap();
            only_record(&db).unwrap()
        };
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "control::host::schema_repair::tests::leave_database_unclean_for_crash_fixture",
                "--nocapture",
            ])
            .env(
                "PVE_SCHEMA_REPAIR_CRASH_FIXTURE",
                f.state.join(JOURNAL_FILE),
            )
            .status()
            .unwrap();
        assert!(status.success());
        let pin = f.cold_copy();
        let raw = f.raw();
        assert!(f.repair(&pin).is_err());
        assert_eq!(
            f.raw(),
            raw,
            "refused native recovery changed original bytes"
        );
        let receipt = f.repair_with_native_recovery(&pin, true).unwrap();
        assert!(receipt.native_recovery_validated);
        let db = Database::open(f.state.join(JOURNAL_FILE)).unwrap();
        let repaired_record = only_record(&db).unwrap();
        let mut expected = original_record;
        expected[10] = b'7';
        assert_eq!(repaired_record, expected);
        assert_eq!(read_journal(&db).unwrap().jobs["ack-start"].attempts, 0);
        assert_eq!(fs::read(f.backup.join(JOURNAL_FILE)).unwrap(), raw);
    }

    #[test]
    fn native_recovery_permission_does_not_admit_unrelated_pending_work_or_write_original() {
        let f = Fixture::new();
        f.reproduce_downgrade();
        f.edit(|j| j.jobs.get_mut("create-guest").unwrap().status = OperationStatus::Queued);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "control::host::schema_repair::tests::leave_database_unclean_for_crash_fixture",
                "--nocapture",
            ])
            .env(
                "PVE_SCHEMA_REPAIR_CRASH_FIXTURE",
                f.state.join(JOURNAL_FILE),
            )
            .status()
            .unwrap();
        assert!(status.success());
        let pin = f.cold_copy();
        let raw = f.raw();
        assert!(f.repair_with_native_recovery(&pin, true).is_err());
        assert_eq!(f.raw(), raw);
    }

    #[test]
    fn wrong_hash_root_binding_imports_or_backup_refuse_before_any_original_write() {
        for case in [
            "hash",
            "binding",
            "imports",
            "backup-db",
            "backup-config",
            "sentinel",
            "no-ack",
            "schema7",
        ] {
            let mut f = Fixture::new();
            f.reproduce_downgrade();
            match case {
                "binding" => {
                    f.config.binding.scope.root_guid = "999".into();
                    fs::write(&f.config_path, serde_json::to_vec(&f.config).unwrap()).unwrap();
                }
                "imports" => f.edit(|j| j.imported_vms.clear()),
                "no-ack" => f.edit(|j| {
                    j.jobs.remove("ack-start");
                }),
                "schema7" => f.edit(|j| j.schema = 7),
                _ => {}
            }
            let mut pin = f.cold_copy();
            match case {
                "hash" => pin = "f".repeat(64),
                "backup-db" => fs::write(f.backup.join(JOURNAL_FILE), b"different").unwrap(),
                "backup-config" => fs::write(f.backup.join("managed.json"), b"different").unwrap(),
                "sentinel" => {
                    fs::write(f.state.join(SENTINEL_FILE), b"wrong-environment\n").unwrap()
                }
                _ => {}
            }
            let raw = f.raw();
            assert!(f.repair(&pin).is_err(), "{case}");
            assert_eq!(
                hash(&f.raw()),
                hash(&raw),
                "refusal wrote original DB: {case}"
            );
        }
    }

    #[test]
    fn attempted_intented_running_unrelated_or_unreceipted_work_is_not_repairable() {
        for case in [
            "running",
            "attempted",
            "intent",
            "witness",
            "unrelated-queued",
            "unrelated-blocked",
            "missing-upid",
            "foreign-upid",
            "unknown-extension",
        ] {
            let f = Fixture::new();
            f.reproduce_downgrade();
            f.edit(|j| match case {
                "running" => j.jobs.get_mut("ack-start").unwrap().status = OperationStatus::Running,
                "attempted" => j.jobs.get_mut("ack-start").unwrap().attempts = 1,
                "intent" => j.jobs.get_mut("ack-start").unwrap().steps[0].phase = Phase::Intent,
                "witness" => {
                    j.jobs.get_mut("ack-start").unwrap().steps[0].witness =
                        Some(json!({"unknown":true}))
                }
                "unrelated-queued" => {
                    j.jobs.get_mut("create-guest").unwrap().status = OperationStatus::Queued
                }
                "unrelated-blocked" => {
                    j.jobs.get_mut("create-guest").unwrap().status = OperationStatus::Blocked
                }
                "missing-upid" => j.jobs.get_mut("failed-start").unwrap().steps[1].upid = None,
                "foreign-upid" => {
                    j.jobs.get_mut("failed-start").unwrap().steps[1].upid =
                        Some(UPID.replace("9201", "103"))
                }
                "unknown-extension" => {
                    let r = j.jobs.get_mut("ack-start").unwrap();
                    let Some(JobExtension::Recover(spec)) = r.job.extension.as_deref_mut() else {
                        unreachable!()
                    };
                    spec.kind = RecoveryKind::AbortPreparation;
                    r.fingerprint = fingerprint(&r.job).unwrap();
                }
                _ => unreachable!(),
            });
            let pin = f.cold_copy();
            let raw = f.raw();
            assert!(f.repair(&pin).is_err(), "{case}");
            assert_eq!(hash(&f.raw()), hash(&raw), "{case}");
        }
    }

    #[test]
    fn missing_state_quarantine_links_and_an_open_database_never_bootstrap_or_bypass_lock() {
        for case in [
            "missing-db",
            "missing-sentinel",
            "quarantine",
            "symlink",
            "hardlink",
            "open",
        ] {
            let f = Fixture::new();
            f.reproduce_downgrade();
            let pin = f.cold_copy();
            let open = if case == "open" {
                Some(Database::open(f.state.join(JOURNAL_FILE)).unwrap())
            } else {
                None
            };
            match case {
                "missing-db" => fs::remove_file(f.state.join(JOURNAL_FILE)).unwrap(),
                "missing-sentinel" => fs::remove_file(f.state.join(SENTINEL_FILE)).unwrap(),
                "quarantine" => write_private(&f.state.join("restore-review-required"), b"review"),
                "symlink" => {
                    fs::remove_file(f.backup.join(SENTINEL_FILE)).unwrap();
                    std::os::unix::fs::symlink(
                        f.state.join(SENTINEL_FILE),
                        f.backup.join(SENTINEL_FILE),
                    )
                    .unwrap();
                }
                "hardlink" => {
                    fs::hard_link(f.state.join(SENTINEL_FILE), f.root.join("another-sentinel"))
                        .unwrap()
                }
                _ => {}
            }
            assert!(f.repair(&pin).is_err(), "{case}");
            if case == "missing-db" {
                assert!(!f.state.join(JOURNAL_FILE).exists());
            }
            drop(open);
        }
    }
}
