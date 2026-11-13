//! Operator-triggered reconciliation. The quick public inspection is a review
//! token, never proof of disk contents. A queued recovery rechecks full durable
//! receipts and current data under the helper execution lock before any change.
use super::*;
use crate::control::vm_change::{JobExtension, RecoveryKind};

impl Host {
    fn original_context(&self, id: &str) -> Result<Context> {
        let j = self.journal()?;
        let r = j.jobs.get(id).context("operation not found")?;
        Ok(Context {
            environment: self.binding.environment.clone(),
            operation_id: id.into(),
            kind: r.job.kind.clone(),
            owner: r.job.owner.user_id.clone(),
            budget_mib: r.job.owner.budget_mib,
            vm: r.job.vm.clone(),
            boot_id: r
                .steps
                .iter()
                .find_map(|s| s.boot_id.clone())
                .unwrap_or_default(),
            policy_revision: r.job.owner.policy_revision,
            owner_policy: r.job.owner.policy(),
            policy_change: r.job.policy_change.clone(),
            disk_shrink: r.job.disk_shrink.clone(),
            extension: r.job.extension.clone(),
            scope: self.binding.namespace(),
            managed: self.binding.managed.clone(),
        })
    }
    async fn recovery_probe(&self, context: &Context, quick: bool) -> Result<Value> {
        match context.kind.as_str() {
            "start" if self.binding.managed.is_some() => self.inspect_failed_start(context).await,
            "shrink_vm" | "finish_vm_shrink" => {
                let mut inspected = context.clone();
                if !quick && context.kind == "shrink_vm" {
                    // A shrink abort receipt belongs to the recovery that
                    // performed it, while every preparation receipt remains
                    // bound to the original operation. Only a retained exact
                    // helper intent may supply that alternate phase identity.
                    let mut attempts=self.journal()?.jobs.values().filter(|job| {
                        job.job.owner.user_id==context.owner && job.steps.iter().any(|s|s.step==Step::Recover && s.phase==Phase::Intent)
                            && matches!(job.job.extension.as_deref(),Some(JobExtension::Recover(spec)) if spec.operation_id==context.operation_id && spec.kind==RecoveryKind::AbortPreparation)
                    }).cloned().collect::<Vec<_>>();
                    attempts.sort_by_key(|job| std::cmp::Reverse(job.sequence));
                    let mut ambiguous = None;
                    for previous in attempts {
                        inspected.operation_id = previous.job.operation_id.clone();
                        inspected
                            .disk_shrink
                            .as_mut()
                            .context("shrink specification missing")?
                            .preparation_id = Some(context.operation_id.clone());
                        let proof = self.inspect_shrink(&inspected, false).await?;
                        if proof["state"] != "ambiguous" {
                            return Ok(proof);
                        }
                        ambiguous = Some(proof);
                    }
                    if let Some(proof) = ambiguous {
                        return Ok(proof);
                    }
                }
                self.inspect_shrink(&inspected, quick).await
            }
            "grow_vm" => {
                if quick {
                    self.inspect_growth_quick(context).await
                } else {
                    self.inspect_growth(context).await
                }
            }
            _ => Ok(
                json!({"classification":"manual_review_required","candidate_phases":{},"verified_phases":{},"abort_candidate":false,"abort_safe":false,"reason":"no automatic reconciliation adapter exists for this operation"}),
            ),
        }
    }

