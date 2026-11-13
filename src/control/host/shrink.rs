//! Conservative shrink execution. Ambiguous intents block on every host boot;
//! no filesystem conversion, rename or recovery deletion is blindly replayed.
use super::*;
use crate::control::JobKind;

pub(super) const WORKER: &str = include_str!("../shrink_worker.py");
const SOURCE_KEYS: &[&str] = &[
    "type",
    "guid",
    "createtxg",
    "volsize",
    "refreservation",
    "origin",
    "org.pve-dev:owner",
    "org.pve-dev:image",
    "org.pve-dev:shrink",
    "readonly",
];

impl Host {
    fn shrink_target(&self, context: &Context) -> Result<Context> {
        let mut target = context.clone();
        target.vm.as_mut().context("shrink VM missing")?.disk_mib = context
            .disk_shrink
            .as_ref()
            .context("shrink specification missing")?
            .target_disk_mib;
        Ok(target)
    }

    fn shrink_proof(&self, preparation: &str, step: Step) -> Result<Value> {
        self.journal()?
            .jobs
            .get(preparation)
            .context("preparation history missing")?
            .steps
            .iter()
            .find(|s| s.step == step && s.phase == Phase::Verified)
            .and_then(|s| s.identity.clone())
            .context("verified preparation proof missing")
    }

    async fn shrink_input(&self, context: &Context, step: Step) -> Result<Value> {
        ensure!(
            self.binding
                .physical_trial
                .as_ref()
                .is_none_or(|b| b.scope.features.contains("resize")),
            "scope does not authorize shrink"
        );
        let spec = context
            .disk_shrink
            .as_ref()
            .context("shrink specification missing")?;
        let preparation = spec
            .preparation_id
            .as_deref()
            .unwrap_or(&context.operation_id);
        let original = self
            .journal()?
            .jobs
            .get(preparation)
            .context("preparation job missing")?
            .job
            .clone();
        ensure!(
            original.kind == JobKind::ShrinkVm
                && original.owner == self.journal()?.jobs[&context.operation_id].job.owner,
            "preparation owner/policy differs from finish payload"
        );
        let mut input = json!({"environment":self.binding.environment,"pool_guid":self.binding.pool_guid,
            "preparation_id":preparation,"vm_id":context.vm_id(),"owner":context.owner,
            "old_mib":spec.previous_disk_mib,"target_mib":spec.target_disk_mib,"workspace_mib":spec.workspace_mib(),
            "helper_pid":std::process::id(),"operation_id":context.operation_id,
            "physical_trial":self.binding.physical_trial,"root_partition_uuid":spec.root_partition_uuid,
            "phase":match step {Step::ShrinkStage=>"stage",Step::ShrinkSwap=>"swap",Step::ShrinkUnlock=>"unlock",Step::ShrinkFinish=>"finish",_=>bail!("not a shrink step")}});
        if step == Step::ShrinkStage {
            self.verify_machine(context).await?;
            ensure!(
                self.status(context).await? == "stopped",
                "shrink requires an actually stopped guest"
            );
            input["config"] = self.managed_config(context, true).await?;
            input["source"] = json!(
                self.backend
                    .zfs_get(&context.root_volume(), SOURCE_KEYS)
                    .await?
            );
        } else {
            let stage = self.shrink_proof(preparation, Step::ShrinkStage)?;
            input["config"] = stage["original_config"].clone();
            input["stage"] = stage;
            if matches!(step, Step::ShrinkUnlock | Step::ShrinkFinish) {
                input["swap"] = self.shrink_proof(preparation, Step::ShrinkSwap)?;
            }
            if step == Step::ShrinkFinish {
                self.verify_machine(context).await?;
                ensure!(
                    self.status(context).await? == "stopped",
                    "stop the verified target before finishing shrink"
                );
            }
        }
        Ok(input)
    }

    async fn shrink_adapter(&self, input: &Value) -> Result<Value> {
        match &self.backend {
            Backend::Lab(lab) => {
                let budget = disk_maintenance_timeout(
                    input["old_mib"].as_u64().context("old size missing")?.max(
                        input["target_mib"]
                            .as_u64()
                            .context("target size missing")?,
                    ),
                );
                ensure!(
                    lab.scope
                        .as_ref()
                        .is_none_or(|s| s.features.contains("resize")),
                    "physical scope does not authorize shrink"
                );
                Worker {
                    label: "shrink adapter",
                    script: WORKER,
                    args: &[],
                    input: Some(input),
                    budget,
                    grace: DEFAULT_GRACE,
                    max_output: MAX_HOST_MESSAGE,
                    gid: Some(0),
                    timeout_hint: "retained original/workspace must be investigated",
                }
                .run_json()
                .await
            }
            Backend::Simulated(world) => self.simulate_shrink(world, input),
        }
    }

