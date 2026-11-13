//! Existing resources use the same durable lifecycle plans, with independently
//! pinned allocation/configuration identities instead of creation markers.
use super::*;
use crate::control::JobKind;
use crate::control::adoption::{DATASET_PROPERTIES, VOLUME_PROPERTIES};

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
                "guest_or_storage_effects":false}),
            );
        }
        Ok(json!(checks))
    }

    pub(super) async fn verify_adopted_allocation(&self, context: &Context) -> Result<()> {
        let adopted = context.adopted().context("adopted allocation missing")?;
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
        vm.state = VmState::Stopped;
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
        host.change(|journal| {
            journal
                .machines
                .insert(101, adopted.machine_record(&context.environment));
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
}
