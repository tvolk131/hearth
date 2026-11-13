//! Explicit enrollment of an already stopped VM inside the immutable scope.
//! An observation is never ownership: the reviewed plan pins all resources,
//! then an intent precedes the small metadata changes and proof publication.
use super::*;
use crate::control::vm_change::{EnrollmentPlan, JobExtension};

const ENROLL_KEYS: &[&str] = &[
    "type",
    "guid",
    "createtxg",
    "volsize",
    "refreservation",
    "origin",
    "org.pve-dev:owner",
    "org.pve-dev:image",
    "readonly",
];
impl Host {
    pub(super) async fn plan_enrollment(
        &self,
        vm_id: u16,
        owner_id: &str,
    ) -> Result<EnrollmentPlan> {
        self.check_host().await?;
        if let Some(binding) = &self.binding.physical_trial {
            ensure!(
                binding.scope.features.contains("enrollment")
                    && binding.scope.permits(owner_id, Some(vm_id)),
                "VM enrollment escapes the explicit physical scope"
            );
        }
        ensure!(
            (9001..=9099).contains(&vm_id),
            "enrollment VMID outside the admitted range"
        );
        let journal = self.journal()?;
        ensure!(
            !journal.machines.contains_key(&vm_id),
            "VMID already has helper history"
        );
        let owner = journal
            .owners
            .get(owner_id)
            .context("enrollment owner has no ready allocation")?;
        ensure!(
            owner.ready && owner.state == AccountState::Active,
            "enrollment owner is not active/ready"
        );
        let policy = Self::owner_policy(&journal, owner)?;
        let context = Context {
            environment: self.binding.environment.clone(),
            operation_id: "enrollment-plan".into(),
            kind: "enroll_vm".into(),
            owner: owner_id.into(),
            budget_mib: owner.budget_mib,
            vm: Some(Vm {
                vm_id,
                owner: owner_id.into(),
                name: "inspection".into(),
                disk_mib: 2048,
                memory_mib: 128,
                min_memory_mib: 128,
                cores: 1,
                state: crate::control::protocol::VmState::Stopped,
                guest: None,
            }),
            boot_id: self.check_host().await?,
            policy_revision: owner.policy_revision,
            owner_policy: policy,
            policy_change: None,
            disk_shrink: None,
            extension: None,
            scope: self.binding.namespace(),
            managed: self.binding.managed.clone(),
        };
        self.verify_owner(&context).await?;
        let config = self
            .backend
            .api_get(&format!("{}/config", context.vm_path()), &[])
            .await?;
        let map = config
            .as_object()
            .context("enrollment configuration is not an object")?;
        for key in map.keys() {
            ensure!(
                !FORBIDDEN_KEYS.contains(&key.as_str()),
                "enrollment refuses locked, snapshot, hook, passthrough or custom-snippet configuration: {key}"
            );
            ensure!(
                !is_device(key) || matches!(key.as_str(), "scsi0" | "ide2" | "net0" | "serial0"),
                "enrollment refuses extra device {key}"
            );
        }
        ensure!(
            !text(&config["description"]).starts_with("pve-dev-control:"),
            "enrollment refuses an existing controller VM ownership claim"
        );
        ensure!(
            text(&config["onboot"]).is_empty() || text(&config["onboot"]) == "0",
            "disable autostart explicitly before enrolling the stopped fixture"
        );
        ensure!(
            text(&config["boot"]) == "order=scsi0" && text(&config["smbios1"]).starts_with("uuid="),
            "enrollment needs an identified scsi0 boot disk"
        );
        let root_reference = format!("{}:{}", context.storage(), context.root_name());
        let ci_reference = format!("{}:{}", context.storage(), context.cloud_init_name());
        ensure!(
            text(&config["scsi0"]).split(',').next() == Some(root_reference.as_str())
                && text(&config["ide2"]).split(',').next() == Some(ci_reference.as_str()),
            "enrollment disks must already belong to the selected scoped owner storage"
        );
        let root = self
            .backend
            .zfs_get(&context.root_volume(), ENROLL_KEYS)
            .await?;
        let ci = self
            .backend
            .zfs_get(&context.cloud_init_volume(), ENROLL_KEYS)
            .await?;
        for props in [&root, &ci] {
            ensure!(
                props["type"] == "volume" && props["origin"] == "-" && props["readonly"] == "off",
                "enrollment requires writable independent zvols"
            );
            let claim = &props["org.pve-dev:owner"];
            ensure!(
                claim == "-" || claim == &context.owner_marker(),
                "enrollment refuses a disk with an existing VM or foreign owner claim"
            );
        }
        let bytes = root["volsize"].parse::<u64>()?;
        ensure!(
            bytes % MIB == 0
                && root["refreservation"].parse::<u64>()? >= bytes
                && attachment_bytes(&text(&config["scsi0"]))? == bytes,
            "enrollment root is not exactly thick-backed at its reported size"
        );
        ensure!(
            ci["volsize"].parse::<u64>()? == 4 * MIB
                && ci["refreservation"].parse::<u64>()? >= 4 * MIB,
            "enrollment requires the exact thick-backed 4MiB cloud-init drive"
        );
        let all = self.backend.zfs_names().await?;
        for name in [context.root_volume(), context.cloud_init_volume()] {
            ensure!(
                !all.iter().any(|n| n.starts_with(&(name.clone() + "@"))
                    || n.starts_with(&(name.clone() + "#"))
                    || n.starts_with(&(name.clone() + "/"))),
                "enrollment snapshots/bookmarks/children are unsupported"
            );
        }
        ensure!(
            self.status(&context).await? == "stopped",
            "enrollment requires an actually stopped VM"
        );
        let vm = Vm {
            vm_id,
            owner: owner_id.into(),
            name: text(&config["name"]),
            disk_mib: bytes / MIB,
            memory_mib: text(&config["memory"])
                .trim_start_matches("current=")
                .parse()?,
            min_memory_mib: text(&config["balloon"])
                .parse()
                .unwrap_or(text(&config["memory"]).parse()?),
            cores: text(&config["cores"]).parse()?,
            state: crate::control::protocol::VmState::Stopped,
            guest: None,
        };
        let limits = self
            .binding
            .physical_trial
            .as_ref()
            .and_then(|b| b.scope.limits)
            .unwrap_or_default();
        let expanded = self
            .binding
            .physical_trial
            .as_ref()
            .is_some_and(|b| b.scope.limits.is_some());
        ensure!(
            valid_label(&vm.name, 48)
                && (16..=if expanded { limits.disk_mib } else { 512 }).contains(&vm.disk_mib)
                && (128..=if expanded { limits.memory_mib } else { 512 }).contains(&vm.memory_mib)
                && vm.min_memory_mib >= 128
                && vm.min_memory_mib <= vm.memory_mib
                && (1..=if expanded { limits.cores } else { 4 }).contains(&vm.cores),
            "enrollment profile exceeds the immutable scope"
        );
        let configuration_sha256 = super::super::store::digest(&normalized_pinned_config(&config)?);
        let evidence = json!({"binding":self.binding,"config":config,"root":root,"cloud_init":ci,"policy_revision":owner.policy_revision});
        let plan_id = super::super::store::digest(&("enrollment-plan/1", &evidence, &vm));
        Ok(EnrollmentPlan {
            plan_id,
            vm,
            configuration_sha256,
            root_guid: root["guid"].clone(),
            cloud_init_guid: ci["guid"].clone(),
            observed_at: super::super::store::now(),
            eligible: true,
            blocked_reasons: vec![],
            evidence,
        })
    }

