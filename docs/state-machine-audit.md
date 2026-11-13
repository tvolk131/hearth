# Independent server state-machine audit — November 10–11, 2026

Result: **the audit found real recovery defects and an executor-serialization gap.** Targeted counterexamples failed before the fixes. The corresponding regression tests pass after the fixes. This is bounded evidence, not a proof that the system is airtight or ready for production writes.

No real host, SSH account, production VM, guest disk or production database was modified by this audit. Test worlds, identities and journals were disposable local fixtures.

## Findings and disposition

### High: recovered runtime actions accepted stale identity verification

`Host::run_step` treated an already-verified `Verify` step as sufficient authority when resuming later `Start`/`Stop` work. The runtime observer checked whether the VMID was running or stopped, without rechecking its recorded VM UUID, disk GUIDs, attachments and owner storage.

Counterexample: create VM 9001, record the start intent, crash before the start effect, reboot, replace the VM's SMBIOS identity at the same VMID, then resume. The old code started the replacement and marked the operation `Verified`. A replacement already satisfying the desired runtime state could also be accepted as success.

**Fixed by root:** `verify_effect_target` revalidates the full recorded machine and owner identities when reconciling an unverified runtime step, immediately before its external effect, and after a recorded task completes. Recorded task receipts are awaited before interpreting their postconditions. The audit covers start and stop at before-effect, after-effect and after-receipt boundaries; changed same-name ZFS GUIDs; and a running replacement falsely satisfying the desired start state.

### High: recovered deletion could destroy newly attached foreign data

Deletion had the same stale-verification issue after its initial full `Verify` step. Its subsequent unprotect/destroy observations checked only part of the VM identity.

Counterexample: crash at unprotect, attach an unrelated zvol to the original VM's `scsi0`, preserving its UUID/profile, then resume after reboot. The old code removed the newly attached foreign volume. The regression suite also covers a same-boot crash after unprotect; recovery protection must not depend on a reboot.

**Fixed by root:** deletion uses full machine/owner identity checks with protection-aware observation during unprotect/destroy, checks that the VM is still stopped, requires protection to remain disabled immediately before destruction, and freshly checks actual VM/managed-volume absence before releasing capacity. A failed recorded destroy task remains blocked even if physical absence happens to satisfy part of the desired state. Already completed steps are not replayed to resurrect deleted resources.

### Medium: multiple executors could enter one Running helper job

The executable normally starts one worker, and redb excludes a second helper process. However, the public `Host::run_pending(&self)` API did not itself serialize execution across asynchronous effects. Two invocations on one `Host` could both enter the same Running job while one awaited a Proxmox task.

Counterexample: keep a start task running after its UPID is persisted, then call a second executor. The journal's attempt count became **2** while the first executor was still active; the required count is **1**. The test controls the task's completion explicitly rather than relying on concurrent process timing.

**Fixed by root:** a per-Host Tokio mutex is held for the entire `run_pending` execution, including awaited external effects. The second executor waits, then sees no remaining work after the first completes.

## Invariants checked

| Invariant | Implementation boundary and evidence |
|---|---|
| Device identity determines the actor; revocation applies to the request transaction | Transport authenticates the iroh endpoint, and `Store::handle` authenticates again inside its serialized catalog transaction; existing authorization/revocation tests |
| A request ID names one immutable operation per actor | Controller request index and frozen request fingerprint; helper operation ID and frozen-job fingerprint; existing retry tests plus lifecycle model |
| Acknowledged acceptance already owns its claims | VM, operation, request index and audit intent commit together with immediate durability before response; existing concurrent admission tests and lifecycle model |
| Unfinished or uncertain work retains admission claims | Creating/deleting/blocked VMs stay charged; conflicting actions are rejected until the earlier operation is verified; existing blocked/deletion tests and lifecycle model |
| Intent precedes an external effect; a stored UPID is awaited | Helper intent/receipt/step records; existing crash tests and new failed-task regression |
| An old verification cannot authorize a replacement resource | Full fresh UUID/GUID/profile/attachment/owner checks at unverified runtime/delete action boundaries; new counterexamples and fixes |
| No physical deletion proof means no capacity release | Exact absence observation, helper tombstone, then verified controller progress; existing deletion tests and lifecycle model |
| Deleted VMIDs are never reused | Controller catalog and helper machine tombstones remain; existing tests and generated lifecycle sequence |
| Helper execution has one active effect owner | Per-Host execution mutex and overlapping-executor regression; redb excludes a second process |
| Missing journals do not silently reinitialize existing infrastructure | Initialization sentinels and helper control-dataset refusal; existing lost-journal tests |
| A restored snapshot cannot automatically start workers | Both restored state directories are marked for review before initialization sentinels; both startup paths refuse any existing marker, including a dangling symlink; maintenance tests and source review |

## Failure and recovery matrix

