//! Existing resources use the same durable lifecycle plans, with independently
//! pinned allocation/configuration identities instead of creation markers.
use super::*;
use crate::control::JobKind;
use crate::control::adoption::{DATASET_PROPERTIES, VOLUME_PROPERTIES};

/// Bytes an emptied zfspool storage filesystem may still account for itself
/// (`usedbydataset`): metadata only. More than this means files were written
/// into it, which retirement must not destroy.
const RETIRE_EMPTY_LIMIT: u64 = 16 * MIB;

impl Host {
    pub(super) async fn inspect_adopted_resources(&self) -> Result<Value> {
        let journal = self.journal()?;
        let managed = self
            .binding
            .managed
            .as_ref()
            .context("managed binding required")?;
        let boot_id = self.backend.facts().await?.boot_id;
        let mut checks = Vec::new();
        for adopted in &managed.adopted_vms {
            let machine = &journal.machines[&adopted.vm.vm_id];
            if journal.jobs.values().any(|job| {
                job.job
                    .vm
                    .as_ref()
                    .is_some_and(|vm| vm.vm_id == adopted.vm.vm_id)
                    && !job.status.is_terminal()
            }) {
                checks.push(json!({"vm_id":adopted.vm.vm_id,"status":"pending"}));
                continue;
            }
            let owner = journal
                .owners
                .get(&adopted.vm.owner)
                .context("adopted owner history missing")?;
            let context = Context {
                environment: self.binding.environment.clone(),
                operation_id: "inventory".into(),
                kind: JobKind::Inspect,
                owner: owner.user_id.clone(),
                budget_mib: owner.budget_mib,
                vm: Some(adopted.vm.clone()),
                boot_id: boot_id.clone(),
                policy_revision: owner.policy_revision,
                owner_policy: Self::owner_policy(&journal, owner)?,
                policy_change: None,
                scope: self.binding.namespace(),
                managed: Some(managed.clone()),
                disk_shrink: None,
                extension: None,
            };
            // A deleted adopted VM keeps its dataset and storage entry until
            // a retirement removes them; neither is required to exist here.
            if machine.state == MachineState::Deleted {
                ensure!(
                    !self.vm_present(&context).await?,
                    "deleted adopted VMID reappeared"
                );
                let names = self.backend.zfs_names().await?;
                ensure!(
                    !names.contains(&adopted.root_volume())
                        && !names.contains(&adopted.cloud_init_volume()),
                    "deleted adopted volumes reappeared"
                );
            } else {
                self.verify_machine(&context).await?;
            }
            checks.push(
                json!({"vm_id":adopted.vm.vm_id,"owner":adopted.vm.owner,"status":"verified",
                "guest_or_storage_effects":false,"allocation_retired":machine.allocation_retired()}),
            );
        }
        Ok(json!(checks))
    }

    pub(super) async fn verify_adopted_allocation(&self, context: &Context) -> Result<()> {
        let adopted = context.adopted().context("adopted allocation missing")?;
        ensure!(
            !self
                .journal()?
                .machines
                .get(&adopted.vm.vm_id)
                .is_some_and(MachineRecord::allocation_retired),
            "adopted allocation was retired; its dataset and storage no longer exist"
        );
        let dataset = self
            .backend
            .zfs_get(&adopted.dataset, DATASET_PROPERTIES)
            .await?;
        ensure!(
            dataset == adopted.dataset_properties,
            "adopted allocation identity or reservation drifted"
        );
        let mut storage = self
            .backend
            .api_get(&format!("/storage/{}", adopted.storage), &[])
            .await?;
        if let Some(map) = storage.as_object_mut() {
            map.remove("digest");
        }
        ensure!(
            storage == adopted.storage_configuration,
            "adopted storage configuration drifted"
        );
        Ok(())
    }