    async fn inspect_failed_start(&self, context: &Context) -> Result<Value> {
        ensure!(
            self.binding.managed.is_some(),
            "failed-start acknowledgment requires a managed binding"
        );
        let journal = self.journal()?;
        let original = journal
            .jobs
            .get(&context.operation_id)
            .context("original start missing")?;
        self.binding.validate_job(&original.job)?;
        let upid = failed_start_receipt(original, self.backend.node(), context.vm_id())?;
        let machine = journal
            .machines
            .get(&context.vm_id())
            .context("managed machine missing")?;
        ensure!(
            machine.state == MachineState::Present
                && machine.owner == context.owner
                && machine.profile == profile(context.vm()?),
            "failed-start machine profile changed"
        );
        let owner = journal
            .owners
            .get(&context.owner)
            .context("managed owner missing")?;
        ensure!(
            owner.state == AccountState::Active
                && owner.ready
                && owner.policy_revision == context.policy_revision
                && Self::owner_policy(&journal, owner)? == context.owner_policy,
            "failed-start owner policy changed"
        );
        self.verify_machine(context).await?;
        let status = self
            .backend
            .api_get(&format!("{}/status/current", context.vm_path()), &[])
            .await?;
        let task = self
            .backend
            .api_get(
                &format!("/nodes/{}/tasks/{upid}/status", self.backend.node()),
                &[],
            )
            .await?;
        let active = self
            .backend
            .api_get(
                &format!("/nodes/{}/tasks", self.backend.node()),
                &[
                    ("source", "active".into()),
                    ("vmid", context.vm_id().to_string()),
                ],
            )
            .await?;
        verify_failed_start_observations(
            &upid,
            self.backend.node(),
            context.vm_id(),
            &status,
            &task,
            &active,
        )?;
        Ok(
            json!({"classification":"failed_managed_start_stopped","acknowledge_failed_start":true,
            "candidate_phases":{},"verified_phases":{},"abort_candidate":false,
            "failed_task":upid,"exitstatus":task["exitstatus"],"vm_status":"stopped",
            "managed_identity_verified":true,"active_vm_tasks":false}),
        )
    }
    pub(super) async fn inspect_operation(&self, id: &str) -> Result<Value> {
        let boot_id = self.check_host().await?;
        let journal = self.journal()?;
        let job = journal.jobs.get(id).context("operation not found")?;
        let context = self.original_context(id)?;
        let probe = if job.status == OperationStatus::Blocked {
            match self.recovery_probe(&context, true).await {
                Ok(value) => value,
                Err(error) => {
                    json!({"classification":"manual_review_required","candidate_phases":{},"abort_candidate":false,"error":format!("{error:#}")})
                }
            }
        } else {
            json!({"classification":if job.status.is_terminal(){"terminal"}else{"operation_running"},"candidate_phases":{},"abort_candidate":false})
        };
        let current = job.steps.iter().find(|s| s.phase != Phase::Verified);
        let mut allowed = Vec::new();
        if job.status == OperationStatus::Blocked {
            if let Some(step) = current {
                if step.phase == Phase::Intent
                    && probe["candidate_phases"].get(step.step.name()).is_some()
                {
                    allowed.push(RecoveryKind::ResumeVerifiedEffect);
                }
                // A Planned phase has no effect intent, but still must pass
                // actual owner/VM checks inside the queued recovery.
                if step.phase == Phase::Planned
                    && matches!(
                        context.kind.as_str(),
                        "grow_vm" | "shrink_vm" | "finish_vm_shrink"
                    )
                {
                    allowed.push(RecoveryKind::ResumeVerifiedEffect);
                }
            } else if !job.steps.is_empty()
                && job.steps.iter().all(|s| s.phase == Phase::Verified)
                && job
                    .steps
                    .iter()
                    .rev()
                    .find(|s| resize_phase(s.step))
                    .is_some_and(|s| probe["candidate_phases"].get(s.step.name()).is_some())
            {
                allowed.push(RecoveryKind::ResumeVerifiedEffect);
            }
            if probe["abort_candidate"] == true
                && matches!(context.kind.as_str(), "shrink_vm" | "grow_vm")
            {
                allowed.push(RecoveryKind::AbortPreparation);
            }
            if probe["acknowledge_failed_start"] == true {
                allowed.push(RecoveryKind::AcknowledgeFailedStart);
            }
        }
        let machine = job
            .job
            .vm
            .as_ref()
            .and_then(|vm| journal.machines.get(&vm.vm_id));
        let owner = journal.owners.get(&context.owner);
        let token = super::super::store::digest(&(
            "operation-inspection/1",
            &self.binding,
            &boot_id,
            &job.fingerprint,
            &job.status,
            &job.steps,
            machine,
            owner,
            &probe,
        ));
        Ok(
            json!({"operation_id":id,"inspection_id":token,"observed_at":super::super::store::now(),
            "classification":probe["classification"].as_str().unwrap_or("full_verification_required"),
            "allowed_actions":allowed,"proof":probe,
            "evidence":[{"name":"durable_intent","status":if job.status==OperationStatus::Blocked{"blocked"}else{"observed"},"detail":format!("{} at {}; {} verified steps",job.job.kind,job.step,job.steps.iter().filter(|s|s.phase==Phase::Verified).count())},
            {"name":"verification","status":"pending_full_verification","detail":if context.kind == "start" {"Acknowledge only this failed task after rechecking stopped state, no active VM task, and exact managed VM/storage identities. The VM stays stopped and its allocation remains held."} else {"A recovery job rechecks disk bytes, receipts and identities before accepting or aborting an effect."}}]}),
        )
    }
    pub(super) async fn run_recovery_step(
        &self,
        context: &Context,
        step: Step,
        record: &StepRecord,
    ) -> Result<()> {
        if record.phase == Phase::Verified {
            return Ok(());
        }
        ensure!(
            step == Step::Recover && record.phase == Phase::Planned,
            "uncertain recovery intent; inspect its receipt and do not replay it"
        );
        let Some(JobExtension::Recover(spec)) = context.extension.as_deref() else {
            bail!("recovery specification missing")
        };
        let original = self
            .journal()?
            .jobs
            .get(&spec.operation_id)
            .cloned()
            .context("original operation missing")?;
        ensure!(
            original.status == OperationStatus::Blocked
                && original.job.owner == self.journal()?.jobs[&context.operation_id].job.owner,
            "original operation or frozen owner policy changed"
        );
        let review = self.inspect_operation(&spec.operation_id).await?;
        ensure!(
            review["inspection_id"] == spec.inspection_id,
            "inspection is stale; inspect again before requesting recovery"
        );
        let allowed: Vec<RecoveryKind> = serde_json::from_value(review["allowed_actions"].clone())?;
        ensure!(
            allowed.contains(&spec.kind),
            "the reviewed inspection does not authorize this recovery kind"
        );
        let mut target = self.original_context(&spec.operation_id)?;
        target.boot_id = self.check_host().await?;
        self.verify_owner(&target).await?;
        let full = self.recovery_probe(&target, false).await?;
        self.update_step(context, step, |_, r| {
            r.witness = Some(json!({"inspection":review,"full_verification":full}));
            Ok(())
        })?;
        self.intent(context, step)?;
        self.backend.trip(step, FaultPoint::BeforeEffect)?;
        let outcome = match spec.kind {
            RecoveryKind::AcknowledgeFailedStart => {
                ensure!(
                    full["acknowledge_failed_start"] == true,
                    "full inspection does not prove a failed managed start"
                );
                // This decision has no external effect. Recheck *after* the
                // durable intent, then commit it atomically with cancellation.
                // An interrupted intent remains blocked rather than replayed.
                let fresh = self.inspect_failed_start(&target).await?;
                ensure!(
                    fresh == full,
                    "failed-start evidence changed before acknowledgment"
                );
                json!({"cancelled":true,"operation_id":spec.operation_id,"vm_stays_stopped":true,
                    "allocation_retained":true,"proof":fresh})
            }
            RecoveryKind::ResumeVerifiedEffect => {
                let pending = original.steps.iter().find(|s| s.phase != Phase::Verified);
                if pending.is_some_and(|pending| pending.phase == Phase::Intent) {
                    let pending = pending.expect("intent exists");
                    let proof = full["verified_phases"]
                        .get(pending.step.name())
                        .cloned()
                        .context("full inspection cannot prove the incomplete effect")?;
                    match target.kind.as_str() {
                        "grow_vm" => {
                            self.accept_growth_proof(&target, pending.step, proof)
                                .await?
                        }
                        "shrink_vm" | "finish_vm_shrink" => {
                            self.accept_shrink_proof(&target, pending.step, proof)
                                .await?
                        }
                        _ => bail!("no receipt-based reconciliation adapter"),
                    }
                } else {
                    ensure!(
                        pending.is_none_or(|pending| pending.phase == Phase::Planned),
                        "invalid recovery phase"
                    );
                    self.verify_resize_continuation(&target, &original, &full)
                        .await?;
                }
                json!({"resumed":true,"operation_id":spec.operation_id})
            }
            RecoveryKind::AbortPreparation => {
                let aborted = if target.kind == "grow_vm" {
                    "grow_abort"
                } else {
                    "shrink_abort"
                };
                let proof = if full["state"] == "aborted" {
                    full["verified_phases"]
                        .get(aborted)
                        .cloned()
                        .context("completed abort lacks an independently verified receipt")?
                } else {
                    ensure!(
                        full["abort_safe"] == true,
                        "full inspection does not authorize preparation abort"
                    );
                    if target.kind == "grow_vm" {
                        self.abort_growth(&target).await?
                    } else {
                        self.abort_shrink(&target, &context.operation_id).await?
                    }
                };
                ensure!(
                    proof["aborted"] == true,
                    "abort did not produce a verified receipt"
                );
                json!({"cancelled":true,"operation_id":spec.operation_id,"proof":proof})
            }
        };
        self.backend.trip(step, FaultPoint::AfterEffect)?;
        // The recovery decision and original terminal/queued state become
        // durable together. Capacity cannot disappear between these records.
        self.update_step(context,step,|j,r|{
            let original=j.jobs.get_mut(&spec.operation_id).context("original operation missing")?;
            ensure!(original.status==OperationStatus::Blocked,"original operation changed during recovery");
            if spec.kind != RecoveryKind::AcknowledgeFailedStart {original.error=None;}
            original.updated_at=Some(super::super::store::now());
            match spec.kind {
                RecoveryKind::ResumeVerifiedEffect=>{original.status=OperationStatus::Queued;original.finished_at=None;}
                RecoveryKind::AbortPreparation=>{original.status=OperationStatus::Cancelled;original.step="cancelled".into();original.finished_at=original.updated_at;if target.kind=="shrink_vm" {j.disk_shrinks.remove(&target.vm_id());}}
                RecoveryKind::AcknowledgeFailedStart=>{original.status=OperationStatus::Cancelled;original.step="failed_start_acknowledged".into();original.finished_at=original.updated_at;}
            }
            for (id,older) in &mut j.jobs {
                if id!=&context.operation_id && !older.status.is_terminal() && matches!(older.job.extension.as_deref(),Some(JobExtension::Recover(s)) if s.operation_id==spec.operation_id) {
                    older.status=OperationStatus::Cancelled;older.step="superseded_by_verified_recovery".into();older.error=None;older.updated_at=Some(super::super::store::now());older.finished_at=older.updated_at;
                }
            }
            r.phase=Phase::Verified;r.identity=Some(outcome);r.boot_id=Some(context.boot_id.clone());
            Ok(())
        })?;
        self.backend.trip(step, FaultPoint::AfterReceipt)?;
        Ok(())
    }