    pub(super) async fn run_enroll_step(
        &self,
        context: &Context,
        step: Step,
        record: &StepRecord,
    ) -> Result<()> {
        if record.phase == Phase::Verified {
            return Ok(());
        }
        ensure!(
            record.phase == Phase::Planned,
            "uncertain enrollment intent; inspect its exact metadata before any recovery"
        );
        let Some(JobExtension::Enroll(spec)) = context.extension.as_deref() else {
            bail!("enrollment payload missing")
        };
        let plan = &spec.plan;
        if step == Step::Enroll {
            let fresh = self
                .plan_enrollment(context.vm_id(), &context.owner)
                .await?;
            ensure!(
                fresh.plan_id == plan.plan_id,
                "VM changed since enrollment review"
            );
            self.update_step(context, step, |_, r| {
                r.witness = Some(plan.evidence.clone());
                Ok(())
            })?;
            self.intent(context, step)?;
            self.backend.trip(step, FaultPoint::BeforeEffect)?;
            match &self.backend {
                Backend::Lab(lab) => {
                    lab.run(
                        "zfs",
                        vec![
                            "set".into(),
                            format!("org.pve-dev:owner={}", context.vm_marker()),
                            context.root_volume(),
                        ],
                    )
                    .await?;
                }
                Backend::Simulated(world) => {
                    world.edit(|w| {
                        w.datasets
                            .get_mut(&context.root_volume())
                            .context("enrollment root missing")?
                            .insert("org.pve-dev:owner".into(), context.vm_marker());
                        Ok(())
                    })?;
                }
            }
            self.backend
                .api_set(
                    &format!("{}/config", context.vm_path()),
                    &[
                        ("digest", text(&plan.evidence["config"]["digest"])),
                        ("description", context.vm_marker()),
                        ("protection", "1".into()),
                    ],
                )
                .await?;
            self.backend.trip(step, FaultPoint::AfterEffect)?;
            let config = self
                .backend
                .api_get(&format!("{}/config", context.vm_path()), &[])
                .await?;
            let mut expected = plan.evidence["config"].clone();
            expected["description"] = json!(context.vm_marker());
            expected["protection"] = json!(1);
            ensure!(
                normalized_pinned_config(&config)? == normalized_pinned_config(&expected)?
                    && text(&config["protection"]) == "1",
                "enrollment changed unexpected VM metadata"
            );
            let root = self
                .backend
                .zfs_get(&context.root_volume(), ENROLL_KEYS)
                .await?;
            let ci = self
                .backend
                .zfs_get(&context.cloud_init_volume(), ENROLL_KEYS)
                .await?;
            let mut expected_root = plan.evidence["root"].clone();
            expected_root["org.pve-dev:owner"] = json!(context.vm_marker());
            ensure!(
                json!(root) == expected_root && json!(ci) == plan.evidence["cloud_init"],
                "enrollment backing identity changed"
            );
            self.change(|j| {
                ensure!(
                    !j.machines.contains_key(&context.vm_id()),
                    "enrollment VM already recorded"
                );
                j.machines.insert(
                    context.vm_id(),
                    MachineRecord {
                        vm_id: context.vm_id(),
                        owner: context.owner.clone(),
                        profile: profile(context.vm()?),
                        create_operation: context.operation_id.clone(),
                        delete_operation: None,
                        state: MachineState::Creating,
                        identities: BTreeMap::new(),
                        pinned_config: Some(config.clone()),
                    },
                );
                Ok(())
            })?;
            for stable in [Step::Vm, Step::Volume, Step::Attach, Step::CloudInit] {
                let value = self
                    .observe(context, stable)
                    .await?
                    .context("enrolled resource disappeared")?;
                self.change(|j| {
                    j.machines
                        .get_mut(&context.vm_id())
                        .context("enrollment history missing")?
                        .identities
                        .insert(stable.name().into(), value);
                    Ok(())
                })?;
            }
            self.verified(context,step,json!({"plan_id":plan.plan_id,"root_guid":plan.root_guid,"cloud_init_guid":plan.cloud_init_guid}),Some(plan.evidence.clone()))?;
            self.backend.trip(step, FaultPoint::AfterReceipt)?;
            return Ok(());
        }
        bail!("unexpected enrollment step")
    }
}
