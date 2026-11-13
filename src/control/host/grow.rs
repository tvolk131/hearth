//! Single-use offline ext4/GPT growth with durable, independently inspectable
//! effects. A lost command reply never authorizes replay of a mutating phase.
use super::*;
use crate::control::vm_change::{GrowthSpec, JobExtension};

const SHARED_WORKER: &str = include_str!("../shrink_worker.py");
const GROW_WORKER: &str = include_str!("../grow_worker.py");
/// Both sources are fixed at compile time. `__name__` prevents the shared
/// library's separate shrink entrypoint from running.
pub(super) fn grow_script() -> String {
    format!("__name__='pve_dev_resize_shared'\n{SHARED_WORKER}\n__name__='__main__'\n{GROW_WORKER}")
}

const GROW_SOURCE_KEYS: &[&str] = &[
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

fn growth_spec(context: &Context) -> Result<&GrowthSpec> {
    match context.extension.as_deref() {
        Some(JobExtension::Grow(spec)) if spec.valid() => Ok(spec),
        _ => bail!("growth requires its immutable valid specification"),
    }
}

fn grow_phase(step: Step) -> Result<&'static str> {
    Ok(match step {
        Step::GrowPrepare => "prepare",
        Step::GrowVolume => "volume",
        Step::GrowPartition => "partition",
        Step::GrowFilesystem => "filesystem",
        Step::GrowUnlock => "unlock",
        _ => bail!("not a growth step"),
    })
}

fn simulated_growth_config(
    w: &World,
    id: u16,
    expected: &Value,
    grown: bool,
    size_mib: u64,
    locked: bool,
) -> Result<()> {
    let mut expected = expected
        .as_object()
        .context("growth configuration receipt missing")?
        .clone();
    expected.remove("digest");
    if grown {
        let attachment = expected["scsi0"]
            .as_str()
            .context("root attachment receipt")?;
        let options = attachment
            .split(',')
            .filter(|part| !part.starts_with("size="))
            .collect::<Vec<_>>()
            .join(",");
        expected.insert("scsi0".into(), json!(format!("{options},size={size_mib}M")));
    }
    if locked {
        expected.insert("lock".into(), json!("migrate"));
    } else {
        expected.remove("lock");
    }
    let mut actual = serde_json::to_value(&w.vms[&id].config)?;
    actual
        .as_object_mut()
        .context("simulated configuration missing")?
        .remove("digest");
    ensure!(
        actual == Value::Object(expected),
        "complete growth VM configuration changed"
    );
    Ok(())
}

impl Host {
    fn growth_target(&self, context: &Context) -> Result<Context> {
        let mut target = context.clone();
        target.vm.as_mut().context("growth VM missing")?.disk_mib =
            growth_spec(context)?.target_disk_mib;
        Ok(target)
    }

    fn growth_proof(&self, context: &Context, step: Step) -> Result<Value> {
        self.journal()?
            .jobs
            .get(&context.operation_id)
            .context("growth job missing")?
            .steps
            .iter()
            .find(|r| r.step == step && r.phase == Phase::Verified)
            .and_then(|r| r.identity.clone())
            .context("verified growth preparation proof missing")
    }

    fn growth_common_input(&self, context: &Context, phase: &str) -> Result<Value> {
        let spec = growth_spec(context)?;
        if let Some(binding) = &self.binding.physical_trial {
            ensure!(
                binding.scope.features.contains("resize"),
                "physical scope does not authorize resizing"
            );
        }
        Ok(
            json!({"environment":self.binding.environment,"pool_guid":self.binding.pool_guid,
            "physical_trial":self.binding.physical_trial,"operation_id":context.operation_id,
            "vm_id":context.vm_id(),"owner":context.owner,"old_mib":spec.previous_disk_mib,
            "target_mib":spec.target_disk_mib,"root_partition_uuid":spec.root_partition_uuid,
            "helper_pid":std::process::id(),"phase":phase}),
        )
    }