    async fn verify_resize_continuation(
        &self,
        target: &Context,
        original: &HostJob,
        full: &Value,
    ) -> Result<()> {
        let mut latest = None;
        for record in original
            .steps
            .iter()
            .filter(|s| resize_phase(s.step) && s.phase == Phase::Verified)
        {
            let identity = record
                .identity
                .as_ref()
                .context("verified resize phase has no immutable identity")?;
            ensure!(
                full["phase_receipts"].get(record.step.name()) == Some(identity),
                "historical resize receipt {} changed or is missing",
                record.step.name()
            );
            latest = Some(record);
        }
        let Some(latest) = latest else {
            return self.verify_machine(target).await;
        };
        ensure!(
            full["verified_phases"].get(latest.step.name()) == latest.identity.as_ref(),
            "latest verified resize phase does not match fresh physical state"
        );
        let mut observed = target.clone();
        if original.steps.iter().any(|s| {
            s.phase == Phase::Verified && matches!(s.step, Step::GrowFilesystem | Step::ShrinkSwap)
        }) {
            let size = if let Some(shrink) = &target.disk_shrink {
                shrink.target_disk_mib
            } else if let Some(JobExtension::Grow(grow)) = target.extension.as_deref() {
                grow.target_disk_mib
            } else {
                bail!("resize target specification missing")
            };
            observed.vm.as_mut().context("resize VM missing")?.disk_mib = size;
        }
        let journal = self.journal()?;
        let machine = journal
            .machines
            .get(&target.vm_id())
            .context("resize machine missing")?;
        ensure!(
            machine.profile == profile(observed.vm()?),
            "resize profile differs from verified phase history"
        );
        for stable in [Step::Vm, Step::CloudInit] {
            ensure!(
                self.observe(&observed, stable).await?.as_ref()
                    == machine.identities.get(stable.name()),
                "stable VM identity changed before resize continuation"
            );
        }
        Ok(())
    }
}

