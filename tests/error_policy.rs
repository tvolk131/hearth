//! Liveness of the helper's job runner: retryable failures retry with a
//! bounded budget, effect errors reconcile like crashes, permanent helper
//! rejections settle the controller operation, migrations are blocked only by
//! typed ambiguity, and a stopping helper finishes its in-flight step.

use std::{path::PathBuf, sync::Arc, time::Duration};

use hearth::control::{
    host::{
        EffectFault, ErrorClass, HelperError, HelperErrorCode, Host, HostConfig, HostRequest,
        Phase, RetryPolicy, SimulatedWorld, Step, World, rpc, serve_until,
    },
    migration::Phase as TransferPhase,
    migration_transport::{blocks_transfer, peer_reply},
    protocol::{OperationStatus, Role, User, Vm, VmState},
    store::{Binding, Job, Store},
    vm_change::{JobExtension, RecoveryKind, RecoverySpec},
};
use iroh::SecretKey;
use serde_json::{Value, json};

const ENV: &str = "0123456789abcdef0123456789abcdef";

struct Lab {
    root: tempfile::TempDir,
    state: PathBuf,
    world: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let world = root.path().join("world.json");
        Self { root, state, world }
    }

    fn config(&self, retry: RetryPolicy) -> HostConfig {
        let mut config = HostConfig::simulated(self.state.clone(), ENV, self.world.clone());
        config.retry = retry;
        config
    }

    async fn open(&self, retry: RetryPolicy) -> Host {
        Host::open(self.config(retry)).await.unwrap()
    }

    fn edit(&self, action: impl FnOnce(&mut World)) {
        SimulatedWorld::new(self.world.clone())
            .edit(|world| {
                action(world);
                Ok(())
            })
            .unwrap();
    }

    fn snapshot(&self) -> World {
        SimulatedWorld::new(self.world.clone()).load().unwrap()
    }

    /// A helper with alice's owner storage verified, ready for VM jobs.
    async fn provisioned(&self, retry: RetryPolicy) -> Host {
        let host = self.open(retry).await;
        host.submit(job("u-alice", "ensure_user", None)).unwrap();
        host.run_pending().await.unwrap();
        assert_eq!(
            host.journal().unwrap().jobs["u-alice"].status,
            OperationStatus::Verified
        );
        host
    }
}

fn policy(initial_ms: u64, max_ms: u64, budget_ms: u64) -> RetryPolicy {
    RetryPolicy {
        initial_delay: Duration::from_millis(initial_ms),
        max_delay: Duration::from_millis(max_ms),
        budget: Duration::from_millis(budget_ms),
    }
}

fn alice() -> User {
    User {
        user_id: "alice".into(),
        role: Role::Member,
        budget_mib: 128,
        min_memory_budget_mib: 256,
        max_memory_budget_mib: 1024,
        max_cores: 4,
        max_vms: 4,
        ready: false,
        policy_revision: 0,
        state: hearth::control::protocol::AccountState::Active,
    }
}

fn machine() -> Vm {
    Vm {
        vm_id: 9001,
        owner: "alice".into(),
        name: "one".into(),
        disk_mib: 32,
        memory_mib: 256,
        min_memory_mib: 128,
        cores: 1,
        state: VmState::Creating,
        guest: None,
    }
}

fn job(operation_id: &str, kind: &str, vm: Option<Vm>) -> Job {
    Job {
        operation_id: operation_id.into(),
        kind: kind.parse().unwrap(),
        owner: alice(),
        vm,
        disk_shrink: None,
        extension: None,
        policy_change: None,
    }
}

fn create() -> Job {
    job("c-9001", "create", Some(machine()))
}

fn step_phase(host: &Host, id: &str, step: Step) -> Phase {
    host.journal().unwrap().jobs[id]
        .steps
        .iter()
        .find(|record| record.step == step)
        .unwrap()
        .phase
}

