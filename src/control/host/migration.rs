//! Scoped migration helper. The network daemon supplies only an immutable plan
//! and bounded chunks; local creation records choose every path and resource.
use super::*;
use crate::control::migration::{
    self as wire, HostMigrationRequest, HostTransferRecord, Manifest, Phase as TransferPhase,
    Role as TransferRole, Snapshot, VolumeManifest,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{os::unix::fs::FileExt, sync::OnceLock};

pub(super) const WORKER: &str = include_str!("../migration_worker.py");
const KEYS: &[&str] = &[
    "type",
    "guid",
    "createtxg",
    "volsize",
    "refreservation",
    "origin",
    "org.pve-dev:owner",
    "org.pve-dev:image",
    "org.pve-dev:migration",
    "readonly",
];
const ZERO_EXTENT: usize = 8 * 1024 * 1024;
type CacheKey = (String, String, String, String);
struct OpenVolume {
    file: fs::File,
    config_path: PathBuf,
    config_sha256: String,
    vm_id: u16,
    device: Value,
    source: bool,
    modified: Option<std::time::SystemTime>,
}
static FILES: OnceLock<Mutex<BTreeMap<CacheKey, Arc<OpenVolume>>>> = OnceLock::new();
fn cache() -> &'static Mutex<BTreeMap<CacheKey, Arc<OpenVolume>>> {
    FILES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn normalize(config: &Value) -> Result<BTreeMap<String, String>> {
    let values = config
        .as_object()
        .context("VM configuration must be an object")?;
    let mut out = BTreeMap::new();
    for (key, value) in values {
        if matches!(key.as_str(), "digest" | "lock") {
            continue;
        }
        let mut value = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(v) => if *v { "1" } else { "0" }.into(),
            _ => bail!("unsupported VM configuration value"),
        };
        if key == "memory" {
            value = value.trim_start_matches("current=").into();
        }
        if key == "description" {
            value = value.trim().into();
        }
        if key == "ide2" {
            value = value
                .split(',')
                .filter(|v| !v.starts_with("size="))
                .collect::<Vec<_>>()
                .join(",");
        }
        out.insert(key.clone(), value);
    }
    Ok(out)
}
fn data_digest(prefix: &[u8], size: u64) -> String {
    let mut hash = Sha256::new();
    let zero = [0u8; 65536];
    let mut offset = 0u64;
    while offset < size {
        let length = (size - offset).min(zero.len() as u64) as usize;
        let mut block = zero;
        if offset < prefix.len() as u64 {
            let end = (prefix.len() - offset as usize).min(length);
            block[..end].copy_from_slice(&prefix[offset as usize..offset as usize + end]);
        }
        hash.update(&block[..length]);
        offset += length as u64;
    }
    format!("{hash:x}", hash = hash.finalize())
}
impl Host {
    fn migration_context(&self, vm_id: u16, id: &str) -> Result<Context> {
        let journal = self.journal()?;
        let machine = journal
            .machines
            .get(&vm_id)
            .context("VM lacks helper creation/enrollment proof")?;
        ensure!(
            machine.state == MachineState::Present,
            "migration requires a present managed VM"
        );
        let owner = journal
            .owners
            .get(&machine.owner)
            .filter(|o| o.ready && o.state == AccountState::Active)
            .context("VM owner not ready/active")?;
        let policy = owner
            .policy
            .clone()
            .unwrap_or_else(|| journal.jobs[&machine.create_operation].job.owner.policy());
        Ok(Context {
            environment: self.binding.environment.clone(),
            operation_id: id.into(),
            kind: "migration".into(),
            owner: machine.owner.clone(),
            budget_mib: owner.budget_mib,
            vm: Some(Vm {
                vm_id,
                owner: machine.owner.clone(),
                name: machine.profile.name.clone(),
                disk_mib: machine.profile.disk_mib,
                memory_mib: machine.profile.memory_mib,
                min_memory_mib: machine.profile.min_memory_mib,
                cores: machine.profile.cores,
                state: crate::control::protocol::VmState::Stopped,
                guest: machine.profile.guest.clone(),
            }),
            boot_id: String::new(),
            policy_revision: owner.policy_revision,
            owner_policy: policy,
            policy_change: None,
            disk_shrink: None,
            extension: None,
            scope: self.binding.namespace(),
            managed: self.binding.managed.clone(),
        })
    }
    fn migration_record(&self, id: &str) -> Result<HostTransferRecord> {
        self.journal()?
            .migrations
            .get(id)
            .cloned()
            .context("migration was not admitted by this helper")
    }
    fn migration_change<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Journal, &mut HostTransferRecord) -> Result<T>,
    ) -> Result<T> {
        self.change(|j| {
            let mut r = j
                .migrations
                .get(id)
                .cloned()
                .context("migration helper record missing")?;
            let result = f(j, &mut r)?;
            j.migrations.insert(id.into(), r);
            Ok(result)
        })
    }
    async fn migration_seal_effect(
        &self,
        id: &str,
        effect: impl Future<Output = Result<Value>>,
    ) -> Result<Value> {
        self.migration_change(id, |_, record| {
            ensure!(
                record.intent.is_none(),
                "ambiguous target seal intent; inspect retained staging without replay; original failure: {}",
                record.error.as_deref().unwrap_or("completion unknown")
            );
            record.intent = Some("seal".into());
            Ok(())
        })?;
        match effect.await {
            Ok(proof) => Ok(proof),
            Err(error) => {
                let diagnostic = format!("{error:#}");
                self.migration_change(id, |_, record| {
                    record.error.get_or_insert_with(|| diagnostic.clone());
                    Ok(())
                })
                .with_context(|| format!("retaining native seal failure: {diagnostic}"))?;
                Err(error)
            }
        }
    }
    async fn migration_native(
        &self,
        context: &Context,
        action: &str,
        record: Option<&HostTransferRecord>,
        extra: Value,
    ) -> Result<Value> {
        let mut p = json!({"binding":self.binding,"state_dir":self.state_dir,"vm_id":context.vm_id(),"owner":context.owner,"disk_mib":context.vm()?.disk_mib,"helper_pid":std::process::id(),"action":action});
        if let Some(record) = record {
            p["record"] = json!(record);
            p["plan_sha256"] = json!(wire::digest(&record.plan));
            p["native"] = record.native.clone().unwrap_or(Value::Null);
            if let Some(m) = &record.manifest {
                p["manifest_sha256"] = json!(wire::digest(m));
            }
        }
        for (k, v) in extra
            .as_object()
            .context("migration extra must be object")?
        {
            p[k] = v.clone();
        }
        match &self.backend {
            Backend::Lab(_) => {
                Worker {
                    label: "migration adapter",
                    script: WORKER,
                    args: &[],
                    input: Some(&p),
                    budget: disk_maintenance_timeout(context.vm()?.disk_mib),
                    grace: DEFAULT_GRACE,
                    max_output: MAX_HOST_MESSAGE,
                    gid: Some(0),
                    timeout_hint: "inspect retained state without replay",
                }
                .run_json()
                .await
            }
            Backend::Simulated(world) => {
                self.migration_simulate(world, context, action, record, extra)
            }
        }
    }
    async fn migration_snapshot(&self, vm_id: u16) -> Result<(Snapshot, Value)> {
        let id = wire::digest(&("migration-inspection", &self.binding.environment, vm_id));
        let context = self.migration_context(vm_id, &id)?;
        self.verify_machine(&context).await?;
        ensure!(
            self.status(&context).await? == "stopped",
            "migration requires a stopped guest"
        );
        let native = self
            .migration_native(&context, "snapshot", None, json!({}))
            .await?;
        let config: BTreeMap<String, String> = serde_json::from_value(native["config"].clone())?;
        let machine = self.journal()?.machines[&vm_id].clone();
        let mut identities = BTreeMap::new();
        for slot in ["root", "cloudinit"] {
            identities.insert(slot.into(), native["volumes"][slot].clone());
        }
        let extra = native["snippets"].as_object().map_or(0, |v| {
            v.values().filter_map(|v| v["bytes"].as_u64()).sum::<u64>()
        }) + native["generated_meta"]
            .as_str()
            .map_or(0, |s| s.len() as u64);
        let vm = context.vm()?.clone();
        let byte_ceiling = (vm.disk_mib + 4)
            .checked_mul(MIB)
            .and_then(|v| v.checked_add(extra))
            .context("migration size overflow")?;
        let snapshot = Snapshot {
            vm,
            machine_sha256: wire::digest(&machine),
            config_sha256: wire::digest(&config),
            config,
            volume_identities: identities,
            byte_ceiling,
        };
        Ok((snapshot, native))
    }
    pub(super) async fn handle_migration(&self, request: HostMigrationRequest) -> Result<Value> {
        let _execution = self.execution.lock().await;
        let hot = match &request {
            HostMigrationRequest::ReadChunk {
                migration_id, slot, ..
            }
            | HostMigrationRequest::WriteChunk {
                migration_id, slot, ..
            } => cache()
                .lock()
                .map_err(|_| anyhow!("migration descriptor cache poisoned"))?
                .contains_key(&self.migration_cache_key(migration_id, slot)),
            _ => false,
        };
        // A live attested FD pins the admitted device; per-chunk config/PID and
        // descriptor checks stay cheap. Cache misses/restarts reattest the full
        // machine/pool/GUID/fence before opening a new descriptor.
        if !hot {
            self.check_host().await?;
        }
        match request {
            HostMigrationRequest::InspectVm { vm_id } => {
                Ok(json!(self.migration_snapshot(vm_id).await?.0))
            }
            HostMigrationRequest::Prepare {
                plan,
                role,
                manifest,
            } => {
                plan.validate()?;
                ensure!(
                    plan.peer(role).environment == self.binding.environment,
                    "migration addressed to a different helper environment"
                );
                let id = plan.migration_id.clone();
                if let Some(existing) = self.journal()?.migrations.get(&id) {
                    ensure!(
                        existing.plan == plan && existing.role == role,
                        "migration ID already belongs to another plan/role"
                    );
                    ensure!(
                        existing.phase != TransferPhase::Preparing,
                        "preparation outcome is ambiguous; inspect without replay"
                    );
                    return Ok(json!(existing));
                }
                let vm_id = if role == TransferRole::Source {
                    plan.source_vm_id
                } else {
                    plan.target_vm_id
                };
                let (snapshot, native) = self.migration_snapshot(vm_id).await?;
                let vm = plan.vm(role);
                let mut expected = vm.clone();
                if role == TransferRole::Destination {
                    expected.guest = None;
                    ensure!(
                        self.journal()?.machines[&vm_id].create_operation
                            == crate::control::migration_transport::target_create_id(&id),
                        "destination is not this migration's blank staging VM"
                    );
                }
                ensure!(
                    serde_json::to_value(&snapshot.vm)? == serde_json::to_value(&expected)?,
                    "local VM resource profile differs from migration plan"
                );
                if role == TransferRole::Source {
                    ensure!(
                        snapshot.machine_sha256 == plan.source_machine_sha256
                            && snapshot.config_sha256 == plan.source_config_sha256,
                        "source identity/configuration changed since administrator review"
                    );
                }
                if let Some(scope) = &self.binding.physical_trial {
                    ensure!(
                        scope.scope.permits(&vm.owner, Some(vm_id)),
                        "migration escapes physical trial"
                    );
                }
                let machine = self.journal()?.machines[&vm_id].clone();
                let mut record = HostTransferRecord {
                    plan,
                    role,
                    phase: if role == TransferRole::Source {
                        TransferPhase::Authorized
                    } else {
                        TransferPhase::Preparing
                    },
                    machine_before: machine,
                    config_before: snapshot.config,
                    volumes_before: snapshot.volume_identities,
                    manifest,
                    offsets: BTreeMap::new(),
                    last_chunk: None,
                    target_identities: None,
                    handoff: None,
                    error: None,
                    native: Some(native),
                    intent: if role == TransferRole::Destination {
                        Some("stage".into())
                    } else {
                        None
                    },
                };
                if role == TransferRole::Destination {
                    record
                        .manifest
                        .as_ref()
                        .context("destination requires the final stopped manifest")?
                        .validate(&record.plan)?;
                }
                self.change(|j| {
                    ensure!(
                        !j.migrations
                            .values()
                            .any(|r| !r.phase.terminal() && r.machine_before.vm_id == vm_id),
                        "another migration retains this VM"
                    );
                    j.schema = 6;
                    j.migrations.insert(id.clone(), record.clone());
                    Ok(())
                })?;
                if role == TransferRole::Destination {
                    let context = self.migration_context(vm_id, &id)?;
                    let result = self
                        .migration_native(
                            &context,
                            "prepare_target",
                            Some(&record),
                            json!({"manifest":record.manifest}),
                        )
                        .await?;
                    record.native = Some(result["native"].clone());
                    record.phase = TransferPhase::Receiving;
                    record.intent = None;
                    self.change(|j| {
                        j.migrations.insert(id, record.clone());
                        Ok(())
                    })?;
                }
                Ok(json!(record))
            }
            HostMigrationRequest::Fence { migration_id } => {
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Source,
                    "only source can fence/export"
                );
                if record.phase == TransferPhase::Fenced {
                    return Ok(json!(
                        record.manifest.context("fenced source missing manifest")?
                    ));
                }
                ensure!(
                    record.phase == TransferPhase::Authorized,
                    "ambiguous fence intent cannot be replayed automatically"
                );
                let context = self.migration_context(record.plan.source_vm_id, &migration_id)?;
                self.migration_change(&migration_id, |_, r| {
                    r.phase = TransferPhase::Preparing;
                    r.intent = Some("fence".into());
                    Ok(())
                })?;
                let result = self
                    .migration_native(&context, "fence", Some(&record), json!({}))
                    .await?;
                let manifest: Manifest = serde_json::from_value(result["manifest"].clone())?;
                manifest.validate(&record.plan)?;
                self.migration_change(&migration_id, |_, r| {
                    r.native = Some(result["native"].clone());
                    r.manifest = Some(manifest.clone());
                    r.phase = TransferPhase::Fenced;
                    r.intent = None;
                    Ok(())
                })?;
                Ok(json!(manifest))
            }
            HostMigrationRequest::ReadChunk {
                migration_id,
                slot,
                offset,
            } => {
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Source
                        && matches!(
                            record.phase,
                            TransferPhase::Fenced | TransferPhase::Relinquished
                        ),
                    "source is not fenced for read"
                );
                let volume = slot_manifest(&record, &slot)?;
                ensure!(offset < volume.bytes, "chunk starts beyond manifest");
                self.migration_read(&record, &slot, offset).await
            }
            HostMigrationRequest::WriteChunk {
                migration_id,
                slot,
                offset,
                bytes_base64,
                sha256,
                zero_bytes,
            } => {
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Destination
                        && record.phase == TransferPhase::Receiving,
                    "target is not inactive receiving staging"
                );
                let volume = slot_manifest(&record, &slot)?;
                let data = if let Some(length) = zero_bytes {
                    ensure!(
                        bytes_base64.is_empty() && length > 0 && length <= ZERO_EXTENT as u64,
                        "invalid zero extent"
                    );
                    vec![0u8; length as usize]
                } else {
                    ensure!(
                        bytes_base64.len() <= wire::CHUNK_BYTES.div_ceil(3) * 4,
                        "encoded chunk exceeds bound"
                    );
                    let data = STANDARD.decode(bytes_base64)?;
                    ensure!(
                        !data.is_empty() && data.len() <= wire::CHUNK_BYTES,
                        "decoded chunk exceeds bound"
                    );
                    data
                };
                ensure!(
                    offset
                        .checked_add(data.len() as u64)
                        .is_some_and(|end| end <= volume.bytes),
                    "chunk exceeds final stopped manifest"
                );
                ensure!(
                    format!("{:x}", Sha256::digest(&data)) == sha256,
                    "chunk hash mismatch"
                );
                let committed = record.offsets.get(&slot).copied().unwrap_or(0);
                let witness = wire::ChunkWitness {
                    slot: slot.clone(),
                    offset,
                    length: data.len() as u64,
                    sha256,
                };
                ensure!(
                    offset == committed
                        || offset < committed && record.last_chunk.as_ref() == Some(&witness),
                    "out-of-order/different replay chunk refused"
                );
                if offset < committed {
                    self.migration_verify_chunk(&record, &slot, offset, &data)
                        .await?;
                    return Ok(json!({"next_offset":committed}));
                }
                if let Some(previous) = &record.last_chunk
                    && previous.offset == offset
                    && previous.slot == slot
                {
                    ensure!(
                        previous == &witness,
                        "changed data for an existing write intent"
                    );
                }
                self.migration_change(&migration_id, |_, r| {
                    r.last_chunk = Some(witness);
                    Ok(())
                })?;
                self.migration_write(&record, &slot, offset, data).await?;
                let length = self
                    .migration_record(&migration_id)?
                    .last_chunk
                    .context("write intent missing")?
                    .length;
                self.migration_change(&migration_id, |_, r| {
                    r.offsets.insert(slot, offset + length);
                    Ok(())
                })?;
                Ok(json!({"next_offset":offset+length}))
            }
            HostMigrationRequest::Seal {
                migration_id,
                manifest,
            } => {
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Destination,
                    "only target can seal"
                );
                if record.phase == TransferPhase::Ready {
                    ensure!(
                        record.manifest.as_ref() == Some(&manifest),
                        "seal manifest changed"
                    );
                    let context =
                        self.migration_context(record.plan.target_vm_id, &migration_id)?;
                    let proof = self
                        .migration_native(&context, "verify_sealed", Some(&record), json!({}))
                        .await?;
                    self.migration_change(&migration_id, |_, r| {
                        r.native = Some(proof["native"].clone());
                        r.target_identities = Some(proof.clone());
                        Ok(())
                    })?;
                    return Ok(proof);
                }
                ensure!(
                    record.phase == TransferPhase::Receiving
                        && record.manifest.as_ref() == Some(&manifest),
                    "target is not receiving this immutable manifest"
                );
                for (slot, v) in manifest.volumes.iter().chain(manifest.snippets.iter()) {
                    ensure!(
                        record.offsets.get(slot) == Some(&v.bytes),
                        "target volume is incomplete"
                    );
                }
                self.migration_close(&migration_id)?;
                let context = self.migration_context(record.plan.target_vm_id, &migration_id)?;
                let proof = self
                    .migration_seal_effect(
                        &migration_id,
                        self.migration_native(
                            &context,
                            "seal",
                            Some(&record),
                            json!({"manifest":manifest}),
                        ),
                    )
                    .await?;
                // Rebind the imported VM while it remains locked/inactive. All
                // subsequent ordinary operations observe this exact native config.
                self.migration_change(&migration_id, |j, r| {
                    let machine = j
                        .machines
                        .get_mut(&r.plan.target_vm_id)
                        .context("target machine missing")?;
                    machine.profile.guest = r.plan.guest.clone();
                    machine.pinned_config = Some(normalized_pinned_config(&proof["config"])?);
                    r.target_identities = Some(proof.clone());
                    r.native = Some(proof["native"].clone());
                    r.phase = TransferPhase::Ready;
                    r.intent = None;
                    Ok(())
                })?;
                let context = self.migration_context(record.plan.target_vm_id, &migration_id)?;
                let mut identities = BTreeMap::new();
                for step in [Step::Vm, Step::Volume, Step::Attach, Step::CloudInit] {
                    identities.insert(
                        step.name().into(),
                        self.observe(&context, step)
                            .await?
                            .context("imported target identity missing")?,
                    );
                }
                if context.vm()?.guest.is_some() {
                    identities.insert(
                        Step::Image.name().into(),
                        self.observe(&context, Step::Image)
                            .await?
                            .context("imported target image proof missing")?,
                    );
                }
                self.migration_change(&migration_id, |j, _| {
                    j.machines
                        .get_mut(&context.vm_id())
                        .context("target machine missing")?
                        .identities = identities;
                    Ok(())
                })?;
                Ok(proof)
            }
            HostMigrationRequest::Commit {
                migration_id,
                handoff,
            } => {
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Source
                        && matches!(
                            record.phase,
                            TransferPhase::Fenced | TransferPhase::Relinquished
                        ),
                    "source is not fenced for ownership commit"
                );
                verify_local_handoff(&record, &handoff)?;
                let context = self.migration_context(record.plan.source_vm_id, &migration_id)?;
                self.migration_native(&context, "reopen", Some(&record), json!({}))
                    .await?;
                self.migration_change(&migration_id, |_, r| {
                    if let Some(old) = &r.handoff {
                        ensure!(old == &handoff, "source ownership proof changed");
                    }
                    r.handoff = Some(handoff);
                    r.phase = TransferPhase::Relinquished;
                    Ok(json!(r))
                })
            }
            HostMigrationRequest::Activate {
                migration_id,
                handoff,
            } => {
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Destination,
                    "only target activates"
                );
                verify_local_handoff(&record, &handoff)?;
                if record.phase == TransferPhase::Complete {
                    ensure!(
                        record.handoff.as_ref() == Some(&handoff),
                        "activation proof changed"
                    );
                    return Ok(json!({"activated":true}));
                }
                ensure!(
                    matches!(record.phase, TransferPhase::Ready | TransferPhase::Accepted),
                    "target has no physical readiness proof"
                );
                if let Some(old) = &record.handoff {
                    ensure!(old == &handoff, "target ownership proof changed");
                }
                self.migration_change(&migration_id, |_, r| {
                    r.handoff = Some(handoff.clone());
                    r.phase = TransferPhase::Accepted;
                    r.intent = Some("activate".into());
                    Ok(())
                })?;
                let context = self.migration_context(record.plan.target_vm_id, &migration_id)?;
                let proof = self
                    .migration_native(&context, "activate", Some(&record), json!({}))
                    .await?;
                self.verify_machine(&context).await?;
                self.migration_change(&migration_id, |_, r| {
                    r.phase = TransferPhase::Complete;
                    r.native = Some(proof["native"].clone());
                    r.intent = None;
                    Ok(())
                })?;
                Ok(proof)
            }
            HostMigrationRequest::Inspect { migration_id } => {
                let record = self.migration_record(&migration_id)?;
                let mut value = json!(record);
                if record.role == TransferRole::Source
                    && matches!(
                        record.phase,
                        TransferPhase::Fenced | TransferPhase::Relinquished
                    )
                {
                    let context =
                        self.migration_context(record.plan.source_vm_id, &migration_id)?;
                    let actual = self
                        .migration_native(&context, "reopen", Some(&record), json!({}))
                        .await?;
                    value["inspection"] = json!({"fence_verified":true,"actual":actual});
                }
                Ok(value)
            }
            HostMigrationRequest::Cleanup {
                migration_id,
                confirm,
                handoff,
                acceptance,
            } => {
                ensure!(confirm, "explicit recovery deletion required");
                let record = self.migration_record(&migration_id)?;
                ensure!(
                    record.role == TransferRole::Source
                        && record.handoff.as_ref() == Some(&handoff),
                    "source recovery is not durably relinquished"
                );
                verify_local_handoff(&record, &handoff)?;
                acceptance.verify(&record.plan.destination.server_id, "accepted")?;
                ensure!(
                    acceptance.value.migration_id == migration_id
                        && acceptance.value.plan_sha256 == wire::digest(&record.plan)
                        && acceptance.value.handoff_sha256 == wire::digest(&handoff.value)
                        && acceptance.value.destination == record.plan.destination,
                    "destination acceptance mismatch"
                );
                if record.phase == TransferPhase::Complete {
                    return Ok(json!({"cleaned":true}));
                }
                let context = self.migration_context(record.plan.source_vm_id, &migration_id)?;
                let proof = if record.phase == TransferPhase::Cleaning
                    && record.intent.as_deref() == Some("cleanup")
                {
                    // Reconcile a completed lost acknowledgement using absence
                    // evidence, never by resubmitting the native destroy task.
                    let proof = self
                        .migration_native(&context, "absence", Some(&record), json!({}))
                        .await?;
                    self.migration_native(&context, "cleanup_metadata", Some(&record), json!({}))
                        .await?;
                    proof
                } else {
                    ensure!(
                        record.phase == TransferPhase::Relinquished,
                        "ambiguous recovery deletion cannot be blindly replayed"
                    );
                    self.migration_change(&migration_id, |_, r| {
                        r.phase = TransferPhase::Cleaning;
                        r.intent = Some("cleanup".into());
                        Ok(())
                    })?;
                    self.migration_close(&migration_id)?;
                    self.migration_native(&context, "cleanup", Some(&record), json!({}))
                        .await?
                };
                self.cleanup_migration_metadata(&context).await?;
                self.migration_change(&migration_id, |j, r| {
                    let vm = j
                        .machines
                        .get_mut(&r.plan.source_vm_id)
                        .context("source machine missing")?;
                    vm.state = MachineState::Deleted;
                    vm.delete_operation = Some(migration_id.clone());
                    r.phase = TransferPhase::Complete;
                    r.intent = None;
                    Ok(())
                })?;
                Ok(proof)
            }
            HostMigrationRequest::Abort {
                migration_id,
                confirm_cleanup,
                source_abort,
                plan,
            } => {
                ensure!(confirm_cleanup, "explicit transfer abort/cleanup required");
                let record = if let Some(record) = self.journal()?.migrations.get(&migration_id) {
                    record.clone()
                } else {
                    let plan =
                        plan.context("unknown migration abort requires its exact reviewed plan")?;
                    plan.validate()?;
                    ensure!(
                        plan.migration_id == migration_id,
                        "abort plan identity mismatch"
                    );
                    let role = if plan.source.environment == self.binding.environment {
                        TransferRole::Source
                    } else {
                        ensure!(
                            plan.destination.environment == self.binding.environment,
                            "abort addressed to another environment"
                        );
                        TransferRole::Destination
                    };
                    let vm_id = if role == TransferRole::Source {
                        plan.source_vm_id
                    } else {
                        plan.target_vm_id
                    };
                    let (snapshot, native) = self.migration_snapshot(vm_id).await?;
                    let machine = self.journal()?.machines[&vm_id].clone();
                    if role == TransferRole::Source {
                        ensure!(
                            snapshot.machine_sha256 == plan.source_machine_sha256
                                && snapshot.config_sha256 == plan.source_config_sha256,
                            "source changed since reviewed plan"
                        );
                    } else {
                        ensure!(
                            machine.create_operation
                                == crate::control::migration_transport::target_create_id(
                                    &migration_id
                                ),
                            "unprepared target lacks this migration's creation proof"
                        );
                        let proof = source_abort
                            .as_ref()
                            .context("target discard requires signed source cancellation")?;
                        proof.verify(&plan.source.server_id, "abort")?;
                        ensure!(
                            proof.value.migration_id == migration_id
                                && proof.value.plan_sha256 == wire::digest(&plan),
                            "source cancellation plan mismatch"
                        );
                    }
                    let mut record = HostTransferRecord {
                        plan,
                        role,
                        phase: if role == TransferRole::Source {
                            TransferPhase::Aborted
                        } else {
                            TransferPhase::Cleaning
                        },
                        machine_before: machine,
                        config_before: snapshot.config,
                        volumes_before: snapshot.volume_identities,
                        manifest: None,
                        offsets: BTreeMap::new(),
                        last_chunk: None,
                        target_identities: None,
                        handoff: None,
                        error: None,
                        native: Some(native),
                        intent: if role == TransferRole::Source {
                            None
                        } else {
                            Some("abort".into())
                        },
                    };
                    self.change(|j| {
                        j.schema = 6;
                        j.migrations.insert(migration_id.clone(), record.clone());
                        Ok(())
                    })?;
                    if role == TransferRole::Source {
                        return Ok(json!({"aborted":true,"source_restored":true}));
                    }
                    let context = self.migration_context(vm_id, &migration_id)?;
                    let proof = self
                        .migration_native(&context, "abort_unprepared", Some(&record), json!({}))
                        .await?;
                    self.migration_change(&migration_id, |j, r| {
                        let vm = j
                            .machines
                            .get_mut(&vm_id)
                            .context("target machine missing")?;
                        vm.state = MachineState::Deleted;
                        vm.delete_operation = Some(migration_id.clone());
                        r.phase = TransferPhase::Aborted;
                        r.intent = None;
                        Ok(())
                    })?;
                    record.phase = TransferPhase::Aborted;
                    return Ok(proof);
                };
                ensure!(
                    record.handoff.is_none()
                        && !matches!(
                            record.phase,
                            TransferPhase::Relinquished
                                | TransferPhase::Accepted
                                | TransferPhase::Complete
                        ),
                    "committed ownership cannot roll back"
                );
                if record.phase == TransferPhase::Aborted {
                    return Ok(json!({"aborted":true}));
                }
                if record.role == TransferRole::Destination {
                    let proof = source_abort
                        .as_ref()
                        .context("destination discard requires signed source cancellation")?;
                    proof.verify(&record.plan.source.server_id, "abort")?;
                    ensure!(
                        proof.value.migration_id == migration_id
                            && proof.value.plan_sha256 == wire::digest(&record.plan)
                            && proof.value.source == record.plan.source,
                        "source cancellation differs from local transfer"
                    );
                }
                if record.phase == TransferPhase::Cleaning
                    && record.intent.as_deref() == Some("abort")
                {
                    let vm_id = record.machine_before.vm_id;
                    let context = self.migration_context(vm_id, &migration_id)?;
                    let proof = if record.role == TransferRole::Source {
                        self.migration_native(&context, "restored", Some(&record), json!({}))
                            .await?
                    } else {
                        let proof = self
                            .migration_native(&context, "absence", Some(&record), json!({}))
                            .await?;
                        self.migration_native(
                            &context,
                            "cleanup_metadata",
                            Some(&record),
                            json!({}),
                        )
                        .await?;
                        proof
                    };
                    self.migration_change(&migration_id, |j, r| {
                        if r.role == TransferRole::Destination {
                            let vm = j
                                .machines
                                .get_mut(&vm_id)
                                .context("target machine missing")?;
                            vm.state = MachineState::Deleted;
                            vm.delete_operation = Some(migration_id.clone());
                        }
                        r.phase = TransferPhase::Aborted;
                        r.intent = None;
                        Ok(())
                    })?;
                    return Ok(proof);
                }
                if record.role == TransferRole::Source && record.phase == TransferPhase::Authorized
                {
                    let (snapshot, _) = self.migration_snapshot(record.plan.source_vm_id).await?;
                    ensure!(
                        snapshot.config == record.config_before
                            && snapshot.volume_identities == record.volumes_before,
                        "source changed before cancellation"
                    );
                    self.migration_change(&migration_id, |_, r| {
                        r.phase = TransferPhase::Aborted;
                        Ok(())
                    })?;
                    return Ok(json!({"aborted":true,"source_restored":true}));
                }
                ensure!(
                    matches!(
                        record.phase,
                        TransferPhase::Fenced | TransferPhase::Receiving | TransferPhase::Ready
                    ),
                    "ambiguous preparation needs explicit inspection before abort"
                );
                self.migration_change(&migration_id, |_, r| {
                    r.phase = TransferPhase::Cleaning;
                    r.intent = Some("abort".into());
                    Ok(())
                })?;
                self.migration_close(&migration_id)?;
                let vm_id = if record.role == TransferRole::Source {
                    record.plan.source_vm_id
                } else {
                    record.plan.target_vm_id
                };
                let context = self.migration_context(vm_id, &migration_id)?;
                let proof = self
                    .migration_native(&context, "abort", Some(&record), json!({}))
                    .await?;
                self.migration_change(&migration_id, |j, r| {
                    if r.role == TransferRole::Destination {
                        let vm = j
                            .machines
                            .get_mut(&r.plan.target_vm_id)
                            .context("target machine missing")?;
                        vm.state = MachineState::Deleted;
                        vm.delete_operation = Some(migration_id.clone());
                    }
                    r.phase = TransferPhase::Aborted;
                    r.intent = None;
                    Ok(())
                })?;
                Ok(proof)
            }
        }
    }
    /// Called only after ordinary deletion has proved the VM and both disks
    /// absent. Historical destination metadata remains bound to the original
    /// exclusive staging creation, even after later source-side migration.
    pub(super) async fn cleanup_migration_metadata(&self, context: &Context) -> Result<()> {
        let journal = self.journal()?;
        for record in journal.migrations.values().filter(|r| {
            r.role == TransferRole::Destination
                && r.phase == TransferPhase::Complete
                && r.plan.target_vm_id == context.vm_id()
        }) {
            ensure!(
                journal
                    .machines
                    .get(&context.vm_id())
                    .is_some_and(|m| m.create_operation
                        == crate::control::migration_transport::target_create_id(
                            &record.plan.migration_id
                        )),
                "metadata cleanup target creation changed"
            );
            self.migration_native(context, "cleanup_metadata", Some(record), json!({}))
                .await?;
        }
        Ok(())
    }
    fn migration_cache_key(&self, id: &str, slot: &str) -> CacheKey {
        (
            self.binding.environment.clone(),
            self.binding.pool_guid.clone(),
            id.into(),
            slot.into(),
        )
    }
    fn migration_close(&self, id: &str) -> Result<()> {
        cache()
            .lock()
            .map_err(|_| anyhow!("migration descriptor cache poisoned"))?
            .retain(|key, _| {
                !(key.0 == self.binding.environment
                    && key.1 == self.binding.pool_guid
                    && key.2 == id)
            });
        Ok(())
    }
    fn migration_file(&self, r: &HostTransferRecord, slot: &str) -> Result<Arc<OpenVolume>> {
        let key = self.migration_cache_key(&r.plan.migration_id, slot);
        let mut files = cache()
            .lock()
            .map_err(|_| anyhow!("migration descriptor cache poisoned"))?;
        if let Some(file) = files.get(&key) {
            file.check()?;
            return Ok(file.clone());
        }
        let native = r
            .native
            .as_ref()
            .context("attested native transfer devices missing")?;
        let device = native["devices"][slot].clone();
        let path = PathBuf::from(
            device["path"]
                .as_str()
                .context("attested device path missing")?,
        );
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "transfer device changed or became symlink"
        );
        let mut options = fs::OpenOptions::new();
        options.read(true);
        if r.role == TransferRole::Destination {
            options.write(true);
        }
        let file = options.open(&path)?;
        let info = file.metadata()?;
        if device["regular"] == true {
            ensure!(
                info.is_file()
                    && info.nlink() == 1
                    && info.uid() == 0
                    && info.dev().to_string() == device["guid"].as_str().unwrap_or_default()
                    && info.ino().to_string() == device["createtxg"].as_str().unwrap_or_default()
                    && info.len() == device["bytes"].as_u64().unwrap_or(0),
                "snippet inode/size/ownership changed"
            );
        } else {
            ensure!(
                info.file_type().is_block_device()
                    && info.rdev() == device["rdev"].as_u64().context("device identity missing")?,
                "migration block-device identity changed"
            );
        }
        let result = Arc::new(OpenVolume {
            modified: info.modified().ok(),
            file,
            config_path: PathBuf::from(
                native["configuration_file"]
                    .as_str()
                    .context("config path missing")?,
            ),
            config_sha256: native["configuration_file_sha256"]
                .as_str()
                .context("config hash missing")?
                .into(),
            vm_id: r.machine_before.vm_id,
            device,
            source: r.role == TransferRole::Source,
        });
        result.check()?;
        files.insert(key, result.clone());
        Ok(result)
    }
    async fn migration_prepare_file(
        &self,
        r: &HostTransferRecord,
        slot: &str,
    ) -> Result<Arc<OpenVolume>> {
        let key = self.migration_cache_key(&r.plan.migration_id, slot);
        let cached = {
            cache()
                .lock()
                .map_err(|_| anyhow!("migration cache poisoned"))?
                .contains_key(&key)
        };
        if cached {
            return self.migration_file(r, slot);
        }
        let context = self.migration_context(r.machine_before.vm_id, &r.plan.migration_id)?;
        let proof = self
            .migration_native(&context, "reopen", Some(r), json!({}))
            .await?;
        self.migration_change(&r.plan.migration_id, |_, record| {
            record.native = Some(proof["native"].clone());
            Ok(())
        })?;
        self.migration_file(&self.migration_record(&r.plan.migration_id)?, slot)
    }
    async fn migration_read(
        &self,
        r: &HostTransferRecord,
        slot: &str,
        offset: u64,
    ) -> Result<Value> {
        if let Backend::Simulated(world) = &self.backend {
            return simulated_read(world, r, slot, offset);
        }
        let file = self.migration_prepare_file(r, slot).await?;
        let size = slot_manifest(r, slot)?.bytes;
        tokio::task::spawn_blocking(move||{file.check()?;let mut data=vec![0u8;(size-offset).min(ZERO_EXTENT as u64) as usize];file.file.read_exact_at(&mut data,offset)?;
            let first=data.len().min(wire::CHUNK_BYTES);let zero=if data[..first].iter().all(|b|*b==0){let mut end=first;while end<data.len(){let next=(end+wire::CHUNK_BYTES).min(data.len());if data[end..next].iter().any(|b|*b!=0){break;}end=next;}Some(end)}else{None};
            let length=zero.unwrap_or(first);let sha=format!("{:x}",Sha256::digest(&data[..length]));Ok::<_,anyhow::Error>(json!({"bytes_base64":if zero.is_some(){String::new()}else{STANDARD.encode(&data[..length])},"sha256":sha,"zero_bytes":zero}))
        }).await?
    }
    async fn migration_write(
        &self,
        r: &HostTransferRecord,
        slot: &str,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<()> {
        if let Backend::Simulated(world) = &self.backend {
            return simulated_write(world, r, slot, offset, &data);
        }
        let file = self.migration_prepare_file(r, slot).await?;
        tokio::task::spawn_blocking(move || {
            file.check()?;
            file.file.write_all_at(&data, offset)?;
            file.file.sync_data()?;
            file.check()?;
            Ok(())
        })
        .await?
    }
    async fn migration_verify_chunk(
        &self,
        r: &HostTransferRecord,
        slot: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        if let Backend::Simulated(world) = &self.backend {
            let observed = simulated_bytes(world, r, slot, offset, data.len())?;
            ensure!(
                observed == data,
                "replayed chunk differs from committed bytes"
            );
            return Ok(());
        }
        let file = self.migration_prepare_file(r, slot).await?;
        let wanted = data.to_vec();
        tokio::task::spawn_blocking(move || {
            file.check()?;
            let mut observed = vec![0u8; wanted.len()];
            file.file.read_exact_at(&mut observed, offset)?;
            ensure!(
                observed == wanted,
                "replayed chunk differs from committed bytes"
            );
            Ok(())
        })
        .await?
    }
}
impl OpenVolume {
    fn check(&self) -> Result<()> {
        ensure!(
            !Path::new(&format!("/run/qemu-server/{}.pid", self.vm_id)).exists(),
            "staging/source VM unexpectedly started"
        );
        ensure!(
            format!("{:x}", Sha256::digest(fs::read(&self.config_path)?)) == self.config_sha256,
            "configuration changed during chunk transfer"
        );
        let info = self.file.metadata()?;
        if self.device["regular"] == true {
            ensure!(
                info.len() == self.device["bytes"].as_u64().unwrap_or(0)
                    && (!self.source || info.modified().ok() == self.modified),
                "snippet changed during transfer"
            );
        } else {
            ensure!(
                info.rdev() == self.device["rdev"].as_u64().unwrap_or(0),
                "open block-device identity changed"
            );
        }
        Ok(())
    }
}
fn slot_manifest<'a>(r: &'a HostTransferRecord, slot: &str) -> Result<&'a VolumeManifest> {
    let m = r
        .manifest
        .as_ref()
        .context("final stopped manifest missing")?;
    m.volumes
        .get(slot)
        .or_else(|| m.snippets.get(slot))
        .context("slot is outside immutable manifest")
}
fn verify_local_handoff(r: &HostTransferRecord, h: &wire::Signed<wire::Handoff>) -> Result<()> {
    h.verify(&r.plan.source.server_id, "handoff")?;
    ensure!(
        h.value.migration_id == r.plan.migration_id
            && h.value.plan_sha256 == wire::digest(&r.plan)
            && h.value.source == r.plan.source
            && h.value.destination == r.plan.destination
            && r.manifest
                .as_ref()
                .is_some_and(|m| h.value.manifest_sha256 == wire::digest(m)),
        "handoff does not match locally attested plan/manifest"
    );
    Ok(())
}