    async fn growth_input(&self, context: &Context, step: Step) -> Result<Value> {
        let mut input = self.growth_common_input(context, grow_phase(step)?)?;
        if step == Step::GrowPrepare {
            self.verify_machine(context).await?;
            ensure!(
                self.status(context).await? == "stopped",
                "growth requires an actually stopped guest"
            );
            input["config"] = self.managed_config(context, true).await?;
            input["source"] = json!(
                self.backend
                    .zfs_get(&context.root_volume(), GROW_SOURCE_KEYS)
                    .await?
            );
        } else {
            input["baseline"] = self.growth_proof(context, Step::GrowPrepare)?;
        }
        Ok(input)
    }

    async fn growth_adapter(&self, input: &Value) -> Result<Value> {
        match &self.backend {
            Backend::Lab(_) => {
                let budget = disk_maintenance_timeout(
                    input["old_mib"].as_u64().context("old size missing")?.max(
                        input["target_mib"]
                            .as_u64()
                            .context("target size missing")?,
                    ),
                );
                let script = grow_script();
                Worker {
                    label: "growth adapter",
                    script: &script,
                    args: &[],
                    input: Some(input),
                    budget,
                    grace: DEFAULT_GRACE,
                    max_output: MAX_HOST_MESSAGE,
                    gid: Some(0),
                    timeout_hint: "inspect retained operation state before recovery",
                }
                .run_json()
                .await
            }
            Backend::Simulated(world) => self.simulate_growth(world, input),
        }
    }

    pub(super) async fn inspect_growth(&self, context: &Context) -> Result<Value> {
        self.check_host().await?;
        self.growth_adapter(&self.growth_common_input(context, "inspect")?)
            .await
    }

    pub(super) async fn inspect_growth_quick(&self, context: &Context) -> Result<Value> {
        self.check_host().await?;
        let mut input = self.growth_common_input(context, "inspect")?;
        input["quick"] = json!(true);
        self.growth_adapter(&input).await
    }

    pub(super) async fn abort_growth(&self, context: &Context) -> Result<Value> {
        self.check_host().await?;
        self.growth_adapter(&self.growth_common_input(context, "abort")?)
            .await
    }