#[tokio::test]
async fn transient_read_failures_back_off_then_the_job_completes() {
    let lab = Lab::new();
    let host = lab.provisioned(policy(200, 400, 60_000)).await;
    lab.edit(|world| world.api_read_failures = 2);
    host.submit(create()).unwrap();

    host.run_pending().await.unwrap();
    let job = host.journal().unwrap().jobs["c-9001"].clone();
    assert_eq!(job.status, OperationStatus::Running, "{job:?}");
    let retry = job.retry.clone().expect("retry scheduled");
    assert_eq!((retry.attempts, retry.step.as_str()), (1, "preflight"));
    assert!(job.error.as_deref().unwrap().contains("transient failure"));
    assert_eq!(job.failure, None);
    assert!(!lab.snapshot().vms.contains_key(&9001));

    // Not due yet: the runner leaves the job alone instead of spinning.
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["c-9001"].retry,
        Some(retry.clone())
    );

    tokio::time::sleep(Duration::from_millis(250)).await;
    host.run_pending().await.unwrap();
    let second = host.journal().unwrap().jobs["c-9001"]
        .retry
        .clone()
        .unwrap();
    assert_eq!(second.attempts, 2);
    assert_eq!(second.first_failure_ms, retry.first_failure_ms);
    assert!(second.next_attempt_ms - retry.next_attempt_ms >= 400);

    tokio::time::sleep(Duration::from_millis(450)).await;
    host.run_pending().await.unwrap();
    let job = host.journal().unwrap().jobs["c-9001"].clone();
    assert_eq!(job.status, OperationStatus::Verified, "{job:?}");
    assert_eq!((job.retry, job.error, job.failure), (None, None, None));
    assert!(lab.snapshot().vms.contains_key(&9001));
    assert_eq!(lab.snapshot().api_read_failures, 0);
}

#[tokio::test]
async fn an_exhausted_retry_budget_blocks_without_any_effect() {
    let lab = Lab::new();
    let host = lab.provisioned(policy(10, 10, 150)).await;
    lab.edit(|world| world.api_read_failures = 10_000);
    host.submit(create()).unwrap();
    let job = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            host.run_pending().await.unwrap();
            let job = host.journal().unwrap().jobs["c-9001"].clone();
            if job.status != OperationStatus::Running {
                break job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("budget exhausts");
    assert_eq!(job.status, OperationStatus::Blocked, "{job:?}");
    assert_eq!(job.failure, Some(ErrorClass::Retryable));
    assert_eq!(job.retry, None);
    let error = job.error.unwrap();
    assert!(error.contains("retry budget exhausted"), "{error}");
    assert!(
        error.contains("simulated transient API read failure"),
        "{error}"
    );
    assert!(job.steps.iter().all(|step| step.phase == Phase::Planned));
    assert!(!lab.snapshot().vms.contains_key(&9001));
}

#[tokio::test]
async fn the_retry_budget_survives_a_helper_restart() {
    let lab = Lab::new();
    let retry = policy(50, 50, 400);
    let host = lab.provisioned(retry).await;
    lab.edit(|world| world.api_read_failures = 10_000);
    host.submit(create()).unwrap();
    host.run_pending().await.unwrap();
    let first = host.journal().unwrap().jobs["c-9001"]
        .retry
        .clone()
        .unwrap();
    drop(host);

    // The helper is down for longer than the whole budget.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let host = lab.open(retry).await;
    assert_eq!(
        host.journal().unwrap().jobs["c-9001"].retry,
        Some(first),
        "retry state is journaled"
    );
    host.run_pending().await.unwrap();
    let job = host.journal().unwrap().jobs["c-9001"].clone();
    assert_eq!(job.status, OperationStatus::Blocked, "{job:?}");
    assert!(
        job.error
            .unwrap()
            .contains("over 2 attempts; retry budget exhausted")
    );
}

#[tokio::test]
async fn a_retrying_job_holds_back_only_later_jobs_of_the_same_owner() {
    let lab = Lab::new();
    let host = lab.provisioned(policy(60_000, 60_000, 600_000)).await;
    lab.edit(|world| world.api_read_failures = 1);
    host.submit(create()).unwrap();
    host.run_pending().await.unwrap();
    assert!(host.journal().unwrap().jobs["c-9001"].retry.is_some());
    // A later job of the same owner waits behind the backing-off job.
    host.submit(job("start", "start", Some(machine()))).unwrap();
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["start"].status,
        OperationStatus::Queued
    );
}