pub(super) fn failed_start_receipt(job: &HostJob, node: &str, vm_id: u16) -> Result<String> {
    ensure!(
        job.status == OperationStatus::Blocked
            && job.job.kind == "start"
            && job.job.vm.as_ref().is_some_and(|vm| vm.vm_id == vm_id)
            && job.steps.len() == 2
            && job.steps[0].step == Step::Verify
            && job.steps[0].phase == Phase::Verified
            && job.steps[1].step == Step::Start
            && job.steps[1].phase == Phase::Intent,
        "only a verified managed start with a retained failed-task intent can be acknowledged"
    );
    let upid = job.steps[1]
        .upid
        .as_deref()
        .context("start intent has no authoritative task receipt")?;
    verify_start_upid(upid, node, vm_id)?;
    Ok(upid.to_owned())
}

fn verify_start_upid(upid: &str, node: &str, vm_id: u16) -> Result<()> {
    let parts = upid.split(':').collect::<Vec<_>>();
    ensure!(
        valid_upid_on(upid, node)
            && parts.len() == 9
            && parts[0] == "UPID"
            && parts[1] == node
            && parts[2..5].iter().all(|v| !v.is_empty()
                && v.len() <= 16
                && v.bytes().all(|c| c.is_ascii_hexdigit()))
            && parts[5] == "qmstart"
            && parts[6] == vm_id.to_string()
            && parts[7] == "root@pam"
            && parts[8].is_empty(),
        "failed task receipt does not match this exact managed qmstart"
    );
    Ok(())
}