| Boundary | Required behavior |
|---|---|
| Before controller acceptance commits | No accepted operation or new claim; caller may submit normally |
| After acceptance, before helper receipt | Reopen and deliver the same immutable operation ID |
| After helper acceptance, before controller acknowledgement | Duplicate submission returns the existing helper job; no second allocation |
| Same-boot intent with no receipt and absent effect | Block for investigation; do not speculate that it is safe to repeat |
| Lost receipt with an exact proven present effect | Reconcile only after current resource identity is validated |
| Recorded UPID, including an unfinished task | Await that task; unknown, failed or timed-out outcomes block rather than release claims |
| New host boot after an incomplete intent | Permit only supported deterministic reconciliation against current identities; a reboot does not authorize replacement resources |
| Verified persistent resource vanished or drifted | Block rather than recreate or overwrite it |
| Destroy completed, before capacity release | Require VM and both managed volume names to remain absent |
| Helper verified, controller acknowledgement lost | Return the same operation proof; the catalog remains a last-verified view, not continuous live inventory |
| Two in-process executors overlap | Serialize the entire effect-bearing execution |
| Offline backup restoration | Restore to a fresh quarantined staging tree; no automatic activation or replay |

## Added validation

`tests/state_machine_audit.rs` contains ten tests:

- Seven targeted crash, drift and task-receipt regressions, including multiple boundaries inside each test.
- One bounded controller/helper lifecycle model: three deterministic seeds, 96 requests per seed, repeated durable reopen, deliberately omitted helper acknowledgements, retries, create/start/stop/delete, physical inventory checks, admission limits and tombstones.
- One legacy lock stress test: 64 independent lock/second-writer-refusal/drop/immediate-reopen cycles.
- One forced overlapping-executor test using an explicitly stalled external task.

Commands:

```sh
cargo test --locked --test state_machine_audit -- --nocapture
cargo clippy --locked --test state_machine_audit -- -D warnings
```

Before the fixes, the original two recovery regressions failed (`Verified` instead of `Blocked` for the replacement VM; unrelated attached zvol removed), and the overlap regression separately failed (`attempts == 2`). These failures were retained as findings, not counted as passing validation. See `evidence/state-machine-audit-20261111/` for the compact result record.

The previously reported intermittent legacy simulator lock failure did not reproduce in the 64-cycle stress test. Its cause remains unexplained; a passing bounded rerun does not establish a fix.

Subsequent quota integration reproduced the matching inherited-descriptor retention failure deterministically and added explicit unlock after SQLite close. The corrected aggregate suite now passes all 137 Rust tests, including this audit and the new lock regressions. The original intermittent fork timeline was not traced. See `legacy-lock-recovery.md` for the mechanism, proof-test cleanup correction and hard-termination limits.

## Operational maintenance read-through

The new maintenance module was also read for interaction with these state machines: it holds both databases open for offline logical export, checks the environment/server identity/pool and matching immutable helper jobs, pins bundle integrity to an independently retained digest, writes to private fresh targets, and writes quarantine markers before restored initialization sentinels. No additional concrete blocker was found in that read-through. Its separate maintenance tests and actual-process recovery drill are maintained by the operational-hardening track.

Semantic cross-journal checks are not a shared backup-epoch attestation. A newer helper completion with an older unacknowledged controller operation can be indistinguishable from normal acknowledgement loss; device revocation also has no independent helper copy. Quarantine and independent physical reconciliation remain necessary before any future restoration activation. A database snapshot cannot prove that outside Proxmox tasks stopped or that real infrastructure did not change afterward.

## Remaining boundaries

- Identity checks and a Proxmox mutation are separate operations. The audit does not establish atomicity against a simultaneous out-of-band root/Proxmox administrator changing configuration between the final check and the effect. Production ownership, locking and conditional-update policy need explicit treatment.
- Simulated crash hooks cover selected boundaries, not every machine instruction, filesystem flush, kernel crash, power loss or corrupt-sector scenario. The bounded model is not exhaustive model checking.
- The simulator completes physical effects before representing asynchronous task status. The new receipt test covers an authoritative failure, but does not model every intermediate behavior of a real long-running Proxmox task.
- Existing real-Proxmox evidence predates these fixes. This audit does not claim a new full real-Proxmox failure-injection run or actual Hetzner mutation validation.
- Iroh public relay/NAT traversal, packet-level abuse, rate limits, history growth/pagination and denial-of-service behavior are outside this state-transition audit. Existing transport tests exercise actual authenticated loopback iroh.
- The reviewed controller remains lab/simulated only. Existing production guests have not been adopted, and no production write gate should be removed on the strength of this audit.
- Removing a quarantine marker manually is not an audited restore procedure. No restore activation/reconciliation mechanism has been implemented.

No finite test suite or final review establishes airtightness. The useful outcome is narrower: explicit invariants, reproducible counterexamples, concrete fixes, and regression tests that prevent these failure modes from silently returning.