#[tokio::test]
async fn an_effect_error_with_the_exact_postcondition_verifies() {
    let lab = Lab::new();
    let host = lab.provisioned(RetryPolicy::default()).await;
    lab.edit(|world| {
        world.effect_faults = vec![EffectFault {
            step: Step::Vm,
            applied: true,
        }]
    });
    host.submit(create()).unwrap();
    host.run_pending().await.unwrap();
    let created = host.journal().unwrap().jobs["c-9001"].clone();
    assert_eq!(created.status, OperationStatus::Verified, "{created:?}");
    let vm = created.steps.iter().find(|r| r.step == Step::Vm).unwrap();
    assert_eq!((vm.phase, vm.upid.clone()), (Phase::Verified, None));
    assert!(lab.snapshot().effect_faults.is_empty());
    assert_eq!(lab.snapshot().vms.len(), 1);

    // A runtime effect too: start reports an error after the VM started.
    lab.edit(|world| {
        world.effect_faults = vec![EffectFault {
            step: Step::Start,
            applied: true,
        }]
    });
    let mut created = machine();
    created.state = VmState::Stopped;
    host.submit(job("start", "start", Some(created))).unwrap();
    host.run_pending().await.unwrap();
    let start = host.journal().unwrap().jobs["start"].clone();
    assert_eq!(start.status, OperationStatus::Verified, "{start:?}");
    assert!(lab.snapshot().vms[&9001].running);
}

#[tokio::test]
async fn an_effect_error_without_the_postcondition_blocks_and_never_repeats() {
    let lab = Lab::new();
    let host = lab.provisioned(RetryPolicy::default()).await;
    lab.edit(|world| {
        world.effect_faults = vec![EffectFault {
            step: Step::Vm,
            applied: false,
        }]
    });
    host.submit(create()).unwrap();
    host.run_pending().await.unwrap();
    let job = host.journal().unwrap().jobs["c-9001"].clone();
    assert_eq!(job.status, OperationStatus::Blocked, "{job:?}");
    assert_eq!(job.failure, Some(ErrorClass::Ambiguous));
    assert!(
        job.error
            .as_deref()
            .unwrap()
            .contains("postcondition is absent"),
        "{job:?}"
    );
    assert_eq!(step_phase(&host, "c-9001", Step::Vm), Phase::Intent);
    assert!(!lab.snapshot().vms.contains_key(&9001));

    // Same boot: reopening does not repeat the effect either.
    drop(host);
    let host = lab.open(RetryPolicy::default()).await;
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["c-9001"].status,
        OperationStatus::Blocked
    );
    assert!(!lab.snapshot().vms.contains_key(&9001));
}

#[tokio::test]
async fn a_stopping_helper_finishes_its_in_flight_step_and_starts_no_other() {
    let lab = Lab::new();
    let mut config = lab.config(RetryPolicy::default());
    config.task_timeout = Duration::from_secs(30);
    let host = Host::open(config.clone()).await.unwrap();
    host.submit(job("u-alice", "ensure_user", None)).unwrap();
    host.run_pending().await.unwrap();
    let host = Arc::new(host);
    lab.edit(|world| world.stall_tasks = true);
    host.submit(create()).unwrap();
    let runner = host.clone();
    let running = tokio::spawn(async move { runner.run_pending().await });
    let upid = wait_for_receipt(&host, Step::Vm).await;

    host.request_shutdown();
    lab.edit(|world| {
        world.stall_tasks = false;
        world
            .tasks
            .insert(upid, json!({"status":"stopped","exitstatus":"OK"}));
    });
    running.await.unwrap().unwrap();
    let job = host.journal().unwrap().jobs["c-9001"].clone();
    assert_eq!(job.status, OperationStatus::Running, "{job:?}");
    assert_eq!(step_phase(&host, "c-9001", Step::Vm), Phase::Verified);
    assert_eq!(step_phase(&host, "c-9001", Step::Volume), Phase::Planned);
    assert_eq!(job.failure, None);

    // A stopping helper starts nothing more.
    host.run_pending().await.unwrap();
    assert_eq!(step_phase(&host, "c-9001", Step::Volume), Phase::Planned);

    drop(host);
    let host = Host::open(config).await.unwrap();
    host.run_pending().await.unwrap();
    assert_eq!(
        host.journal().unwrap().jobs["c-9001"].status,
        OperationStatus::Verified
    );
}