fn verify_failed_start_observations(
    upid: &str,
    node: &str,
    vm_id: u16,
    status: &Value,
    task: &Value,
    active: &Value,
) -> Result<()> {
    verify_start_upid(upid, node, vm_id)?;
    ensure!(
        status["status"] == "stopped"
            && status["vmid"].as_u64() == Some(u64::from(vm_id))
            && status.get("lock").is_none_or(Value::is_null)
            && status
                .get("pid")
                .is_none_or(|v| v.is_null() || v.as_u64() == Some(0)),
        "VM is running, locked, or has an unexpected process identity"
    );
    let exit = task["exitstatus"]
        .as_str()
        .context("task failure result is unknown")?;
    ensure!(
        task["status"] == "stopped"
            && !exit.trim().is_empty()
            && exit.len() <= 4096
            && !matches!(
                exit.trim().to_ascii_lowercase().as_str(),
                "ok" | "unknown" | "unexpected status"
            ),
        "task has not authoritatively failed; running, successful or unknown tasks cannot be acknowledged"
    );
    for (key, expected) in [
        ("upid", upid.to_owned()),
        ("node", node.to_owned()),
        ("id", vm_id.to_string()),
        ("type", "qmstart".to_owned()),
        ("user", "root@pam".to_owned()),
    ] {
        ensure!(
            task.get(key).is_none_or(|value| text(value) == expected),
            "task {key} identity differs"
        );
    }
    for task in active.as_array().context("active task list is unknown")? {
        let receipt = task["upid"]
            .as_str()
            .context("active task has no receipt")?;
        ensure!(
            valid_upid_on(receipt, node),
            "active task receipt is invalid"
        );
        let parts = receipt.split(':').collect::<Vec<_>>();
        ensure!(parts.len() == 9, "active task receipt is malformed");
        ensure!(
            text(&task["id"]) != vm_id.to_string() && parts[6] != vm_id.to_string(),
            "VM still has an active native task"
        );
    }
    Ok(())
}

fn resize_phase(step: Step) -> bool {
    matches!(
        step,
        Step::GrowPrepare
            | Step::GrowVolume
            | Step::GrowPartition
            | Step::GrowFilesystem
            | Step::GrowUnlock
            | Step::ShrinkStage
            | Step::ShrinkSwap
            | Step::ShrinkUnlock
            | Step::ShrinkFinish
    )
}

#[cfg(test)]
mod failed_start_tests {
    use super::*;

    const UPID: &str = "UPID:pve1:0000DA05:031CA796:6AF6225F:qmstart:9201:root@pam:";

    fn original() -> HostJob {
        serde_json::from_value(json!({
            "sequence":1,"fingerprint":"frozen-start","job":{
                "operation_id":"failed-start","kind":"start","owner":{
                    "user_id":"erin","role":"admin","budget_mib":8192,
                    "min_memory_budget_mib":512,"max_memory_budget_mib":2048,
                    "max_cores":4,"max_vms":2,"ready":true},
                "vm":{"vm_id":9201,"owner":"erin","name":"new-vm","disk_mib":4096,
                    "memory_mib":1024,"min_memory_mib":512,"cores":1,"state":"stopped"}},
            "status":"blocked","step":"start","error":"start failed: QEMU exited with code 1",
            "attempts":1,"steps":[
                {"step":"verify","phase":"verified","boot_id":"boot","upid":null,"identity":{"verified":true},"witness":null},
                {"step":"start","phase":"intent","boot_id":"boot","upid":UPID,"identity":null,"witness":null}]
        })).unwrap()
    }
    fn stopped() -> Value {
        json!({"status":"stopped","vmid":9201})
    }
    fn failed() -> Value {
        json!({"status":"stopped","exitstatus":"start failed: QEMU exited with code 1",
        "upid":UPID,"node":"pve1","id":"9201","type":"qmstart","user":"root@pam"})
    }
    fn verify(status: &Value, task: &Value, active: &Value) -> Result<()> {
        verify_failed_start_observations(UPID, "pve1", 9201, status, task, active)
    }