// Simulated disk bytes are sparse: an optional source prefix and only nonzero
// received chunks are persisted. Physical adapters always read/hash real devices.
fn sim_name(r: &HostTransferRecord, slot: &str) -> String {
    let root = r
        .native
        .as_ref()
        .and_then(|n| n["volume_names"][slot].as_str())
        .unwrap_or("");
    root.into()
}
fn simulated_bytes(
    world: &SimulatedWorld,
    r: &HostTransferRecord,
    slot: &str,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let w = world.load()?;
    let name = sim_name(r, slot);
    let props = w
        .datasets
        .get(&name)
        .context("simulated migration volume disappeared")?;
    let mut out = vec![0u8; length];
    if let Some(prefix) = props.get("migration_data") {
        let bytes = STANDARD.decode(prefix)?;
        if offset < bytes.len() as u64 {
            let count = (bytes.len() - offset as usize).min(length);
            out[..count].copy_from_slice(&bytes[offset as usize..offset as usize + count]);
        }
    }
    for (key, value) in props.range("migration_chunk_".to_string()..) {
        if !key.starts_with("migration_chunk_") {
            break;
        }
        let start = key.trim_start_matches("migration_chunk_").parse::<u64>()?;
        let bytes = STANDARD.decode(value)?;
        let end = start + bytes.len() as u64;
        let from = start.max(offset);
        let to = end.min(offset + length as u64);
        if from < to {
            out[(from - offset) as usize..(to - offset) as usize]
                .copy_from_slice(&bytes[(from - start) as usize..(to - start) as usize]);
        }
    }
    Ok(out)
}
fn simulated_read(
    world: &SimulatedWorld,
    r: &HostTransferRecord,
    slot: &str,
    offset: u64,
) -> Result<Value> {
    let size = slot_manifest(r, slot)?.bytes;
    let data = simulated_bytes(
        world,
        r,
        slot,
        offset,
        (size - offset).min(ZERO_EXTENT as u64) as usize,
    )?;
    let first = data.len().min(wire::CHUNK_BYTES);
    let zero = if data[..first].iter().all(|b| *b == 0) {
        let mut end = first;
        while end < data.len() {
            let next = (end + wire::CHUNK_BYTES).min(data.len());
            if data[end..next].iter().any(|b| *b != 0) {
                break;
            }
            end = next;
        }
        Some(end)
    } else {
        None
    };
    let length = zero.unwrap_or(first);
    Ok(
        json!({"bytes_base64":if zero.is_some(){String::new()}else{STANDARD.encode(&data[..length])},"sha256":format!("{:x}",Sha256::digest(&data[..length])),"zero_bytes":zero}),
    )
}
fn simulated_write(
    world: &SimulatedWorld,
    r: &HostTransferRecord,
    slot: &str,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    let name = sim_name(r, slot);
    world.edit(|w| {
        let p = w
            .datasets
            .get_mut(&name)
            .context("simulated staging disappeared")?;
        if data.iter().any(|b| *b != 0) {
            p.insert(
                format!("migration_chunk_{offset:020}"),
                STANDARD.encode(data),
            );
        }
        Ok(())
    })
}
impl Host {
    fn migration_simulate(
        &self,
        world: &SimulatedWorld,
        context: &Context,
        action: &str,
        record: Option<&HostTransferRecord>,
        extra: Value,
    ) -> Result<Value> {
        let vm_id = context.vm_id();
        let names = BTreeMap::from([
            ("root".to_string(), context.root_volume()),
            ("cloudinit".to_string(), context.cloud_init_volume()),
        ]);
        if action == "snapshot" {
            let w = world.load()?;
            let vm = w.vms.get(&vm_id).context("VM absent")?;
            ensure!(
                !vm.running && !vm.config.contains_key("lock"),
                "VM running or locked"
            );
            let mut volumes = BTreeMap::new();
            for (slot, name) in &names {
                let p = w.datasets.get(name).context("volume absent")?;
                let mut v = BTreeMap::new();
                for k in KEYS {
                    v.insert(
                        k.to_string(),
                        p.get(*k).cloned().unwrap_or_else(|| {
                            if *k == "readonly" {
                                "off".into()
                            } else {
                                "-".into()
                            }
                        }),
                    );
                }
                volumes.insert(slot.clone(), v);
            }
            return Ok(
                json!({"config":normalize(&json!(vm.config))?,"raw_config":vm.config,"volumes":volumes,"devices":{},"snippets":{},"volume_names":names}),
            );
        }
        let r = record.context("simulated migration record missing")?;
        match action {
            "absence" | "cleanup_metadata" => {
                let w = world.load()?;
                ensure!(!w.vms.contains_key(&vm_id), "VM still exists");
                for (name, p) in &w.datasets {
                    ensure!(
                        !names.values().any(|n| n == name)
                            && !r
                                .volumes_before
                                .values()
                                .any(|v| p.get("guid").map(String::as_str) == v["guid"].as_str()),
                        "owned volume identity still exists"
                    );
                }
                Ok(json!({"vm_absent":true,"volumes_absent":true,"metadata_absent":true}))
            }
            "restored" => {
                let w = world.load()?;
                let vm = w.vms.get(&vm_id).context("source absent")?;
                ensure!(
                    !vm.running && normalize(&json!(vm.config))? == r.config_before,
                    "source configuration not restored"
                );
                for (slot, name) in &names {
                    let p = w.datasets.get(name).context("source volume absent")?;
                    for key in KEYS {
                        ensure!(
                            p.get(*key)
                                .map(String::as_str)
                                .unwrap_or(if *key == "readonly" { "off" } else { "-" })
                                == r.volumes_before[slot][key].as_str().unwrap_or("-"),
                            "source volume properties not restored"
                        );
                    }
                }
                Ok(json!({"aborted":true,"source_restored":true}))
            }
            "verify_sealed" => {
                let w = world.load()?;
                let vm = w.vms.get(&vm_id).context("target absent")?;
                let old = r
                    .target_identities
                    .as_ref()
                    .context("sealed target proof missing")?;
                ensure!(
                    !vm.running
                        && vm.config.get("lock").map(String::as_str) == Some("migrate")
                        && json!(normalize(&json!(vm.config))?) == old["config"],
                    "sealed target configuration/fence changed"
                );
                for (slot, v) in &r.manifest.as_ref().context("manifest missing")?.volumes {
                    ensure!(
                        json!(w.datasets[&names[slot]]) == old["volumes"][slot],
                        "sealed volume identity changed"
                    );
                    let mut h = Sha256::new();
                    let mut off = 0;
                    while off < v.bytes {
                        let bytes = simulated_bytes(
                            world,
                            r,
                            slot,
                            off,
                            (v.bytes - off).min(ZERO_EXTENT as u64) as usize,
                        )?;
                        h.update(&bytes);
                        off += bytes.len() as u64;
                    }
                    ensure!(
                        format!("{:x}", h.finalize()) == v.sha256,
                        "sealed target contents changed"
                    );
                }
                Ok(old.clone())
            }
            "reopen" => {
                let w = world.load()?;
                let vm = w.vms.get(&vm_id).context("VM absent")?;
                let mut expected = r.config_before.clone();
                expected.insert("onboot".into(), "0".into());
                ensure!(
                    !vm.running
                        && vm.config.get("lock").map(String::as_str) == Some("migrate")
                        && normalize(&json!(vm.config))? == expected,
                    "simulated migration fence/configuration changed"
                );
                for (slot, name) in &names {
                    let p = w.datasets.get(name).context("volume absent")?;
                    for key in [
                        "guid",
                        "createtxg",
                        "volsize",
                        "refreservation",
                        "origin",
                        "org.pve-dev:owner",
                        "org.pve-dev:image",
                    ] {
                        ensure!(
                            p.get(key).map(String::as_str).unwrap_or("-")
                                == r.volumes_before[slot][key].as_str().unwrap_or("-"),
                            "simulated migration volume identity changed"
                        );
                    }
                    ensure!(
                        p.get("readonly").map(String::as_str)
                            == Some(if r.role == TransferRole::Source {
                                "on"
                            } else {
                                "off"
                            })
                            && p.get("org.pve-dev:migration") == Some(&r.plan.migration_id),
                        "simulated migration volume fence changed"
                    );
                }
                Ok(json!({"native":r.native}))
            }
            "prepare_target" | "fence" => {
                let source = action == "fence";
                world.edit(|w| {
                    let vm = w.vms.get_mut(&vm_id).context("VM absent")?;
                    ensure!(
                        !vm.running && normalize(&json!(vm.config))? == r.config_before,
                        "VM changed before migration"
                    );
                    vm.config.insert("lock".into(), "migrate".into());
                    vm.config.insert("onboot".into(), "0".into());
                    for name in names.values() {
                        let v = w.datasets.get_mut(name).context("volume absent")?;
                        v.insert("readonly".into(), if source { "on" } else { "off" }.into());
                        v.insert("org.pve-dev:migration".into(), r.plan.migration_id.clone());
                    }
                    Ok(())
                })?;
                let w = world.load()?;
                let config = normalize(&json!(w.vms[&vm_id].config))?;
                let native = json!({"config":config,"volumes":names.iter().map(|(s,n)|(s.clone(),w.datasets[n].clone())).collect::<BTreeMap<_,_>>(),"devices":{},"volume_names":names});
                if !source {
                    return Ok(json!({"native":native}));
                }
                let mut volumes = BTreeMap::new();
                for (slot, name) in &names {
                    let p = &w.datasets[name];
                    let bytes = p["volsize"].parse::<u64>()?;
                    let prefix = p
                        .get("migration_data")
                        .map(|v| STANDARD.decode(v))
                        .transpose()?
                        .unwrap_or_default();
                    volumes.insert(
                        slot.clone(),
                        VolumeManifest {
                            bytes,
                            sha256: data_digest(&prefix, bytes),
                            guid: p["guid"].clone(),
                            createtxg: p["createtxg"].clone(),
                        },
                    );
                }
                let manifest = Manifest {
                    plan_sha256: wire::digest(&r.plan),
                    source_fence: json!({"migration_id":r.plan.migration_id,"lock":"migrate","stopped":true}),
                    config: r.config_before.clone(),
                    volumes,
                    snippets: BTreeMap::new(),
                };
                Ok(json!({"manifest":manifest,"native":native}))
            }
            "seal" => {
                let manifest: Manifest = serde_json::from_value(extra["manifest"].clone())?;
                for (slot, v) in &manifest.volumes {
                    let mut h = Sha256::new();
                    let mut off = 0;
                    while off < v.bytes {
                        let bytes = simulated_bytes(
                            world,
                            r,
                            slot,
                            off,
                            (v.bytes - off).min(ZERO_EXTENT as u64) as usize,
                        )?;
                        h.update(&bytes);
                        off += bytes.len() as u64;
                    }
                    ensure!(
                        format!("{:x}", h.finalize()) == v.sha256,
                        "simulated target data hash mismatch"
                    );
                }
                world.edit(|w| {
                    let mut config = manifest.config.clone();
                    config.insert("description".into(), context.vm_marker());
                    config.insert(
                        "scsi0".into(),
                        format!(
                            "{}:{},size={}M",
                            context.storage(),
                            context.root_name(),
                            r.plan.disk_mib
                        ),
                    );
                    config.insert(
                        "ide2".into(),
                        format!(
                            "{}:{},media=cdrom",
                            context.storage(),
                            context.cloud_init_name()
                        ),
                    );
                    config.insert("onboot".into(), "0".into());
                    config.insert("lock".into(), "migrate".into());
                    w.vms.get_mut(&vm_id).context("VM absent")?.config = config;
                    for name in names.values() {
                        w.datasets
                            .get_mut(name)
                            .context("volume missing")?
                            .insert("readonly".into(), "on".into());
                    }
                    if let Some(guest) = &r.plan.guest {
                        w.datasets
                            .get_mut(&names["root"])
                            .context("root missing")?
                            .insert("org.pve-dev:image".into(), guest.image_sha256.clone());
                    }
                    Ok(())
                })?;
                let w = world.load()?;
                Ok(
                    json!({"config":normalize(&json!(w.vms[&vm_id].config))?,"native":{"volume_names":names},"volumes":names.iter().map(|(s,n)|(s.clone(),w.datasets[n].clone())).collect::<BTreeMap<_,_>>(),"source_manifest":wire::digest(&manifest)}),
                )
            }
            "activate" => {
                world.edit(|w| {
                    let vm = w.vms.get_mut(&vm_id).context("VM absent")?;
                    ensure!(!vm.running, "target is already running");
                    vm.config.remove("lock");
                    for name in names.values() {
                        w.datasets
                            .get_mut(name)
                            .context("volume missing")?
                            .insert("readonly".into(), "off".into());
                    }
                    Ok(())
                })?;
                Ok(json!({"activated":true,"native":{"volume_names":names}}))
            }
            "cleanup" | "abort" | "abort_unprepared" => {
                if action == "abort_unprepared" {
                    world.edit(|w| {
                        let vm = w.vms.get_mut(&vm_id).context("target missing")?;
                        ensure!(
                            !vm.running && normalize(&json!(vm.config))? == r.config_before,
                            "unprepared target changed"
                        );
                        vm.config.insert("lock".into(), "migrate".into());
                        vm.config.insert("onboot".into(), "0".into());
                        Ok(())
                    })?;
                }
                let abort = action == "abort";
                if abort && r.role == TransferRole::Source {
                    world.edit(|w| {
                        let vm = w.vms.get_mut(&vm_id).context("VM absent")?;
                        ensure!(!vm.running, "source running");
                        vm.config = r.config_before.clone();
                        for name in names.values() {
                            let p = w.datasets.get_mut(name).context("volume absent")?;
                            p.insert("readonly".into(), "off".into());
                            p.remove("org.pve-dev:migration");
                        }
                        Ok(())
                    })?;
                    return Ok(json!({"aborted":true}));
                }
                world.edit(|w| {
                    let vm = w.vms.get(&vm_id).context("VM absent")?;
                    ensure!(
                        !vm.running && vm.config.get("lock").map(String::as_str) == Some("migrate"),
                        "cleanup VM is running/unfenced"
                    );
                    for name in names.values() {
                        let p = w.datasets.get(name).context("volume absent")?;
                        let slot = names
                            .iter()
                            .find_map(|(s, n)| (n == name).then_some(s))
                            .expect("slot");
                        ensure!(
                            p["guid"]
                                == r.volumes_before[slot]["guid"]
                                    .as_str()
                                    .context("volume guid missing")?,
                            "cleanup identity changed"
                        );
                    }
                    w.vms.remove(&vm_id);
                    for name in names.values() {
                        w.datasets.remove(name);
                    }
                    Ok(())
                })?;
                Ok(json!({"cleaned":true}))
            }
            _ => bail!("unsupported simulated migration action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_plan(environment: &str) -> wire::Plan {
        wire::Plan {
            schema: 1,
            migration_id: "a".repeat(64),
            source: wire::Peer {
                server_id: iroh::SecretKey::generate().public().to_string(),
                environment: "1".repeat(32),
            },
            destination: wire::Peer {
                server_id: iroh::SecretKey::generate().public().to_string(),
                environment: environment.into(),
            },
            source_vm_id: 9001,
            target_vm_id: 9002,
            source_owner: "alice".into(),
            target_owner: "bob".into(),
            source_machine_sha256: "b".repeat(64),
            source_config_sha256: "c".repeat(64),
            name: "test-guest".into(),
            disk_mib: 64,
            memory_mib: 512,
            min_memory_mib: 128,
            cores: 1,
            guest: None,
            byte_ceiling: 68 * MIB,
            target_bridge: "vmbr77".into(),
        }
    }
    #[test]
    fn prepare_rpc_has_long_deadline_while_inspections_and_chunks_stay_bounded() {
        let environment = "2".repeat(32);
        let requests = [
            HostMigrationRequest::Prepare {
                plan: test_plan(&environment),
                role: TransferRole::Destination,
                manifest: None,
            },
            HostMigrationRequest::Fence {
                migration_id: "a".repeat(64),
            },
            HostMigrationRequest::Abort {
                migration_id: "a".repeat(64),
                confirm_cleanup: true,
                source_abort: None,
                plan: None,
            },
        ];
        for request in requests {
            let request = HostRequest::Migration {
                environment: environment.clone(),
                request: Box::new(request),
            };
            assert_eq!(
                helper_exchange_timeout(&request),
                Duration::from_secs(72 * 3600)
            );
        }
        for request in [
            HostMigrationRequest::InspectVm { vm_id: 9002 },
            HostMigrationRequest::ReadChunk {
                migration_id: "a".repeat(64),
                slot: "root".into(),
                offset: 0,
            },
        ] {
            let request = HostRequest::Migration {
                environment: environment.clone(),
                request: Box::new(request),
            };
            assert_eq!(helper_exchange_timeout(&request), Duration::from_secs(30));
        }
        assert_eq!(
            helper_exchange_timeout(&HostRequest::Health),
            Duration::from_secs(30)
        );
    }
    #[tokio::test]
    async fn failed_native_seal_retains_first_diagnosis_and_never_replays_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let environment = "2".repeat(32);
        let state_dir = directory.path().join("helper");
        let world = directory.path().join("world.json");
        let host = Host::open(HostConfig::simulated(
            state_dir.clone(),
            &environment,
            world.clone(),
        ))
        .await
        .unwrap();
        let plan = test_plan(&environment);
        let id = plan.migration_id.clone();
        let machine = MachineRecord {
            vm_id: 9002,
            owner: "bob".into(),
            profile: profile(&plan.vm(TransferRole::Destination)),
            create_operation: crate::control::migration_transport::target_create_id(&id),
            delete_operation: None,
            state: MachineState::Present,
            identities: BTreeMap::new(),
            pinned_config: None,
        };
        let record = HostTransferRecord {
            plan,
            role: TransferRole::Destination,
            phase: TransferPhase::Receiving,
            machine_before: machine,
            config_before: BTreeMap::new(),
            volumes_before: BTreeMap::new(),
            manifest: None,
            offsets: BTreeMap::new(),
            last_chunk: None,
            target_identities: None,
            handoff: None,
            error: None,
            native: None,
            intent: None,
        };
        host.change(|journal| {
            journal.schema = 6;
            journal.migrations.insert(id.clone(), record);
            Ok(())
        })
        .unwrap();
        let native_effect = directory.path().join("native-effect");
        let error = host
            .migration_seal_effect(&id, async {
                fs::write(&native_effect, b"first native effect")?;
                bail!("injected pvesh failure: specific storage diagnosis")
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("specific storage diagnosis"));
        let record = host.migration_record(&id).unwrap();
        assert_eq!(record.intent.as_deref(), Some("seal"));
        assert_eq!(
            record.error.as_deref(),
            Some("injected pvesh failure: specific storage diagnosis")
        );
        drop(host);
        let host = Host::open(HostConfig::simulated(state_dir, &environment, world))
            .await
            .unwrap();
        for _ in 0..2 {
            let error = host
                .migration_seal_effect(&id, async {
                    fs::write(&native_effect, b"UNSAFE REPLAY")?;
                    Ok(json!({"unexpected":true}))
                })
                .await
                .unwrap_err();
            assert!(error.to_string().contains("ambiguous target seal intent"));
            assert!(error.to_string().contains("specific storage diagnosis"));
            assert_eq!(fs::read(&native_effect).unwrap(), b"first native effect");
        }
        assert_eq!(host.migration_record(&id).unwrap().error, record.error);
    }
}