async fn wait_for_receipt(host: &Host, step: Step) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(upid) = host
                .journal()
                .unwrap()
                .jobs
                .get("c-9001")
                .and_then(|job| job.steps.iter().find(|r| r.step == step))
                .and_then(|record| record.upid.clone())
            {
                break upid;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the step records its task receipt")
}

#[tokio::test]
async fn serve_drains_on_the_first_signal_and_exits_on_the_second() {
    let lab = Lab::new();
    let socket = lab.root.path().join("run").join("helper.sock");

    // Idle: one signal stops promptly.
    let host = Arc::new(lab.open(RetryPolicy::default()).await);
    let (signal, signals) = tokio::sync::mpsc::channel(2);
    let served = tokio::spawn({
        let host = host.clone();
        let socket = socket.clone();
        async move { serve_until(host, &socket, signals, Duration::from_secs(60)).await }
    });
    let health = wait_for_socket(&socket).await;
    assert_eq!(health.status, OperationStatus::Verified);
    signal.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("an idle helper stops on the first signal")
        .unwrap()
        .unwrap();
    drop(host);

    // In flight: the first signal waits for the step; the second exits now.
    let mut config = lab.config(RetryPolicy::default());
    config.task_timeout = Duration::from_secs(30);
    let host = Arc::new(Host::open(config).await.unwrap());
    lab.edit(|world| world.stall_tasks = true);
    host.submit(job("u-alice", "ensure_user", None)).unwrap();
    host.submit(create()).unwrap();
    let (signal, signals) = tokio::sync::mpsc::channel(2);
    let served = tokio::spawn({
        let host = host.clone();
        let socket = socket.clone();
        async move { serve_until(host, &socket, signals, Duration::from_secs(60)).await }
    });
    wait_for_receipt(&host, Step::Vm).await;
    signal.send(()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !served.is_finished(),
        "draining waits for the in-flight step"
    );
    signal.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("a second signal exits immediately")
        .unwrap()
        .unwrap();
    // The interrupted step keeps its durable intent and receipt for recovery.
    assert_eq!(step_phase(&host, "c-9001", Step::Vm), Phase::Intent);
}