    pub(super) async fn verify_adopted_volumes(&self, context: &Context) -> Result<()> {
        let adopted = context
            .adopted()
            .context("adopted volume binding missing")?;
        if let Backend::Simulated(world) = &self.backend {
            ensure!(
                world
                    .load()?
                    .vms
                    .get(&context.vm_id())
                    .context("adopted VM missing")?
                    .pending_config
                    .is_empty(),
                "adopted VM has unapplied configuration changes"
            );
        } else {
            let pending = self
                .backend
                .api_get(&format!("{}/pending", context.vm_path()), &[])
                .await?;
            ensure!(
                pending.as_array().is_some_and(|rows| rows
                    .iter()
                    .all(|row| row.get("pending").is_none() && row.get("delete").is_none())),
                "adopted VM has unapplied or unrecognized configuration changes"
            );
        }
        for (path, expected) in &adopted.external_files_sha256 {
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_file()
                    && metadata.uid() == 0
                    && metadata.mode() & 0o022 == 0
                    && metadata.len() <= 1024 * 1024
                    && fs::canonicalize(path)? == Path::new(path),
                "cloud-init source is not a bounded protected regular file"
            );
            let actual = format!("{:x}", Sha256::digest(tokio::fs::read(path).await?));
            ensure!(
                &actual == expected,
                "cloud-init source changed; review its SSH/network policy before lifecycle changes"
            );
        }
        for (name, expected) in [
            (adopted.root_volume(), &adopted.root_properties),
            (adopted.cloud_init_volume(), &adopted.cloud_init_properties),
        ] {
            let observed = self.backend.zfs_get(&name, VOLUME_PROPERTIES).await?;
            ensure!(
                &observed == expected,
                "adopted disk identity, size or reservation drifted: {name}"
            );
        }
        Ok(())
    }

    pub(super) async fn verify_adopted_deletion(&self, context: &Context) -> Result<()> {
        let adopted = context
            .adopted()
            .context("adopted deletion binding missing")?;
        // Never recursively destroy retained recovery snapshots or other users'
        // resources as a side effect of a VM delete.
        let references = self.backend.owner_snapshots(&adopted.dataset).await?;
        ensure!(
            references.is_empty(),
            "allocation retains snapshots/bookmarks; review these explicitly before VM deletion"
        );
        let expected = BTreeSet::from([
            adopted.dataset.clone(),
            adopted.root_volume(),
            adopted.cloud_init_volume(),
        ]);
        ensure!(
            self.backend
                .zfs_names()
                .await?
                .iter()
                .filter(|name| *name == &adopted.dataset
                    || name.starts_with(&(adopted.dataset.clone() + "/")))
                .all(|name| expected.contains(name)),
            "adopted allocation contains additional resources; refusing deletion"
        );
        Ok(())
    }

    /// Recheck every precondition of retiring a deleted adopted VM's
    /// allocation and return `(storage_present, dataset_present)`.
    ///
    /// Runs before each step and again immediately before each effect, so a
    /// change made by anyone between steps (a new snapshot, a reused storage
    /// ID, a guest that references the storage) refuses the next effect.
    pub(super) async fn verify_retirement_state(&self, context: &Context) -> Result<(bool, bool)> {
        let adopted = context
            .adopted()
            .context("storage retirement requires an adopted allocation")?;
        let journal = self.journal()?;
        let machine = journal
            .machines
            .get(&adopted.vm.vm_id)
            .context("adopted VM is missing from helper history")?;
        ensure!(
            machine.state == MachineState::Deleted,
            "adopted VM {} is {:?}; only a verified deletion leaves an allocation to retire",
            adopted.vm.vm_id,
            machine.state
        );
        ensure!(
            machine
                .retirement
                .as_ref()
                .is_none_or(|r| r.operation_id == context.operation_id),
            "another operation already owns this allocation's retirement"
        );
        ensure!(
            !journal.jobs.values().any(|job| {
                job.job.operation_id != context.operation_id
                    && job
                        .job
                        .vm
                        .as_ref()
                        .is_some_and(|vm| vm.vm_id == adopted.vm.vm_id)
                    && !job.status.is_terminal()
            }),
            "VM {} has other unfinished helper work",
            adopted.vm.vm_id
        );
        let managed = context
            .managed
            .as_ref()
            .context("managed binding required")?;
        ensure!(
            !managed.adopted_vms.iter().any(|other| {
                other.vm.vm_id != adopted.vm.vm_id
                    && (other.storage == adopted.storage
                        || other.dataset == adopted.dataset
                        || other.dataset.starts_with(&format!("{}/", adopted.dataset)))
            }) && !journal
                .owners
                .keys()
                .chain(managed.scope.owners.iter())
                .any(|owner| format!("{}{owner}", managed.scope.storage_prefix) == adopted.storage),
            "another managed binding shares this storage or dataset; preserve it"
        );
        let job = &journal.jobs[&context.operation_id];
        let phase = |step| {
            job.steps
                .iter()
                .find(|record| record.step == step)
                .map(|record| record.phase)
                .ok_or_else(|| anyhow!("retirement plan missing {}", step.name()))
        };
        ensure!(
            !self.vm_present(context).await?,
            "VMID {} exists again; refusing to retire its allocation",
            adopted.vm.vm_id
        );
        let names = self.backend.zfs_names().await?;
        ensure!(
            !names.contains(&adopted.root_volume())
                && !names.contains(&adopted.cloud_init_volume()),
            "adopted volumes are present; the VM deletion did not finish"
        );
        let dataset_present = names.contains(&adopted.dataset);
        let prefix = format!("{}/", adopted.dataset);
        ensure!(
            !names.iter().any(|name| name.starts_with(&prefix)),
            "allocation dataset has child datasets; refusing (retirement never destroys recursively)"
        );
        match phase(Step::DestroyDataset)? {
            Phase::Planned => ensure!(
                dataset_present,
                "allocation dataset disappeared before its removal intent"
            ),
            Phase::Verified => ensure!(
                !dataset_present,
                "allocation dataset reappeared after its verified removal"
            ),
            Phase::Intent => {}
        }
        if dataset_present {
            ensure!(
                self.backend
                    .zfs_get(&adopted.dataset, DATASET_PROPERTIES)
                    .await?
                    == adopted.dataset_properties,
                "allocation dataset identity or reservation drifted"
            );
            ensure!(
                self.backend
                    .owner_snapshots(&adopted.dataset)
                    .await?
                    .is_empty(),
                "allocation dataset has snapshots or bookmarks; remove them explicitly first"
            );
            // A zfspool storage dataset holds only zvols, which were checked
            // absent above. Data written directly into the filesystem would be
            // lost with it, so refuse anything beyond an empty filesystem.
            let used = self
                .backend
                .zfs_get(&adopted.dataset, &["usedbydataset"])
                .await?;
            ensure!(
                used["usedbydataset"]
                    .parse::<u64>()
                    .is_ok_and(|bytes| bytes <= RETIRE_EMPTY_LIMIT),
                "allocation dataset holds data of its own; refusing to destroy it"
            );
        }
        let storages = self.backend.api_get("/storage", &[]).await?;
        let storages = storages
            .as_array()
            .ok_or_else(|| anyhow!("storage inventory missing"))?;
        let mut storage_present = false;
        for storage in storages {
            let id = storage["storage"]
                .as_str()
                .ok_or_else(|| anyhow!("storage ID missing"))?;
            if id == adopted.storage {
                storage_present = true;
                continue;
            }
            if storage["type"] != "zfspool" {
                continue;
            }
            ensure!(
                valid_label(id, 128),
                "unsupported storage ID; inspect before retiring storage"
            );
            let config = self.backend.api_get(&format!("/storage/{id}"), &[]).await?;
            let pool = config["pool"]
                .as_str()
                .ok_or_else(|| anyhow!("ZFS storage pool binding missing"))?;
            ensure!(
                pool != adopted.dataset && !pool.starts_with(&prefix),
                "storage {id} also points at this allocation dataset; preserve it"
            );
        }
        match phase(Step::RemoveStorage)? {
            Phase::Planned => ensure!(
                storage_present,
                "allocation storage disappeared before its removal intent"
            ),
            Phase::Verified => ensure!(
                !storage_present,
                "allocation storage reappeared after its verified removal"
            ),
            Phase::Intent => {}
        }
        ensure!(
            dataset_present || !storage_present,
            "storage entry still exists while its dataset is absent"
        );
        if storage_present {
            let mut config = self
                .backend
                .api_get(&format!("/storage/{}", adopted.storage), &[])
                .await?;
            if let Some(map) = config.as_object_mut() {
                map.remove("digest");
            }
            ensure!(
                config == adopted.storage_configuration,
                "allocation storage configuration drifted"
            );
            let contents = self
                .backend
                .api_get(
                    &format!(
                        "/nodes/{}/storage/{}/content",
                        context.node(),
                        adopted.storage
                    ),
                    &[],
                )
                .await?;
            ensure!(
                contents.as_array().is_some_and(Vec::is_empty),
                "allocation storage still has content"
            );
        }
        let reference = format!("{}:", adopted.storage);
        let resources = self
            .backend
            .api_get("/cluster/resources", &[("type", "vm".into())])
            .await?;
        for resource in resources
            .as_array()
            .ok_or_else(|| anyhow!("VM inventory missing"))?
        {
            let vm_id = resource["vmid"]
                .as_u64()
                .ok_or_else(|| anyhow!("invalid VM identity in inventory"))?;
            let kind = text(&resource["type"]);
            let node = text(&resource["node"]);
            ensure!(
                matches!(kind.as_str(), "qemu" | "lxc") && valid_label(&node, 64),
                "unsupported guest in inventory; inspect before retiring storage"
            );
            for current in ["1", "0"] {
                let config = self
                    .backend
                    .api_get(
                        &format!("/nodes/{node}/{kind}/{vm_id}/config"),
                        &[("current", current.into())],
                    )
                    .await?;
                let config = config
                    .as_object()
                    .ok_or_else(|| anyhow!("guest configuration missing"))?;
                ensure!(
                    !config.values().any(|value| value
                        .as_str()
                        .is_some_and(|value| value.contains(&reference))),
                    "guest {vm_id} still references storage {}; preserve it",
                    adopted.storage
                );
            }
        }
        let pools = self.backend.api_get("/pools", &[]).await?;
        for pool in pools
            .as_array()
            .ok_or_else(|| anyhow!("resource pool inventory missing"))?
        {
            let id = pool["poolid"]
                .as_str()
                .ok_or_else(|| anyhow!("resource pool ID missing"))?;
            ensure!(
                valid_label(id, 64),
                "unsupported resource pool identity; inspect before retiring storage"
            );
            let config = self.backend.api_get(&format!("/pools/{id}"), &[]).await?;
            let members = config["members"]
                .as_array()
                .ok_or_else(|| anyhow!("pool members missing"))?;
            ensure!(
                !members
                    .iter()
                    .any(|member| member["storage"] == adopted.storage
                        || member["id"] == format!("storage/{}", adopted.storage)),
                "resource pool {id} still references storage {}; detach it explicitly",
                adopted.storage
            );
        }
        Ok((storage_present, dataset_present))
    }

    /// One step of [`JobKind::RetireAllocation`]: a journal-only verification,
    /// removal of the Proxmox storage entry, a non-recursive destroy of the
    /// emptied dataset, and a journal-only release.
    pub(super) async fn run_retire_step(
        &self,
        context: &Context,
        step: Step,
        record: &StepRecord,
    ) -> Result<()> {
        let state = self.verify_retirement_state(context).await?;
        match step {
            Step::RetireVerify => {
                if record.phase != Phase::Verified {
                    self.backend.trip(step, FaultPoint::BeforeEffect)?;
                    self.update_step(context, step, |journal, record| {
                        let machine = journal
                            .machines
                            .get_mut(&context.vm_id())
                            .ok_or_else(|| anyhow!("adopted VM history missing"))?;
                        machine.retirement = Some(Retirement {
                            operation_id: context.operation_id.clone(),
                            retired: false,
                        });
                        record.phase = Phase::Verified;
                        record.boot_id = Some(context.boot_id.clone());
                        record.identity = Some(json!({"retirable": true}));
                        Ok(())
                    })?;
                    self.backend.trip(step, FaultPoint::AfterEffect)?;
                }
                return Ok(());
            }
            Step::RetireRelease => {
                ensure!(
                    !state.0 && !state.1,
                    "allocation storage or dataset is not absent; retirement is unfinished"
                );
                if record.phase != Phase::Verified {
                    self.backend.trip(step, FaultPoint::BeforeEffect)?;
                    self.update_step(context, step, |journal, record| {
                        let machine = journal
                            .machines
                            .get_mut(&context.vm_id())
                            .ok_or_else(|| anyhow!("adopted VM history missing"))?;
                        let retirement = machine
                            .retirement
                            .as_mut()
                            .filter(|r| r.operation_id == context.operation_id)
                            .ok_or_else(|| anyhow!("retirement claim missing"))?;
                        retirement.retired = true;
                        record.phase = Phase::Verified;
                        record.boot_id = Some(context.boot_id.clone());
                        record.identity = Some(json!({"retired": true}));
                        Ok(())
                    })?;
                    self.backend.trip(step, FaultPoint::AfterEffect)?;
                }
                return Ok(());
            }
            Step::RemoveStorage | Step::DestroyDataset => {}
            _ => bail!("{} is not a retirement step", step.name()),
        }
        if record.phase == Phase::Verified {
            return Ok(());
        }
        let journal = self.journal()?;
        ensure!(
            journal.machines[&context.vm_id()]
                .retirement
                .as_ref()
                .is_some_and(|r| r.operation_id == context.operation_id && !r.retired),
            "retirement claim missing before removing allocation resources"
        );
        let present = |state: (bool, bool)| {
            if step == Step::RemoveStorage {
                state.0
            } else {
                state.1
            }
        };
        if record.phase == Phase::Intent && !present(state) {
            return self.verified(context, step, json!({"absent": true}), None);
        }
        if record.phase == Phase::Intent {
            ensure!(
                record.boot_id.as_deref() != Some(context.boot_id.as_str()),
                "{} intent was recorded during this host boot and its target is still present; blocked pending investigation",
                step.name()
            );
        } else {
            self.intent(context, step)?;
        }
        self.backend.trip(step, FaultPoint::BeforeEffect)?;
        // Recheck immediately before the effect.
        let state = self.verify_retirement_state(context).await?;
        ensure!(present(state), "{} target disappeared", step.name());
        let adopted = context.adopted().context("adopted allocation missing")?;
        let effect = async {
            let fault = self.backend.effect_fault(step)?;
            if fault.as_ref().is_none_or(|fault| fault.applied) {
                if step == Step::RemoveStorage {
                    self.backend.remove_owner_storage(&adopted.storage).await?;
                } else {
                    self.backend
                        .remove_empty_owner_dataset(&adopted.dataset)
                        .await?;
                }
            }
            if let Some(fault) = fault {
                bail!(
                    "simulated {} effect command failure (effect applied: {})",
                    step.name(),
                    fault.applied
                );
            }
            Ok(())
        };
        if let Err(error) = effect.await {
            if error.chain().any(|cause| cause.is::<Crash>()) {
                return Err(error);
            }
            // The command's outcome is unknown: accept only an observed
            // postcondition, and never repeat the effect.
            return match self.verify_retirement_state(context).await {
                Ok(state) if !present(state) => {
                    eprintln!(
                        "hearth-host: {} reported an error but its target is verified absent ({error:#})",
                        step.name()
                    );
                    self.verified(context, step, json!({"absent": true}), None)
                }
                Ok(_) => Err(ambiguous(error.context(format!(
                    "{} command failed and its target is still present; blocked pending investigation",
                    step.name()
                )))),
                Err(observation) => Err(ambiguous(error.context(format!(
                    "{} command failed and reconciling it also failed ({observation:#}); blocked pending investigation",
                    step.name()
                )))),
            };
        }
        self.backend.trip(step, FaultPoint::AfterEffect)?;
        let after = self.verify_retirement_state(context).await?;
        ensure!(
            !present(after),
            "{} postcondition missing after its effect",
            step.name()
        );
        self.verified(context, step, json!({"absent": true}), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        managed::ManagedConfig,
        protocol::{User, VmState},
    };

    async fn fixture(kind: JobKind) -> (tempfile::TempDir, Host, Context, SimulatedWorld) {
        let temp = tempfile::tempdir().unwrap();
        let config: ManagedConfig = serde_json::from_str(include_str!(
            "../../../tests/fixtures/managed-adoption-config.json"
        ))
        .unwrap();
        let mut managed = config.binding;
        managed.scope.node = NODE.into();
        managed.scope.pool = POOL.into();
        managed.scope.root_dataset = format!("{POOL}/pve-dev-managed-{}", config.environment);
        let adopted = &mut managed.adopted_vms[0];
        adopted.dataset = format!("{POOL}/data/dev-erin");
        adopted.storage_configuration["pool"] = json!(adopted.dataset);
        adopted.storage_configuration["nodes"] = json!(NODE);
        adopted.storage_configuration["sparse"] = json!("0");
        let world_path = temp.path().join("world.json");
        let host = Host::open(HostConfig::simulated(
            temp.path().join("helper"),
            &config.environment,
            world_path.clone(),
        ))
        .await
        .unwrap();
        let world = SimulatedWorld::new(world_path);
        let adopted = managed.adopted_vms[0].clone();
        world
            .edit(|w| {
                w.datasets
                    .insert(adopted.dataset.clone(), adopted.dataset_properties.clone());
                w.datasets
                    .insert(adopted.root_volume(), adopted.root_properties.clone());
                w.datasets.insert(
                    adopted.cloud_init_volume(),
                    adopted.cloud_init_properties.clone(),
                );
                w.storages.insert(
                    adopted.storage.clone(),
                    adopted
                        .storage_configuration
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(k, v)| (k.clone(), text(v)))
                        .collect(),
                );
                w.vms.insert(
                    101,
                    SimulatedVm {
                        config: adopted
                            .configuration
                            .as_object()
                            .unwrap()
                            .iter()
                            .map(|(k, v)| (k.clone(), text(v)))
                            .collect(),
                        pending_config: BTreeMap::new(),
                        running: false,
                        pid: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        let p = &managed.admin_policy;
        let user = User {
            user_id: managed.admin_user.clone(),
            role: Role::Admin,
            budget_mib: p.budget_mib,
            min_memory_budget_mib: p.min_memory_budget_mib,
            max_memory_budget_mib: p.max_memory_budget_mib,
            max_cores: p.max_cores,
            max_vms: p.max_vms,
            ready: true,
            policy_revision: 0,
            state: AccountState::Active,
        };
        let mut vm = adopted.vm.clone();
        let retire = kind == JobKind::RetireAllocation;
        vm.state = if retire {
            VmState::Deleted
        } else {
            VmState::Stopped
        };
        let context = Context {
            environment: config.environment.clone(),
            operation_id: kind.as_str().into(),
            kind,
            owner: user.user_id.clone(),
            budget_mib: user.budget_mib,
            vm: Some(vm.clone()),
            boot_id: world.load().unwrap().boot_id,
            policy_revision: 0,
            owner_policy: user.policy(),
            policy_change: None,
            scope: Some(Namespace::from(&managed.scope)),
            managed: Some(managed),
            disk_shrink: None,
            extension: None,
        };
        let job = Job {
            operation_id: kind.as_str().into(),
            kind,
            owner: user,
            vm: Some(vm),
            policy_change: None,
            disk_shrink: None,
            extension: None,
        };
        if retire {
            // A verified delete removed the guest and its zvols and left the
            // allocation's dataset and storage entry behind.
            world
                .edit(|w| {
                    w.vms.remove(&101);
                    w.datasets.remove(&adopted.root_volume());
                    w.datasets.remove(&adopted.cloud_init_volume());
                    Ok(())
                })
                .unwrap();
        }
        host.change(|journal| {
            let mut machine = adopted.machine_record(&context.environment);
            if retire {
                machine.state = MachineState::Deleted;
                machine.delete_operation = Some("delete".into());
            }
            journal.machines.insert(101, machine);
            journal.jobs.insert(
                kind.as_str().into(),
                HostJob {
                    sequence: 1,
                    fingerprint: fingerprint(&job)?,
                    job,
                    status: OperationStatus::Running,
                    step: "queued".into(),
                    error: None,
                    attempts: 1,
                    steps: plan(kind)
                        .unwrap()
                        .iter()
                        .map(|step| StepRecord {
                            step: *step,
                            phase: Phase::Planned,
                            boot_id: None,
                            upid: None,
                            identity: None,
                            witness: None,
                            started_at: None,
                            finished_at: None,
                        })
                        .collect(),
                    started_at: None,
                    updated_at: None,
                    finished_at: None,
                    retry: None,
                    failure: None,
                },
            );
            Ok(())
        })
        .unwrap();
        (temp, host, context, world)
    }

    #[tokio::test]
    async fn adopted_lifecycle_uses_original_disks_without_configuration_rewrites() {
        for kind in [
            JobKind::Start,
            JobKind::Stop,
            JobKind::Shutdown,
            JobKind::Delete,
        ] {
            let (_temp, host, context, world) = fixture(kind).await;
            if matches!(kind, JobKind::Stop | JobKind::Shutdown) {
                world
                    .edit(|w| {
                        w.vms.get_mut(&101).unwrap().running = true;
                        Ok(())
                    })
                    .unwrap();
            }
            let original = world.load().unwrap();
            for step in plan(kind).unwrap() {
                host.run_step(&context, *step).await.unwrap();
            }
            let after = world.load().unwrap();
            assert_eq!(
                after.datasets[&context.dataset()],
                original.datasets[&context.dataset()]
            );
            assert_eq!(after.storages, original.storages);
            if kind == JobKind::Delete {
                assert!(!after.vms.contains_key(&101));
                assert!(!after.datasets.contains_key(&context.root_volume()));
                assert_eq!(
                    host.journal().unwrap().machines[&101].state,
                    MachineState::Deleted
                );
            } else {
                assert_eq!(after.vms[&101].config, original.vms[&101].config);
                assert_eq!(after.datasets, original.datasets);
                assert_eq!(after.vms[&101].running, kind == JobKind::Start);
            }
        }
    }

    #[tokio::test]
    async fn adopted_drift_blocks_before_recording_any_effect_intent() {
        for change in 0..4 {
            let (_temp, host, context, world) = fixture(JobKind::Start).await;
            world
                .edit(|w| {
                    match change {
                        0 => {
                            w.datasets
                                .get_mut(&context.root_volume())
                                .unwrap()
                                .insert("guid".into(), "999".into());
                        }
                        1 => {
                            w.datasets
                                .get_mut(&context.dataset())
                                .unwrap()
                                .insert("quota".into(), "1".into());
                        }
                        2 => {
                            w.vms
                                .get_mut(&101)
                                .unwrap()
                                .config
                                .insert("sshkeys".into(), "foreign-key".into());
                        }
                        _ => {
                            w.vms
                                .get_mut(&101)
                                .unwrap()
                                .pending_config
                                .insert("cores".into(), "16".into());
                        }
                    };
                    Ok(())
                })
                .unwrap();
            assert!(host.run_step(&context, Step::Start).await.is_err());
            assert_eq!(
                host.step_record(&context, Step::Start).unwrap().phase,
                Phase::Planned
            );
            assert!(!world.load().unwrap().vms[&101].running);
        }
    }

    #[tokio::test]
    async fn adopted_lost_receipt_accepts_only_a_verified_postcondition() {
        let (_temp, host, context, world) = fixture(JobKind::Start).await;
        world
            .edit(|w| {
                w.faults.push(Fault {
                    step: Step::Start,
                    point: FaultPoint::AfterEffect,
                });
                Ok(())
            })
            .unwrap();
        assert!(host.run_step(&context, Step::Start).await.is_err());
        let task_count = world.load().unwrap().tasks.len();
        host.run_step(&context, Step::Start).await.unwrap();
        assert_eq!(world.load().unwrap().tasks.len(), task_count);
        assert_eq!(
            host.step_record(&context, Step::Start).unwrap().phase,
            Phase::Verified
        );
    }

    #[tokio::test]
    async fn adopted_retained_snapshots_refuse_delete_before_stopping_the_guest() {
        let (_temp, host, context, world) = fixture(JobKind::Delete).await;
        world
            .edit(|w| {
                w.vms.get_mut(&101).unwrap().running = true;
                w.datasets.insert(
                    context.root_volume() + "@recovery",
                    BTreeMap::from([("guid".into(), "998".into())]),
                );
                Ok(())
            })
            .unwrap();
        assert!(host.run_step(&context, Step::Verify).await.is_err());
        assert!(world.load().unwrap().vms[&101].running);
        assert_eq!(
            host.step_record(&context, Step::Stop).unwrap().phase,
            Phase::Planned
        );
    }

    #[tokio::test]
    async fn adopted_missing_effect_receipt_does_not_reexecute_during_the_same_boot() {
        let (_temp, host, context, world) = fixture(JobKind::Start).await;
        world
            .edit(|w| {
                w.faults.push(Fault {
                    step: Step::Start,
                    point: FaultPoint::BeforeEffect,
                });
                Ok(())
            })
            .unwrap();
        assert!(host.run_step(&context, Step::Start).await.is_err());
        assert!(host.run_step(&context, Step::Start).await.is_err());
        assert!(!world.load().unwrap().vms[&101].running);
        assert!(world.load().unwrap().tasks.is_empty());
    }

    const RETIRE: [Step; 4] = [
        Step::RetireVerify,
        Step::RemoveStorage,
        Step::DestroyDataset,
        Step::RetireRelease,
    ];

    async fn retire_all(host: &Host, context: &Context) -> Result<()> {
        for step in RETIRE {
            host.run_step(context, step).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn retirement_removes_storage_then_dataset_of_a_deleted_adoption() {
        let (_temp, mut host, context, world) = fixture(JobKind::RetireAllocation).await;
        let adopted = context.adopted().unwrap().clone();
        let before = world.load().unwrap();
        host.run_step(&context, Step::RetireVerify).await.unwrap();
        // The verification is journal-only.
        assert_eq!(world.load().unwrap().datasets, before.datasets);
        assert_eq!(world.load().unwrap().storages, before.storages);
        host.run_step(&context, Step::RemoveStorage).await.unwrap();
        let after_storage = world.load().unwrap();
        assert!(!after_storage.storages.contains_key(&adopted.storage));
        assert!(after_storage.datasets.contains_key(&adopted.dataset));
        host.run_step(&context, Step::DestroyDataset).await.unwrap();
        host.run_step(&context, Step::RetireRelease).await.unwrap();
        let after = world.load().unwrap();
        assert!(!after.datasets.contains_key(&adopted.dataset));
        // Nothing else on the pool changed.
        let mut expected = before.datasets.clone();
        expected.remove(&adopted.dataset);
        assert_eq!(after.datasets, expected);
        for step in RETIRE {
            assert_eq!(
                host.step_record(&context, step).unwrap().phase,
                Phase::Verified
            );
        }
        let journal = host.journal().unwrap();
        let machine = &journal.machines[&101];
        assert!(machine.allocation_retired());
        assert_eq!(machine.state, MachineState::Deleted);
        validate_scope(&journal).unwrap();
        // Rerunning a finished retirement changes nothing.
        retire_all(&host, &context).await.unwrap();
        assert_eq!(world.load().unwrap().datasets, after.datasets);
        // A retired allocation is accepted as absent by inventory, and is no
        // longer verified as a live allocation.
        host.binding.managed = context.managed.clone();
        host.change(|journal| {
            journal.jobs.get_mut("retire_allocation").unwrap().status = OperationStatus::Verified;
            journal.owners.insert(
                context.owner.clone(),
                OwnerRecord {
                    user_id: context.owner.clone(),
                    budget_mib: context.budget_mib,
                    operation_id: "owner".into(),
                    ready: true,
                    identities: BTreeMap::new(),
                    policy_revision: 0,
                    policy: Some(context.owner_policy.clone()),
                    state: AccountState::Active,
                    delete_operation: None,
                },
            );
            Ok(())
        })
        .unwrap();
        let checks = host.inspect_adopted_resources().await.unwrap();
        assert_eq!(checks[0]["status"], "verified");
        assert_eq!(checks[0]["allocation_retired"], true);
        assert!(host.verify_adopted_allocation(&context).await.is_err());
    }

    #[tokio::test]
    async fn retirement_refuses_an_adopted_vm_that_is_not_deleted() {
        let (_temp, host, context, world) = fixture(JobKind::Start).await;
        let mut retire = context.clone();
        retire.kind = JobKind::RetireAllocation;
        host.change(|journal| {
            let mut job = journal.jobs.remove("start").unwrap();
            job.job.kind = JobKind::RetireAllocation;
            job.job.operation_id = "retire".into();
            job.steps = plan(JobKind::RetireAllocation)
                .unwrap()
                .iter()
                .map(|step| StepRecord {
                    step: *step,
                    phase: Phase::Planned,
                    boot_id: None,
                    upid: None,
                    identity: None,
                    witness: None,
                    started_at: None,
                    finished_at: None,
                })
                .collect();
            journal.jobs.insert("retire".into(), job);
            Ok(())
        })
        .unwrap();
        retire.operation_id = "retire".into();
        let before = world.load().unwrap();
        let error = host
            .run_step(&retire, Step::RetireVerify)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("only a verified deletion"));
        assert!(host.journal().unwrap().machines[&101].retirement.is_none());
        assert_eq!(world.load().unwrap().datasets, before.datasets);
        assert_eq!(world.load().unwrap().storages, before.storages);
        // Admission refuses it too.
        assert!(host.admit(&retire).is_err());
    }

    #[tokio::test]
    async fn retirement_refuses_drift_and_retained_references_before_any_effect() {
        for change in 0..8 {
            let (_temp, host, context, world) = fixture(JobKind::RetireAllocation).await;
            let adopted = context.adopted().unwrap().clone();
            world
                .edit(|w| {
                    match change {
                        0 => {
                            w.datasets.insert(
                                adopted.dataset.clone() + "@keep",
                                BTreeMap::from([("guid".into(), "990".into())]),
                            );
                        }
                        1 => {
                            w.datasets.insert(
                                adopted.dataset.clone() + "#mark",
                                BTreeMap::from([("guid".into(), "991".into())]),
                            );
                        }
                        2 => {
                            w.datasets.insert(
                                adopted.dataset.clone() + "/extra",
                                BTreeMap::from([("type".into(), "filesystem".into())]),
                            );
                        }
                        3 => {
                            w.datasets
                                .get_mut(&adopted.dataset)
                                .unwrap()
                                .insert("guid".into(), "999".into());
                        }
                        4 => {
                            w.datasets
                                .get_mut(&adopted.dataset)
                                .unwrap()
                                .insert("reservation".into(), "1".into());
                        }
                        5 => {
                            w.storages
                                .get_mut(&adopted.storage)
                                .unwrap()
                                .insert("content".into(), "images,rootdir".into());
                        }
                        6 => {
                            // Another guest still points at the storage.
                            w.vms.insert(
                                102,
                                SimulatedVm {
                                    config: BTreeMap::from([(
                                        "scsi1".into(),
                                        format!("{}:vm-102-disk-0", adopted.storage),
                                    )]),
                                    ..SimulatedVm::default()
                                },
                            );
                        }
                        _ => {
                            // Data written directly into the filesystem.
                            w.datasets
                                .get_mut(&adopted.dataset)
                                .unwrap()
                                .insert("usedbydataset".into(), (64 * MIB).to_string());
                        }
                    };
                    Ok(())
                })
                .unwrap();
            let before = world.load().unwrap();
            let mut refused = false;
            for step in RETIRE {
                if host.run_step(&context, step).await.is_err() {
                    refused = true;
                    break;
                }
            }
            assert!(refused, "change {change} was not refused");
            let after = world.load().unwrap();
            assert_eq!(after.datasets, before.datasets, "change {change}");
            assert_eq!(after.storages, before.storages, "change {change}");
            for step in [Step::RemoveStorage, Step::DestroyDataset] {
                assert_eq!(
                    host.step_record(&context, step).unwrap().phase,
                    Phase::Planned,
                    "change {change}"
                );
            }
        }
    }

    #[tokio::test]
    async fn retirement_rechecks_preconditions_immediately_before_destroying_the_dataset() {
        let (_temp, host, context, world) = fixture(JobKind::RetireAllocation).await;
        let adopted = context.adopted().unwrap().clone();
        host.run_step(&context, Step::RetireVerify).await.unwrap();
        host.run_step(&context, Step::RemoveStorage).await.unwrap();
        // A snapshot appears between the storage removal and the destroy.
        world
            .edit(|w| {
                w.datasets.insert(
                    adopted.dataset.clone() + "@late",
                    BTreeMap::from([("guid".into(), "992".into())]),
                );
                Ok(())
            })
            .unwrap();
        assert!(host.run_step(&context, Step::DestroyDataset).await.is_err());
        assert!(
            world
                .load()
                .unwrap()
                .datasets
                .contains_key(&adopted.dataset)
        );
        assert_eq!(
            host.step_record(&context, Step::DestroyDataset)
                .unwrap()
                .phase,
            Phase::Planned
        );
        assert!(host.run_step(&context, Step::RetireRelease).await.is_err());
        assert!(!host.journal().unwrap().machines[&101].allocation_retired());
    }

    #[tokio::test]
    async fn retirement_lost_receipt_after_storage_removal_resolves_by_observation() {
        let (_temp, host, context, world) = fixture(JobKind::RetireAllocation).await;
        let adopted = context.adopted().unwrap().clone();
        world
            .edit(|w| {
                // The DELETE ran but its reply was lost.
                w.effect_faults.push(EffectFault {
                    step: Step::RemoveStorage,
                    applied: true,
                });
                // The dataset destroy crashed right after it ran.
                w.faults.push(Fault {
                    step: Step::DestroyDataset,
                    point: FaultPoint::AfterEffect,
                });
                Ok(())
            })
            .unwrap();
        host.run_step(&context, Step::RetireVerify).await.unwrap();
        host.run_step(&context, Step::RemoveStorage).await.unwrap();
        assert_eq!(
            host.step_record(&context, Step::RemoveStorage)
                .unwrap()
                .phase,
            Phase::Verified
        );
        assert!(
            !world
                .load()
                .unwrap()
                .storages
                .contains_key(&adopted.storage)
        );
        assert!(host.run_step(&context, Step::DestroyDataset).await.is_err());
        assert_eq!(
            host.step_record(&context, Step::DestroyDataset)
                .unwrap()
                .phase,
            Phase::Intent
        );
        // The retry observes the absent dataset instead of destroying again.
        host.run_step(&context, Step::DestroyDataset).await.unwrap();
        host.run_step(&context, Step::RetireRelease).await.unwrap();
        assert!(host.journal().unwrap().machines[&101].allocation_retired());
    }

    #[tokio::test]
    async fn retirement_failed_effect_with_target_still_present_blocks_without_retrying() {
        let (_temp, host, context, world) = fixture(JobKind::RetireAllocation).await;
        let adopted = context.adopted().unwrap().clone();
        world
            .edit(|w| {
                w.effect_faults.push(EffectFault {
                    step: Step::RemoveStorage,
                    applied: false,
                });
                Ok(())
            })
            .unwrap();
        host.run_step(&context, Step::RetireVerify).await.unwrap();
        let error = host
            .run_step(&context, Step::RemoveStorage)
            .await
            .unwrap_err();
        assert_eq!(
            super::super::error_policy::class_of(&error),
            Some(super::super::error_policy::ErrorClass::Ambiguous)
        );
        assert_eq!(
            host.step_record(&context, Step::RemoveStorage)
                .unwrap()
                .phase,
            Phase::Intent
        );
        // Same boot, target still present: never re-run the DELETE blindly.
        assert!(host.run_step(&context, Step::RemoveStorage).await.is_err());
        assert!(
            world
                .load()
                .unwrap()
                .storages
                .contains_key(&adopted.storage)
        );
    }

    #[tokio::test]
    async fn refused_retirement_can_be_acknowledged_without_touching_the_allocation() {
        let (_temp, mut host, context, world) = fixture(JobKind::RetireAllocation).await;
        let adopted = context.adopted().unwrap().clone();
        world
            .edit(|w| {
                w.datasets.insert(
                    adopted.dataset.clone() + "@keep",
                    BTreeMap::from([("guid".into(), "990".into())]),
                );
                Ok(())
            })
            .unwrap();
        let error = host
            .run_step(&context, Step::RetireVerify)
            .await
            .unwrap_err();
        host.change(|journal| {
            let job = journal.jobs.get_mut("retire_allocation").unwrap();
            job.status = OperationStatus::Blocked;
            job.error = Some(format!("{error:#}"));
            Ok(())
        })
        .unwrap();
        host.binding.managed = context.managed.clone();
        let proof = host.inspect_refusal(&context).await.unwrap();
        assert_eq!(proof["acknowledge_refusal"], true);
        assert_eq!(proof["vm_status"], "absent");
        assert_eq!(proof["refused_step"], "retire_verify");
        // Once a retirement claim is recorded, a refusal no longer applies.
        host.change(|journal| {
            journal.machines.get_mut(&101).unwrap().retirement = Some(Retirement {
                operation_id: "retire_allocation".into(),
                retired: false,
            });
            Ok(())
        })
        .unwrap();
        assert!(host.inspect_refusal(&context).await.is_err());
    }
}