    pub(super) async fn run_grow_step(
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
            "ambiguous growth intent; inspect its durable receipt and actual disk; no automatic filesystem replay"
        );
        self.check_host().await?;
        if step == Step::GrowUnlock {
            // Machine profile/backing identity must already be durable before
            // removing the physical fence.
            self.verify_machine(&self.growth_target(context)?).await?;
        }
        let input = self.growth_input(context, step).await?;
        self.intent(context, step)?;
        self.backend.trip(step, FaultPoint::BeforeEffect)?;
        let proof = self.growth_adapter(&input).await?;
        self.backend.trip(step, FaultPoint::AfterEffect)?;
        self.accept_growth_proof(context, step, proof).await?;
        self.backend.trip(step, FaultPoint::AfterReceipt)?;
        Ok(())
    }

    pub(super) async fn accept_growth_proof(
        &self,
        context: &Context,
        step: Step,
        proof: Value,
    ) -> Result<()> {
        grow_phase(step)?;
        ensure!(
            self.step_record(context, step)?.phase == Phase::Intent,
            "growth proof requires its existing durable intent"
        );
        self.check_host().await?;
        self.verify_owner(context).await?;
        let actual = self
            .backend
            .zfs_get(&context.root_volume(), GROW_SOURCE_KEYS)
            .await?;
        ensure!(
            json!(actual) == proof["source"],
            "actual growth disk differs from completion proof"
        );
        let baseline = if step == Step::GrowPrepare {
            proof.clone()
        } else {
            self.growth_proof(context, Step::GrowPrepare)?
        };
        ensure!(
            proof["source"]["guid"] == baseline["source"]["guid"]
                && proof["source"]["createtxg"] == baseline["source"]["createtxg"],
            "growth source identity changed"
        );
        let expected_size = if step == Step::GrowPrepare {
            growth_spec(context)?.previous_disk_mib
        } else {
            growth_spec(context)?.target_disk_mib
        };
        ensure!(
            proof["source"]["volsize"]
                .as_str()
                .and_then(|v| v.parse::<u64>().ok())
                == Some(expected_size * MIB),
            "growth proof has an unexpected volume size"
        );
        if step == Step::GrowPrepare {
            self.verify_machine(context).await?;
        }
        let mut replacements = BTreeMap::new();
        if step == Step::GrowFilesystem {
            let target = self.growth_target(context)?;
            let machine = self
                .journal()?
                .machines
                .get(&context.vm_id())
                .context("growth machine missing")?
                .clone();
            for stable in [Step::Vm, Step::CloudInit] {
                ensure!(
                    self.observe(&target, stable).await?.as_ref()
                        == machine.identities.get(stable.name()),
                    "stable guest identity changed during growth"
                );
            }
            for changed in [Step::Volume, Step::Attach] {
                replacements.insert(
                    changed.name().into(),
                    self.observe(&target, changed)
                        .await?
                        .context("grown backing/attachment missing")?,
                );
            }
            if context.vm()?.guest.is_some() {
                let image = self
                    .observe(&target, Step::Image)
                    .await?
                    .context("grown image provenance missing")?;
                ensure!(
                    machine.identities.get(Step::Image.name()) == Some(&image),
                    "image provenance changed during growth"
                );
                replacements.insert(Step::Image.name().into(), image);
            }
            let before = self.growth_proof(context, Step::GrowPrepare)?;
            ensure!(
                replacements["volume"]["guid"] == before["source"]["guid"]
                    && replacements["volume"]["createtxg"] == before["source"]["createtxg"],
                "growth replaced the original disk identity"
            );
        }
        if step == Step::GrowUnlock {
            self.verify_machine(&self.growth_target(context)?).await?;
        }
        self.update_step(context, step, |journal, record| {
            if step == Step::GrowFilesystem {
                let machine = journal
                    .machines
                    .get_mut(&context.vm_id())
                    .context("growth machine missing")?;
                machine.profile.disk_mib = growth_spec(context)?.target_disk_mib;
                machine.identities.extend(replacements);
                if let Some(pinned) = machine.pinned_config.as_mut() {
                    pinned["scsi0"] = proof["config"]["scsi0"].clone();
                }
            }
            record.phase = Phase::Verified;
            record.boot_id = Some(context.boot_id.clone());
            record.identity = Some(proof);
            Ok(())
        })?;
        Ok(())
    }

    fn simulate_growth(&self, world: &SimulatedWorld, p: &Value) -> Result<Value> {
        world.edit(|w| {
            let id = u16::try_from(p["vm_id"].as_u64().context("growth VM ID")?)?;
            let operation = p["operation_id"].as_str().context("growth operation ID")?;
            let owner = p["owner"].as_str().context("growth owner")?;
            let root = format!("{CONTROL_DATASET}/{owner}/vm-{id}-disk-0");
            let key = format!("simulated:growth-{operation}");
            let old = p["old_mib"].as_u64().context("old growth size")?;
            let new = p["target_mib"].as_u64().context("target growth size")?;
            let phase = p["phase"].as_str().context("growth phase")?;
            let vm = w.vms.get(&id).context("growth VM missing")?;
            ensure!(!vm.running && vm.config.get("protection").map(String::as_str) == Some("1") && vm.pending_config.is_empty(), "growth target must be stopped, protected and without pending config");
            let actual = w.datasets.get(&root).context("growth root missing")?;
            ensure!(!actual.contains_key("unsupported_layout"), "unsupported disk layout");
            ensure!(actual.get("type").map(String::as_str)==Some("volume")
                && actual.get("origin").map(String::as_str).unwrap_or("-")=="-"
                && actual.get("readonly").map(String::as_str).unwrap_or("off")=="off"
                && actual["refreservation"].parse::<u64>()? >= actual["volsize"].parse::<u64>()?,
                "growth source must remain independent, writable and thick");
            ensure!(!w.datasets.keys().any(|name|name.starts_with(&format!("{root}@"))
                ||name.starts_with(&format!("{root}#"))||name.starts_with(&format!("{root}/"))),
                "growth refuses snapshots, bookmarks or descendants");
            let mut state: Value = actual.get(&key).map(|v|serde_json::from_str(v)).transpose()?.unwrap_or(Value::Null);
            if phase == "prepare" {
                ensure!(state.is_null() && !vm.config.contains_key("lock"), "growth baseline/fence already exists");
                simulated_growth_config(w,id,&p["config"],false,old,false)?;
                ensure!(actual.get("volsize") == Some(&(old*MIB).to_string()), "growth source size changed");
                ensure!(!w.datasets.keys().any(|name| name.starts_with(&format!("{root}@")) || name.starts_with(&format!("{root}/"))), "growth snapshots refused");
                let baseline = json!({"source":p["source"],"original_config":p["config"],"prepared":true,"simulated":true});
                state = json!({"environment":p["environment"],"operation_id":operation,"vm_id":id,"old_mib":old,"target_mib":new,
                    "baseline":baseline,"partition_mib":old,"filesystem_mib":old,"receipts":{"grow_prepare":baseline}});
                w.vms.get_mut(&id).context("VM")?.config.insert("lock".into(),"migrate".into());
                w.datasets.get_mut(&root).context("root")?.insert("org.pve-dev:grow".into(),operation.into());
            } else {
                ensure!(state["environment"] == p["environment"] && state["operation_id"] == operation && state["vm_id"] == id
                    && state["old_mib"] == old && state["target_mib"] == new, "simulated growth receipt binding changed");
                let baseline = &state["baseline"];
                for key in ["guid","createtxg","org.pve-dev:owner","org.pve-dev:image"] {
                    let expected = baseline["source"][key].as_str().context("growth source receipt missing identity")?;
                    ensure!(actual.get(key).map(String::as_str).unwrap_or("-") == expected, "growth source {key} changed");
                }
                ensure!(p.get("baseline").is_none_or(|value| value == baseline), "growth baseline differs from receipt");
                let size = actual["volsize"].parse::<u64>()?/MIB;
                ensure!(size == old || size == new, "unexpected growth size");
                let marker = actual.get("org.pve-dev:grow").map(String::as_str).unwrap_or("-");
                let locked = vm.config.get("lock").map(String::as_str) == Some("migrate");
                simulated_growth_config(w,id,&baseline["original_config"],state["filesystem_mib"]==new,new,locked)?;
                if phase == "inspect" {
                    let fully_grown = size == new && state["partition_mib"] == new && state["filesystem_mib"] == new;
                    let observed = if !locked && marker == "-" && fully_grown {"unlocked"}
                        else if locked && marker == operation && fully_grown {"filesystem_extended"}
                        else if locked && marker == operation && size == new && state["partition_mib"] == new {"partition_extended"}
                        else if locked && marker == operation && size == new {"volume_extended"}
                        else if locked && marker == operation && size == old {"prepared"}
                        else if !locked && marker == "-" && size == old && state["receipts"].get("grow_abort").is_some() {"aborted"}
                        else {"ambiguous"};
                    let current=match observed {"prepared"=>"grow_prepare","volume_extended"=>"grow_volume","partition_extended"=>"grow_partition",
                        "filesystem_extended"=>"grow_filesystem","unlocked"=>"grow_unlock","aborted"=>"grow_abort",_=>""};
                    let mut verified=json!({});
                    if let Some(proof)=state["receipts"].get(current) {verified[current]=proof.clone();}
                    if p["quick"].as_bool().unwrap_or(false) {
                        return Ok(json!({"state":"full_verification_required","classification":"full_verification_required",
                            "verified_phases":{},"candidate_phases":verified,
                            "abort_safe":false,"abort_candidate":matches!(observed,"prepared"|"aborted"),"bytes_verified":false,"simulated":true}));
                    }
                    return Ok(json!({"state":observed,"phase_receipts":state["receipts"],"verified_phases":verified,"abort_safe":observed=="prepared","simulated":true}));
                }
                ensure!(locked && marker == operation, "growth fence changed");
                if phase == "abort" {
                    ensure!(size == old && state["partition_mib"] == old && state["filesystem_mib"] == old
                        && state["receipts"].as_object().context("receipts")?.len()==1, "growth abort refused after extension");
                    w.vms.get_mut(&id).context("VM")?.config.remove("lock");
                    w.datasets.get_mut(&root).context("root")?.remove("org.pve-dev:grow");
                    let proof=json!({"aborted":true,"source":baseline["source"],"simulated":true});
                    state["receipts"]["grow_abort"]=proof.clone();
                    w.datasets.get_mut(&root).context("root")?.insert(key,serde_json::to_string(&state)?);
                    return Ok(proof);
                }
                let receipt = format!("grow_{phase}");
                ensure!(state["receipts"].get(&receipt).is_none(), "growth phase already has a durable receipt; inspect instead");
                match phase {
                    "volume" => {
                        ensure!(size==old && state["partition_mib"]==old && state["filesystem_mib"]==old, "unexpected pre-growth state");
                        let properties=w.datasets.get_mut(&root).context("root")?;
                        properties.insert("volsize".into(),(new*MIB).to_string());
                        properties.insert("refreservation".into(),(new*MIB+MIB).to_string());
                        let dataset=format!("{CONTROL_DATASET}/{owner}");
                        let quota=w.datasets[&dataset]["quota"].parse::<u64>()?;
                        ensure!(w.used(&dataset)<=quota && w.used(POOL)<=w.pool_size_mib*MIB,"growth exceeds actual reserved capacity");
                    }
                    "partition" => {
                        ensure!(size==new && state["partition_mib"]==old && state["filesystem_mib"]==old
                            && state["receipts"].get("grow_volume").is_some(),"missing verified volume growth");
                        state["partition_mib"]=json!(new);
                    }
                    "filesystem" => {
                        ensure!(size==new && state["partition_mib"]==new && state["filesystem_mib"]==old
                            && state["receipts"].get("grow_partition").is_some(),"missing verified partition growth");
                        state["filesystem_mib"]=json!(new);
                        let config=&mut w.vms.get_mut(&id).context("VM")?.config;
                        let reference=config["scsi0"].split(',').filter(|part|!part.starts_with("size=")).collect::<Vec<_>>().join(",");
                        config.insert("scsi0".into(),format!("{reference},size={new}M"));
                    }
                    "unlock" => {
                        ensure!(size==new && state["partition_mib"]==new && state["filesystem_mib"]==new
                            && state["receipts"].get("grow_filesystem").is_some(),"grown filesystem receipt missing");
                        w.vms.get_mut(&id).context("VM")?.config.remove("lock");
                        w.datasets.get_mut(&root).context("root")?.remove("org.pve-dev:grow");
                    }
                    _ => bail!("unknown growth phase"),
                }
                let source: BTreeMap<String,String> = GROW_SOURCE_KEYS.iter().map(|key| {
                    ((*key).into(),w.datasets[&root].get(*key).cloned().unwrap_or_else(||"-".into()))
                }).collect();
                let proof = json!({"source":source,"config":w.vms[&id].config,"grown":phase=="unlock","simulated":true});
                state["receipts"][receipt]=proof;
            }
            let answer=state["receipts"][format!("grow_{phase}")].clone();
            w.datasets.get_mut(&root).context("growth root")?.insert(key,serde_json::to_string(&state)?);
            Ok(answer)
        })
    }
}