async fn wait_for_socket(socket: &std::path::Path) -> hearth::control::host::HostResponse {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(response) = rpc(socket, &HostRequest::Health).await {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("helper socket answers")
}

struct Controller {
    lab: Lab,
    store: Store,
    admin: String,
}

impl Controller {
    fn new() -> Self {
        let lab = Lab::new();
        let admin = SecretKey::generate().public().to_string();
        let store = Store::create(
            &lab.root.path().join("state.redb"),
            Binding {
                environment: ENV.into(),
                backend: "simulated".into(),
                disk_capacity_mib: 16384,
                min_memory_capacity_mib: 2048,
                guest_images: vec![],
                physical_trial: None,
                managed: None,
            },
            &admin,
        )
        .unwrap();
        Self { lab, store, admin }
    }

    fn send(&self, value: Value) -> Value {
        self.store
            .handle(&self.admin, serde_json::from_value(value).unwrap())
            .unwrap()
    }

    async fn flush(&self, host: &Host) {
        for record in self.store.pending().unwrap() {
            let id = record.public.operation_id.clone();
            host.submit(record.job).unwrap();
            host.run_pending().await.unwrap();
            let journal = host.journal().unwrap();
            let r = &journal.jobs[&id];
            self.store
                .progress(&id, r.status.clone(), &r.step, r.error.clone())
                .unwrap();
        }
        let left = self.store.pending().unwrap();
        assert!(
            left.is_empty(),
            "{:?}",
            left.iter()
                .map(|r| (&r.public.operation_id, &r.public.status, &r.public.error))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn a_helper_rejection_fails_the_operation_and_releases_its_claims() {
    let controller = Controller::new();
    let socket = controller.lab.root.path().join("run").join("helper.sock");
    let host = Arc::new(controller.lab.open(RetryPolicy::default()).await);
    controller.lab.edit(|world| world.pool_size_mib = 65536);
    controller.flush(&host).await;
    controller.send(
        json!({"action":"create_user","request_id":"owner","user_id":"alice",
        "budget_mib":512,"min_memory_budget_mib":512,"max_memory_budget_mib":1024,
        "max_cores":2,"max_vms":1}),
    );
    controller.flush(&host).await;
    let accepted = controller.send(json!({"action":"create_vm","request_id":"vm","name":"one",
        "owner":"alice","disk_mib":256,"memory_mib":256,"min_memory_mib":128,"cores":1}));
    let id = accepted["operation_id"].as_str().unwrap().to_string();
    let vm_id = accepted["vm_id"].as_u64().unwrap() as u16;
    // The owner's only VM slot is now claimed.
    assert!(
        controller
            .store
            .handle(
                &controller.admin,
                serde_json::from_value(json!({"action":"create_vm","request_id":"other",
                    "name":"two","owner":"alice","disk_mib":16,"memory_mib":128,
                    "min_memory_mib":128,"cores":1}))
                .unwrap()
            )
            .is_err()
    );

    // Deliver a payload this helper's validation refuses: on a simulated
    // binding, a failed-start acknowledgment requires a managed helper.
    let mut job = controller.store.pending().unwrap().remove(0).job;
    assert_eq!(job.operation_id, id);
    job.extension = Some(Box::new(JobExtension::Recover(RecoverySpec {
        operation_id: id.clone(),
        inspection_id: "inspection".into(),
        kind: RecoveryKind::AcknowledgeFailedStart,
    })));

    // A transport failure leaves the operation queued for resubmission.
    let missing = controller.lab.root.path().join("absent.sock");
    let transport = rpc(
        &missing,
        &HostRequest::Submit {
            environment: ENV.into(),
            job: job.clone(),
        },
    )
    .await
    .unwrap_err();
    assert!(
        !controller
            .store
            .settle_submit_error(&id, &transport)
            .unwrap()
    );
    assert_eq!(controller.store.pending().unwrap().len(), 1);

    // The real wire: the helper answers with a typed rejection code.
    let (signal, signals) = tokio::sync::mpsc::channel(1);
    let served = tokio::spawn({
        let host = host.clone();
        let socket = socket.clone();
        async move { serve_until(host, &socket, signals, Duration::from_secs(5)).await }
    });
    wait_for_socket(&socket).await;
    let rejection = rpc(
        &socket,
        &HostRequest::Submit {
            environment: ENV.into(),
            job,
        },
    )
    .await
    .unwrap_err();
    assert!(HelperError::has_code(&rejection, HelperErrorCode::Rejected));
    assert!(!host.journal().unwrap().jobs.contains_key(&id));
    assert!(
        controller
            .store
            .settle_submit_error(&id, &rejection)
            .unwrap()
    );
    signal.send(()).await.unwrap();
    served.await.unwrap().unwrap();

    let catalog = controller.store.read().unwrap();
    let operation = &catalog.operations[&id].public;
    assert_eq!(operation.status, OperationStatus::Failed);
    assert!(operation.status.is_terminal());
    assert!(
        operation
            .error
            .as_deref()
            .unwrap()
            .contains("host helper error")
    );
    assert_eq!(catalog.vms[&vm_id].state, VmState::Deleted);
    assert!(controller.store.pending().unwrap().is_empty());
    // Later helper progress cannot reopen it, and a retry returns the outcome.
    controller
        .store
        .progress(&id, OperationStatus::Running, "vm", None)
        .unwrap();
    assert_eq!(
        controller.store.read().unwrap().operations[&id]
            .public
            .status,
        OperationStatus::Failed
    );
    // The claims are released: the owner's single VM slot is free again.
    let replacement = controller.send(json!({"action":"create_vm","request_id":"other",
        "name":"one","owner":"alice","disk_mib":256,"memory_mib":256,
        "min_memory_mib":128,"cores":1}));
    assert_ne!(replacement["vm_id"].as_u64().unwrap() as u16, vm_id);
    // The signalled helper stays stopped; a restarted one runs the new work.
    drop(host);
    let host = controller.lab.open(RetryPolicy::default()).await;
    controller.flush(&host).await;
}

#[test]
fn only_this_controllers_typed_helper_ambiguity_blocks_a_transfer() {
    // A peer's refusal arrives as text; whatever it says, it never blocks.
    let peer = anyhow::anyhow!(
        "migration peer refused request: host helper error: ambiguous target seal intent"
    );
    assert!(!blocks_transfer(&peer, TransferPhase::Preparing, false));
    assert!(!blocks_transfer(
        &anyhow::anyhow!("ambiguous"),
        TransferPhase::Fenced,
        false
    ));
    // The local helper's typed ambiguity blocks a precommit transfer.
    let local = HelperError::ambiguous("ambiguous fence intent cannot be replayed automatically")
        .context("source fence");
    assert!(blocks_transfer(&local, TransferPhase::Preparing, false));
    assert!(blocks_transfer(&local, TransferPhase::Ready, false));
    // ...but never one whose ownership handoff is committed.
    assert!(!blocks_transfer(&local, TransferPhase::Ready, true));
    for committed in [
        TransferPhase::Relinquished,
        TransferPhase::Accepted,
        TransferPhase::Cleaning,
    ] {
        assert!(!blocks_transfer(&local, committed, false));
    }
    // A rejection is not an ambiguity.
    assert!(!blocks_transfer(
        &HelperError::rejected("invalid"),
        TransferPhase::Preparing,
        false
    ));
}

#[tokio::test]
async fn a_stopping_helper_refuses_new_migration_effects_but_still_answers_reads() {
    let lab = Lab::new();
    let host = lab.open(RetryPolicy::default()).await;
    let migration = |request| HostRequest::Migration {
        environment: ENV.into(),
        request: Box::new(request),
    };
    use hearth::control::migration::HostMigrationRequest;
    let fence = || {
        migration(HostMigrationRequest::Fence {
            migration_id: "m-unknown".into(),
        })
    };
    let inspect = || {
        migration(HostMigrationRequest::Inspect {
            migration_id: "m-unknown".into(),
        })
    };
    // Before stopping, both reach the handler and fail on the unknown record.
    for request in [fence(), inspect()] {
        let error = host.handle(request).await.unwrap_err();
        assert!(
            !format!("{error:#}").contains("helper is stopping"),
            "{error:#}"
        );
    }
    host.request_shutdown();
    let error = host.handle(fence()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("helper is stopping"),
        "{error:#}"
    );
    let error = host.handle(inspect()).await.unwrap_err();
    assert!(
        !format!("{error:#}").contains("helper is stopping"),
        "{error:#}"
    );
}

#[test]
fn only_a_destination_that_blocked_its_own_record_blocks_the_source() {
    let refused = peer_reply(json!({"ok":false,"error":"ambiguous preparation"})).unwrap_err();
    assert!(!blocks_transfer(&refused, TransferPhase::Fenced, false));
    let blocked = peer_reply(json!({
        "ok":false,
        "error":"preparation outcome is ambiguous",
        "code":"destination_blocked",
    }))
    .unwrap_err();
    assert!(format!("{blocked:#}").contains("preparation outcome is ambiguous"));
    assert!(blocks_transfer(&blocked, TransferPhase::Fenced, false));
    assert!(!blocks_transfer(&blocked, TransferPhase::Ready, true));
    assert_eq!(
        peer_reply(json!({"ok":true,"result":{"phase":"receiving"}})).unwrap(),
        json!({"phase":"receiving"})
    );
}