    #[test]
    fn failed_start_requires_an_exact_retained_failed_task_intent() {
        let original = original();
        assert_eq!(failed_start_receipt(&original, "pve1", 9201).unwrap(), UPID);
        for kind in [
            "stop",
            "shutdown",
            "delete",
            "create_guest",
            "recover_operation",
        ] {
            let mut bad = original.clone();
            bad.job.kind = kind.into();
            assert!(failed_start_receipt(&bad, "pve1", 9201).is_err());
        }
        for status in [
            OperationStatus::Queued,
            OperationStatus::Running,
            OperationStatus::Verified,
            OperationStatus::Cancelled,
        ] {
            let mut bad = original.clone();
            bad.status = status;
            assert!(failed_start_receipt(&bad, "pve1", 9201).is_err());
        }
        for phase in [Phase::Planned, Phase::Verified] {
            let mut bad = original.clone();
            bad.steps[1].phase = phase;
            assert!(failed_start_receipt(&bad, "pve1", 9201).is_err());
        }
        let mut bad = original.clone();
        bad.steps[0].phase = Phase::Planned;
        assert!(failed_start_receipt(&bad, "pve1", 9201).is_err());
        let mut bad = original.clone();
        bad.steps[1].upid = None;
        assert!(failed_start_receipt(&bad, "pve1", 9201).is_err());
        for upid in [
            UPID.replace("pve1", "pve2"),
            UPID.replace("qmstart", "qmstop"),
            UPID.replace("9201", "103"),
            UPID.replace("root@pam", "alice@pve"),
            UPID.replace("9201", "09201"),
            UPID.replace("0000DA05", "not-hex"),
            UPID.trim_end_matches(':').into(),
        ] {
            let mut bad = original.clone();
            bad.steps[1].upid = Some(upid);
            assert!(failed_start_receipt(&bad, "pve1", 9201).is_err());
        }
        assert!(failed_start_receipt(&original, "pve1", 103).is_err());
    }

    #[test]
    fn running_successful_missing_unknown_or_foreign_tasks_never_authorize_acknowledgment() {
        verify(&stopped(), &failed(), &json!([])).unwrap();
        for exit in [
            json!(null),
            json!(""),
            json!(" "),
            json!("OK"),
            json!("unknown"),
            json!("unexpected status"),
            json!(1),
        ] {
            let mut task = failed();
            task["exitstatus"] = exit;
            assert!(verify(&stopped(), &task, &json!([])).is_err(), "{task}");
        }
        for status in [json!("running"), json!("unknown"), json!(null)] {
            let mut task = failed();
            task["status"] = status;
            assert!(verify(&stopped(), &task, &json!([])).is_err());
        }
        for (key, value) in [
            ("upid", UPID.replace("9201", "103")),
            ("node", "pve2".into()),
            ("id", "103".into()),
            ("type", "qmstop".into()),
            ("user", "alice@pve".into()),
        ] {
            let mut task = failed();
            task[key] = json!(value);
            assert!(verify(&stopped(), &task, &json!([])).is_err(), "{task}");
        }
        for active in [
            json!(null),
            json!([{}]),
            json!([{"upid":UPID}]),
            json!([{"upid":UPID.replace("qmstart","qmstop"),"id":"9201"}]),
            json!([{"upid":UPID.replace("pve1","pve2"),"id":"103"}]),
        ] {
            assert!(verify(&stopped(), &failed(), &active).is_err(), "{active}");
        }
        verify(
            &stopped(),
            &failed(),
            &json!([{"upid":UPID.replace("9201","103"),"id":"103"}]),
        )
        .unwrap();
    }

    #[test]
    fn stopped_guest_with_no_process_or_lock_is_required_even_after_a_failed_task() {
        for (key, value) in [
            ("status", json!("running")),
            ("status", json!("unknown")),
            ("vmid", json!(103)),
            ("vmid", json!(null)),
            ("pid", json!(123)),
            ("pid", json!("0")),
            ("lock", json!("backup")),
        ] {
            let mut status = stopped();
            status[key] = value;
            assert!(verify(&status, &failed(), &json!([])).is_err(), "{status}");
        }
        let mut status = stopped();
        status["pid"] = json!(0);
        status["lock"] = json!(null);
        verify(&status, &failed(), &json!([])).unwrap();
    }
}