    pub(super) async fn run_shrink_step(
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
            "ambiguous shrink intent; original and capacity holds retained; no automatic phase replay"
        );
        self.check_host().await?;
        let input = self.shrink_input(context, step).await?;
        self.update_step(context, step, |_, r| {
            r.witness = Some(input.clone());
            Ok(())
        })?;
        self.intent(context, step)?;
        self.backend.trip(step, FaultPoint::BeforeEffect)?;
        let mut proof = self.shrink_adapter(&input).await?;
        self.backend.trip(step, FaultPoint::AfterEffect)?;
        if step == Step::ShrinkStage {
            proof["original_config"] = input["config"].clone();
        }
        self.accept_shrink_proof(context, step, proof).await?;
        self.backend.trip(step, FaultPoint::AfterReceipt)?;
        Ok(())
    }

    pub(super) async fn accept_shrink_proof(
        &self,
        context: &Context,
        step: Step,
        mut proof: Value,
    ) -> Result<()> {
        ensure!(
            matches!(
                step,
                Step::ShrinkStage | Step::ShrinkSwap | Step::ShrinkUnlock | Step::ShrinkFinish
            ),
            "invalid shrink proof phase"
        );
        ensure!(
            self.step_record(context, step)?.phase == Phase::Intent,
            "shrink proof requires its durable intent"
        );
        self.check_host().await?;
        if step == Step::ShrinkStage && proof.get("original_config").is_none() {
            proof["original_config"] = self
                .step_record(context, step)?
                .witness
                .context("source witness missing")?["config"]
                .clone();
        }
        let mut replacements = BTreeMap::new();
        if step == Step::ShrinkSwap {
            let target = self.shrink_target(context)?;
            let machine = self.journal()?.machines[&context.vm_id()].clone();
            // Keep the original VM identity and cloud-init identity exact. Only
            // root backing/attachment and its imported-image GUID may change.
            for stable in [Step::Vm, Step::CloudInit] {
                ensure!(
                    self.observe(&target, stable).await?.as_ref()
                        == machine.identities.get(stable.name()),
                    "stable guest identity changed during shrink"
                );
            }
            for changed in [Step::Volume, Step::Image, Step::Attach]
                .into_iter()
                .filter(|s| {
                    *s != Step::Image || context.vm.as_ref().is_some_and(|v| v.guest.is_some())
                })
            {
                replacements.insert(
                    changed.name().into(),
                    self.observe(&target, changed)
                        .await?
                        .context("replacement identity missing")?,
                );
            }
            let stage = self.shrink_proof(&context.operation_id, Step::ShrinkStage)?;
            ensure!(
                replacements["volume"]["guid"] == stage["replacement"]["guid"]
                    && replacements["volume"]["createtxg"] == stage["replacement"]["createtxg"],
                "replacement provenance differs from prepared copy"
            );
        }
        if step == Step::ShrinkUnlock {
            self.verify_machine(&self.shrink_target(context)?).await?;
        }
        self.update_step(context, step, |journal, record| {
            if step == Step::ShrinkSwap {
                let machine = journal
                    .machines
                    .get_mut(&context.vm_id())
                    .context("machine missing")?;
                machine.profile.disk_mib = context
                    .disk_shrink
                    .as_ref()
                    .context("spec missing")?
                    .target_disk_mib;
                if let Some(config) = machine.pinned_config.as_mut() {
                    let original = text(&config["scsi0"]);
                    let mut options: Vec<String> = original
                        .split(',')
                        .filter(|p| !p.starts_with("size="))
                        .map(str::to_owned)
                        .collect();
                    options.push(format!("size={}M", machine.profile.disk_mib));
                    config["scsi0"] = json!(options.join(","));
                }
                machine.identities.extend(replacements);
            }
            if step == Step::ShrinkUnlock {
                journal
                    .disk_shrinks
                    .get_mut(&context.vm_id())
                    .context("shrink hold missing")?
                    .prepared = true;
            }
            if step == Step::ShrinkFinish {
                journal.disk_shrinks.remove(&context.vm_id());
            }
            record.phase = Phase::Verified;
            record.boot_id = Some(context.boot_id.clone());
            record.identity = Some(proof);
            Ok(())
        })?;
        Ok(())
    }

    pub(super) async fn inspect_shrink(&self, context: &Context, quick: bool) -> Result<Value> {
        self.check_host().await?;
        let spec = context
            .disk_shrink
            .as_ref()
            .context("shrink specification missing")?;
        let prep = spec
            .preparation_id
            .as_deref()
            .unwrap_or(&context.operation_id);
        self.shrink_adapter(&json!({"environment":self.binding.environment,"pool_guid":self.binding.pool_guid,"physical_trial":self.binding.physical_trial,
            "preparation_id":prep,"operation_id":context.operation_id,"vm_id":context.vm_id(),"owner":context.owner,
            "old_mib":spec.previous_disk_mib,"target_mib":spec.target_disk_mib,"workspace_mib":spec.workspace_mib(),
            "root_partition_uuid":spec.root_partition_uuid,"helper_pid":std::process::id(),"phase":"inspect","quick":quick})).await
    }
    pub(super) async fn abort_shrink(&self, context: &Context, recovery_id: &str) -> Result<Value> {
        let spec = context
            .disk_shrink
            .as_ref()
            .context("shrink specification missing")?;
        self.shrink_adapter(&json!({"environment":self.binding.environment,"pool_guid":self.binding.pool_guid,"physical_trial":self.binding.physical_trial,
            "preparation_id":context.operation_id,"operation_id":recovery_id,"vm_id":context.vm_id(),"owner":context.owner,
            "old_mib":spec.previous_disk_mib,"target_mib":spec.target_disk_mib,"workspace_mib":spec.workspace_mib(),
            "root_partition_uuid":spec.root_partition_uuid,"helper_pid":std::process::id(),"phase":"abort"})).await
    }

    fn simulate_shrink(&self, world: &SimulatedWorld, p: &Value) -> Result<Value> {
        world.edit(|w| {
            let id = p["vm_id"].as_u64().context("VMID")? as u16;
            let vm = w.vms.get(&id).context("VM missing")?;
            ensure!(!vm.running, "VM is running");
            let op = p["preparation_id"].as_str().context("preparation ID")?;
            let work = format!("{POOL}/pve-dev-shrink-{op}");
            let root = format!("{CONTROL_DATASET}/{}/vm-{id}-disk-0", p["owner"].as_str().context("owner")?);
            let phase = p["phase"].as_str().context("phase")?;
            let old = p["old_mib"].as_u64().context("old size")?;
            let new = p["target_mib"].as_u64().context("target size")?;
            let key=format!("simulated:shrink-{op}");
            let mut receipts:Value=w.datasets[POOL].get(&key).map(|v|serde_json::from_str(v)).transpose()?.unwrap_or_else(||json!({}));
            if phase=="inspect" {
                let mut verified=json!({});
                if let Some(stage)=receipts.get("stage") {
                    let root_new=w.datasets.get(&root).is_some_and(|v|v.get("guid").map(String::as_str)==stage["replacement"]["guid"].as_str());
                    let original=w.datasets.get(if root_new {format!("{work}/original")}else{root.clone()}.as_str());
                    ensure!(original.is_some_and(|v|serde_json::to_value(v).ok().as_ref()==Some(&stage["source"])) || receipts.get("finish").is_some(),"retained original changed");
                    if original.is_some() || receipts.get("finish").is_some() {verified["shrink_stage"]=stage.clone();}
                    if root_new {
                        if let Some(value)=receipts.get("swap") {verified["shrink_swap"]=value.clone();}
                        if !w.vms[&id].config.contains_key("lock") && let Some(value)=receipts.get("unlock") {verified["shrink_unlock"]=value.clone();}
                        if !w.datasets.contains_key(&work) && let Some(value)=receipts.get("finish") {verified["shrink_finish"]=value.clone();}
                    }
                }
                let abort_safe=receipts.get("stage").is_some() && receipts.get("swap").is_none() && w.datasets.contains_key(&format!("{work}/replacement"));
                let aborted=receipts.get("abort").is_some() && !w.datasets.contains_key(&work) && !w.vms[&id].config.contains_key("lock");
                if aborted {verified=json!({"shrink_abort":receipts["abort"]});}
                if p["quick"]==true {return Ok(json!({"classification":"full_verification_required","candidate_phases":verified,"abort_candidate":abort_safe||aborted,"verified_phases":{},"abort_safe":false,"bytes_verified":false}));}
                let phase_receipts=receipts.as_object().context("shrink receipt map")?.iter().map(|(name,value)|(format!("shrink_{name}"),value.clone())).collect::<serde_json::Map<String,Value>>();
                return Ok(json!({"state":if aborted {"aborted"}else{"inspected"},"phase_receipts":phase_receipts,"verified_phases":verified,"abort_safe":abort_safe,"bytes_verified":true}));
            }
            if phase=="abort" {
                let stage=receipts.get("stage").context("no receipt authorizes preparation abort")?;
                ensure!(receipts.get("swap").is_none() && w.datasets.contains_key(&format!("{work}/replacement")),"cannot abort after a swap or uncertain replacement");
                ensure!(serde_json::to_value(&w.datasets[&root])?==stage["source"],"source changed before abort");
                ensure!(w.datasets.keys().filter(|n|n.starts_with(&format!("{work}/"))).count()==1 && !w.datasets[&work].contains_key("files"),"foreign workspace data");
                w.datasets.remove(&format!("{work}/replacement"));w.datasets.remove(&work);w.vms.get_mut(&id).context("VM")?.config.remove("lock");
                let result=json!({"aborted":true,"source":w.datasets[&root]});receipts["abort"]=result.clone();w.datasets.get_mut(POOL).context("pool")?.insert(key,receipts.to_string());return Ok(result);
            }
            let result: Result<Value> = match phase {
                "stage" => {
                    ensure!(w.available() >= (p["workspace_mib"].as_u64().context("workspace")? + SAFETY_FLOOR_MIB)*MIB, "insufficient actual workspace");
                    ensure!(!w.datasets.contains_key(&work), "occupied workspace");
                    let original = w.datasets[&root].clone();
                    ensure!(!original.contains_key("unsupported_layout"), "unsupported disk layout");
                    let amount = p["workspace_mib"].as_u64().context("workspace")? * MIB;
                    w.zfs_create(&work, None, &[("mountpoint","none".into()),("quota",amount.to_string()),("reservation",amount.to_string())])?;
                    w.zfs_create(&format!("{work}/replacement"), Some(new), &[("org.pve-dev:owner",original["org.pve-dev:owner"].clone()),("org.pve-dev:image",original["org.pve-dev:image"].clone())])?;
                    w.vms.get_mut(&id).context("VM")?.config.insert("lock".into(),"migrate".into());
                    Ok(json!({"source":original,"replacement":w.datasets[&format!("{work}/replacement")],"workspace":w.datasets[&work],"original_config":p["config"]}))
                }
                "swap" => {
                    let original = w.datasets.remove(&root).context("original disappeared")?;
                    ensure!(original.get("volsize") == Some(&(old*MIB).to_string()), "source size changed");
                    w.datasets.insert(format!("{work}/original"), original);
                    let replacement = w.datasets.remove(&format!("{work}/replacement")).context("replacement missing")?;
                    ensure!(replacement.get("guid").map(String::as_str) == p["stage"]["replacement"]["guid"].as_str(), "replacement GUID changed");
                    w.datasets.insert(root, replacement);
                    let config = &mut w.vms.get_mut(&id).context("VM")?.config;
                    let reference = config["scsi0"].split(',').next().context("attachment")?.to_owned();
                    config.insert("scsi0".into(),format!("{reference},size={new}M"));
                    Ok(json!({"config":config}))
                }
                "unlock" => { w.vms.get_mut(&id).context("VM")?.config.remove("lock"); Ok(json!({"prepared":true})) }
                "finish" => {
                    let expected = p["stage"]["source"]["guid"].as_str().context("original GUID missing")?;
                    ensure!(w.datasets.get(&format!("{work}/original")).and_then(|x|x.get("guid")).map(String::as_str) == Some(expected), "recovery GUID drifted");
                    ensure!(w.datasets.keys().filter(|x|x.starts_with(&format!("{work}/"))).count()==1, "foreign workspace child");
                    ensure!(!w.datasets[&work].contains_key("files"), "workspace has data");
                    w.datasets.remove(&format!("{work}/original"));w.datasets.remove(&work);
                    Ok(json!({"recovery_removed":true}))
                }
                _ => bail!("unknown shrink phase"),
            };
            let result=result?;
            receipts[phase]=result.clone();
            w.datasets.get_mut(POOL).context("pool")?.insert(key,receipts.to_string());
            Ok(result)
        })
    }
}
