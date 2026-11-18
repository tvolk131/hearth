# Changelog

Project history for Hearth, newest first. The docs in [`docs/`](docs) describe
current behaviour; this file keeps the dated validation runs and development
notes that used to be mixed into them.

Notes on what these entries refer to:

- Evidence paths such as `evidence/...` and `.lab/...` were local to the original
  development workspace. Both directories are Git-ignored, and none of that
  evidence is in this repository.
- Operator tooling under `deployment/` (for example `deployment/stage.py`,
  `deployment/physical_trial.py`, `deployment/six_feature_trial.py`,
  `deployment/persistent_management.py` and `deployment/managed_adoption.py`)
  was used for some of these runs but was never committed to this repository.
- Test counts are as reported at the time and are not current.

## Unreleased

- Administrators can retire the storage a deleted adopted VM leaves behind
  (`retire_storage`; `hearth vm retire-storage`; **Retire Storage** in the
  app's Servers page). It removes the VM's Proxmox storage entry and then
  destroys its emptied dataset non-recursively, freeing the reservation, after
  rechecking the pinned identities and that nothing else uses either. The
  helper journal records it on the machine record (`retirement`) and the
  controller catalog in `retired_allocations`; both are optional fields, so
  the storage format stays at 2.
- The app now keeps reading a server while it has a queued or running
  operation, until that work finishes, even with auto-refresh off. Before this
  it read the server once after a change was accepted, so an operation that
  finished seconds later could look stuck at its last step until a manual
  refresh. Blocked operations are not followed, and a failed read stops
  following until the next successful read.
- Managed recovery can now close a blocked start, shutdown, stop or delete
  that the helper refused before any step began (`acknowledge_refusal`;
  `hearth operations recover --kind acknowledge-refusal`). Before this, only
  failed starts could be inspected or recovered on a managed host, so a delete
  refused because of leftover snapshots stayed blocked with no way to clear it.
- Removed storage format-1 migration and the `hearth-host repair-managed-schema`
  tool, which only worked on format-1 journals. Both managed hosts were
  migrated with `e619ef0`. A format-1 database is now refused without being
  modified, and the error names `e619ef0` as the last build that migrates it.
  Backup bundles from either format still verify and restore.
- Renamed the binaries and app to Hearth: `pve-devd` is now `hearthd`,
  `pve-dev-host` is `hearth-host`, `pve-devctl` is `hearth`, `pve-dev-ops` is
  `hearth-ops` and `pve-dev-inventory` is `hearth-inventory`. The crate is
  `hearth`, and the Swift package builds `Hearth` and `HearthCore`. Entries
  below keep the old names. Deployed identifiers are unchanged so existing
  installs keep working: the iroh ALPN `pve-dev/control/1`, systemd unit names,
  state, run and config directories, service accounts, and the app's
  `UserDefaults` keys, bundle identifiers, ledger namespace and
  `Application Support` directories.
- Licensed under the MIT License (`LICENSE`, `license = "MIT"` in `Cargo.toml`).
- Moved dated validation history out of the docs and READMEs into this file.
- Docs no longer point at `deployment/` scripts as if they were in the
  repository, and the lab README now documents only the existing `lab.py`
  commands (`build`, `control-test`, `guest-test`).

## 2026-11-13

### Heidi SSD shrink

A separately authorized one-off maintenance shrank Heidi's SSD VM to 400 GiB and
updated its accounting; the SSD managed storage ceiling became 440 GiB. No
physical host was rebooted. It did not expand the lab shrink adapter's supported
layouts or enable managed resize RPCs. See
[persistent-management.md](docs/persistent-management.md) and
[disk-shrink.md](docs/disk-shrink.md).

## 2026-11-12

### Managed deployment and fleet UI review

The November 12 deployment of the `managed` backend on both real hosts introduced
a separate app, settings suite and request ledger. A later fleet adoption made
the four original VMs managed records. See [persistent-management.md](docs/persistent-management.md).

The complete SwiftUI app received an Opus 5.5 review, a second review of the
fixes, and a focused safety recheck. The final static review found no blocking
issues. Additional local regressions and narrow-window layout corrections
followed; the final native debug/release build passed 165 Swift tests. Reports
were retained under `.lab/swiftui-opus-review-20261112`. Native checks on both
real-host views were read-only. An explicitly approved local fixture then
verified queued cancellation by button and Escape (zero launched RPCs), disabled
Close and Escape dismissal while sending, and the narrow fleet table. That pass
corrected cancellation wording and native Escape handling; all 165 tests still
passed. Final evidence: `evidence/swiftui-opus-review-20261112/summary.json`.

### Disk-shrink qualification (November 11–12)

The administrator-only controller/helper/CLI/GUI shrink flow is implemented for
controller-created single-ext4 GPT lab guests. The fresh real-Proxmox fixture
passed two shrink cycles (4096→2560→2048 MiB), paired service restarts, guest
cloud-init/systemd/SSH health, unchanged guest identities and random sentinel
data, graceful shutdown, explicit recovery deletion, real unused-quota
contraction and orphan-free normal deletion. A completed-copy/lost-ack helper
kill blocked without replay and preserved the original. All owned local groups
retired without forced cleanup. See [disk-shrink.md](docs/disk-shrink.md) and the
report.

Validation passed 174 Rust tests, 115 Python lab tests, 97 Swift tests including
real CLI integration, 87 deployment tests and ten local process checks, plus
formatting, Clippy and both Swift build configurations. No new native GUI visual
check was performed. This supersedes the earlier checkpoint's unsupported
guest-resize status only for this lab adapter. Production execution/adoption,
Heidi's layout/workspace needs and generic blocked reconciliation remain
unsupported; no production host or VM was changed.

## 2026-11-11

### Bounded physical-host trial

The emulated scoped `physical_trial` run passed on November 11. A later
supervised two-host hardware trial completed, including cleanup, using
`deployment/physical_trial.py`, which is not in this repository. See [physical-trial.md](docs/physical-trial.md),
[physical-trial-backend.md](docs/physical-trial-backend.md) and
[quota-real-qualification.md](docs/quota-real-qualification.md).

### Recovery audit and read-only host support (November 10–11)

The independent state-machine audit reproduced two high-severity recovery identity gaps and an overlapping-executor gap, all in disposable simulation. Resumed start/stop could reuse an earlier verification after VM replacement; resumed delete could destroy a newly attached foreign volume. The helper now revalidates complete recorded identities at action reconciliation/effect boundaries, checks stopped/unprotected state before destruction, and rechecks absence before releasing capacity. An async execution mutex prevents overlapping `run_pending` calls. See [`state-machine-audit.md`](docs/state-machine-audit.md) for baseline failures, ten passing regressions/model tests and the remaining check/effect race with external administrators. This is not a formal proof or a new real-Proxmox mutation qualification.

The read-only GUI path ([`read-only-gui.md`](docs/read-only-gui.md)) successfully collected fresh, pinned observations from both real hosts. It creates no controller resources or management authority. The controller/helper remain undeployed on those hosts. Offline paired recovery and direct-mode iroh restart/revocation are covered by focused tests and a local-process crash/export/quarantined-restore drill; cross-machine public relay/NAT behavior remains unverified. The historical legacy lock flake did not recur during bounded stress, but its cause remains unresolved.

Integrated validation passed 116 Rust tests, 78 Swift tests including the real Rust CLI integration, 36 Python lab tests, four Python staging tests, both Swift build configurations, formatting, all-target Clippy with warnings denied, Python/shell syntax checks and workflow lint. The local controller process harness passed all five checks. After explicit launch approval, the isolated review app's native visual check also passed against both real hosts, with correct per-host VM placement, recovery labels and read-only controls. Evidence is in `evidence/parallel-hardening-20261111/summary.json` and `evidence/read-only-gui-20261111/native-visual-qa.md`.

### Quota and migration follow-up (November 10–11)

The lab controller, CLI and GUI now support administrator policy edits with expected-revision checks, immutable old/target policies, conservative capacity holds and an owner freeze until helper verification. Reductions cannot discard configured allocations or actual ZFS use. The helper journals quota/reservation effects separately, rechecks physical identities during recovery, and retains claims on ambiguous outcomes. Memory/CPU-only edits preserve storage properties. Zero/unlimited/removal and guest disk resizing remain unsupported. See [`quota-editing.md`](docs/quota-editing.md) and [`quota-state-review.md`](docs/quota-state-review.md); production host mode remains read-only and the complete new Rust quota operation still needs real-Proxmox qualification.

Final integrated validation passed **137 Rust tests**, **84 Swift tests** with actual Rust RPC, **54 Python lab tests**, four offline staging tests, nine local-process checks, both Swift build configurations, formatting, all-target Clippy with warnings denied, syntax checks and workflow lint. This includes twelve quota tests with 24 crash combinations and five adversarial quota tests (four independent review cases plus a parent follow-up). Evidence is in `evidence/quota-integration-20261111/summary.json`. No new native quota-editor visual check was performed.

Integration also reproduced the legacy simulator's inherited-descriptor lock-retention failure class and fixed normal-drop release without weakening live-writer exclusion. All aggregate checks pass after correcting a parallel-inheritance assumption in the new proof test. The legacy simulator has since been removed from the repository.

The separate two-independent-node Proxmox spike passed a stopped toy-VM copy with byte-exact root/cloud-init disks, preserved guest identities, thick backing at both ends, target collision/repeat refusal and retained-source start refusal. Both API tokens were revoked and both owned QEMU hosts shut down. This is a conditional go for the native transfer primitive, not a migration API or production qualification. Interruption/lost-ack recovery, reboot fencing, bootable guest identity, custom snippets and the loopback API over iroh remain gates. See [`migration-spike.md`](docs/migration-spike.md) for real evidence and [`migration-handoff.md`](docs/migration-handoff.md) for the proposed protocol and bounded executable model.

## 2026-11-10

### Offline recovery and service staging

All nine `cargo test --locked --test maintenance` checks passed at the time,
including real Direct-mode iroh. `python3 scripts/test-maintenance-local.py` also
passed its five-check actual-process drill: it killed the daemon after durable
acceptance, exported both stopped journals, verified the pinned bundle, preserved
the same accepted operation/VM identity, and observed both restored binaries
refuse to start. The simulated fixtures and processes were removed afterward.

Four `python3 -B deployment/test_stage.py` checks passed for exact manifest
coverage, non-Linux binary refusal, symlink refusal and unsafe-parent refusal.
That staging script and its tests were never committed to this repository. The
tests used synthetic ELF headers; they did not execute or qualify Linux binaries.
Focused Clippy with warnings denied, Rust formatting, all-bin debug build and
Python compilation passed. Socket tests ran outside the restricted sandbox; the
initial sandbox attempt could not bind UDP (`EPERM`). The process drill initially
used an incorrect CLI identity subcommand; it was corrected to the existing
`identity new --path` interface before the passing run.

The non-secret receipt summary was `evidence/service-recovery-20261110/summary.json`.

### Bootable guest: first clean run

The fresh run `real-controller-20261110T000212-44dcfa` passed **all eleven combined checks** against Proxmox VE **9.2.26**, with `complete: true` and `nested_guest_boot_verified: true`. Its measured suite time was **3,576.5 seconds** (about 60 minutes) on an ARM Mac under multithreaded x86 TCG, excluding final diagnostics/shutdown. Evidence is in `evidence/real-controller-20261110T000212-44dcfa/`. The driver exited successfully, no cleanup error was recorded, and a final local process check found no lab QEMU, demo daemon/helper or guest SSH probes remaining.

- `guest-image-build.json` identifies recipe v2, the unchanged checksum-pinned parent, both drop-ins and the independently pinned derivative. `guest-image-comparison.txt` confirms the pre-boot source/zvol comparison; its size-mismatch warning is expected because the 3 GiB source is zero-extended into a 4 GiB target.
- `guest-readiness.json` records `status: done`, `extended_status: done`, empty errors and recoverable errors in every cloud-init stage, successful owner SSH, foreign-key refusal and healthy systemd. `guest-health.txt` independently records the `dev` account, mounted ext4 root, expanded filesystem, passwordless owner sudo and systemd `running`.
- The passive serial log was captured without console input. No extra manual SSH probes, guest configuration changes or emergency-mode continuation were used in this passing run.
- Verified stop/delete removed both guest zvols. The remaining baseline checks also passed: shared owner budgets including pending claims, cross-tenant refusals, capacity release, daemon SIGKILL recovery, real ZFS quota refusal and live device revocation. The final physical VM/zvol inventory exactly matched the catalog, with every retained VM stopped.

The failed/incomplete attempts from November 9 remain failures; none is substituted for this clean run. A passing lab-only derivative is not evidence that an unchanged arbitrary cloud image, a production network, or the Hetzner guests were tested or modified.

### macOS UI smoke check

A local UI smoke check on 2026-11-10 reproduced and fixed an oversized VM-page layout that moved both table content and sidebar navigation outside the window. The native table is now bounded by the detail area's actual size. Dashboard loading, all sidebar destinations, returning to VMs, sorting/filter controls, window zoom, and opening/cancelling the New VM sheet were checked on macOS 26.6.2 against the simulated backend. No mutations were submitted. Debug/release builds and the Swift suite passed; its opt-in Rust CLI integration test was skipped in this rerun. This is local smoke coverage, not a complete visual or interaction audit.

### Follow-up regression results (November 9–10)

The November 9–10 follow-up fast validation passed **84 Rust tests**, formatting, Clippy with warnings denied, and workflow/shell syntax checks; the updated lab suite passed **36 Python tests**. The local process harness also passed all five checks, including daemon SIGKILL recovery, helper persistence, live revocation and real Swift-to-Rust RPC. Swift debug/release builds and **57 Swift tests** passed. These simulator/process results are separate from the November 9 real Proxmox results and from the bootable-guest extension in [`bootable-guests.md`](docs/bootable-guests.md).

A later full validation attempt failed once in the unchanged legacy simulator test `exclusive_lock_blocks_a_second_writer`, at reopening state immediately after dropping the first writer. The isolated rerun and the subsequent complete `scripts/check.sh` both passed, without changing the test or lock implementation. The cause is not established; the passing rerun does not establish that this intermittent failure is resolved. Private logs are retained in `.lab/final-fast-checks.log` and `.lab/final-fast-checks-rerun.log`.

## 2026-11-09

### Rust controller suite on disposable Proxmox

The complete selected controller suite passed against Proxmox VE **9.2.26**, with `complete: true` and all eight integration checks in `evidence/real-controller-20261109T173920-5183e4/summary.json`. It reported 2,353.7 seconds (about 39 minutes) on an ARM Mac under single-threaded x86 software emulation, excluding final diagnostics/shutdown. The QEMU process exited successfully; no demo services or lab QEMU processes remained afterward.

- Three real 128 MiB owner reservations and per-user storage entries were provisioned. Frank's two VMs shared one owner budget; a third request was refused while the second was still durably queued, with the helper deliberately paused.
- Device-key authentication, unknown-device refusal, cross-user action/read/owner-spoofing refusals, and live device revocation passed over real iroh connections.
- Actual Proxmox create/start/stop/delete passed, and disk admission claims were released only after verified deletion.
- The controller was SIGKILLed after accepting an unfinished create. A new process recovered the same operation and VM ID without duplication.
- A 256 MiB thick-zvol allocation failed within Frank's 128 MiB quota while the pool still had 3,488,788,480 bytes available. The failed probe left no volume. Final physical VM/zvol inventories matched the catalog exactly, and all three remaining VMs were stopped.
- Fast checks passed **52 Rust tests**, **22 Python harness/safety tests**, formatting, Clippy with warnings denied and workflow lint. Separately, **57 Swift tests** passed, including actual Rust CLI/daemon integration and fail-closed handling of pending-ledger read/write failures. Debug and release Swift builds passed; the GUI was not launched or signed.

Earlier disposable attempts were retained as failures, not counted as passing runs. They exposed the required root primary group for Proxmox IPC, a read-only boot-poll timeout under emulation, and an overly specific quota-error assertion. These were corrected and regression-tested before the clean full run. No production SSH/API calls, production mutations, repository publishing, or persistent service installation on the development Mac occurred in this phase.

### Bootable guest: failed attempts (November 9–10)

The first combined attempt passed all eight baseline checks but correctly refused the guest owner under the old 1024 MiB admission limit; it remains failed evidence, not a guest-boot success.

The next single-threaded attempt passed the baseline and image comparison but reached an initramfs emergency shell. Its read-only diagnostics found the correct root PARTUUID, with disk discovery only around kernel timestamp 696 seconds under nested software emulation. Diagnostic console input was explicitly recorded, and this attempt is not unattended-boot evidence. Controller/guest runs were then switched to multithreaded outer TCG with four emulated CPUs.

A one-vCPU nested guest under multithreaded outer emulation subsequently reached systemd unattended, but critical services repeatedly timed out and SSH did not become reachable within the 2400-second readiness deadline. A four-vCPU attempt then mounted root but missed the early EFI-device deadline and entered systemd emergency mode without console intervention. Both remain failed evidence. The fixture was changed to grant its guest four virtual CPUs and use a lab-only image derivative with longer systemd deadlines, and the post-boot check began requiring `systemctl is-system-running --wait`.

The first timeout-only derivative booted unattended, mounted EFI, and completed cloud-init. A longer read-only SSH diagnostic confirmed owner authentication and healthy systemd, but cloud-init reported the Proxmox-generated `user` deprecation as `degraded done`. That attempt was interrupted and remains incomplete, not a passing guest suite. Its diagnostic evidence is retained under `.lab/run-controller-20261109T231711-9a1c4a/evidence/`. This led to the supported default-user mapping in recipe v2. SSH command probes were raised from a 60-second to a 180-second bound because the successful diagnostic took longer than the old probe. No diagnostic sent guest-console input or changed guest configuration.

### Read-only discovery of the existing host

Collector v2 ran over strictly pinned, non-forwarding SSH against the existing Hetzner PVE **9.2.26** node. The capture at `2026-11-10T01:32:56Z` is November 9 in America/Chicago. It validated without warnings: ten datasets, six volumes, three VMs, no containers or snapshots, and all double-read consistency checks true. No Proxmox/ZFS configuration, guest state or access policy was changed; SSH can still generate ordinary access logs.

The explicit mapping binds VM 101 and `dev-erin` to Erin, 102 and `dev-frank` to Frank, and 103 and `dev-heidi` to Heidi, using dataset GUIDs and exact disk-slot maps rather than names as authority. Each observed owner allocation has a 1.5 TiB quota and reservation; all six zvols are fully reserved. The first import recorded the observation, a separate-process re-import returned `Duplicate`, and reopening/verifying the database found one snapshot, one import and two intact log entries. All management flags remain false and no controller state was created.

Private local artifacts are under `.lab/hetzner-discovery-v2-20261110T013233Z/`: `snapshot.json`, the reviewed `mapping.json`, final importer outputs, `receipt.json`, and **`observations-final.redb`**. Use this final database; the earlier `observations.redb` is retained as intermediate evidence from before correcting missing NIC model/MAC extraction. Do not publish this operational inventory or the `.lab` directory. The final receipt checks that the collector actually sent has the import record's expected source hash.

The initial v1 capture was correctly refused because PVE returned equal storage-content sets in different orders. It was retained as rejected evidence, not modified to bypass validation. A new v2 capture sorts only set-valued storage `content`/`nodes`; regression tests still refuse actual membership changes.

### Legacy five-case smoke run

The legacy Python-worker matrix (`lab.py test --smoke --timeout 1800`, since removed) passed against Proxmox VE **9.2.26**, kernel **7.0.14-28-pve**, using single-threaded x86 TCG on an ARM Mac. The selected suite reported 4,521.1 seconds (about 75 minutes), reusing the already-built base. QEMU then shut down cleanly. Evidence is retained in `evidence/real-proxmox-20261109T123405-575f27/`.

- All five allocations passed identity-preserving reruns and actual quota-refusal checks.
- Fresh CLI processes recovered after submission of VM creation, after observation of zvol creation, and before submission of VM start.
- Both completed-operation host reboot recovery and reboot with a durably recorded pending cloud-init action passed.
- Final inventory contained exactly the expected VMs and zvols, with no orphan volumes.
- Deliberate quota drift was rejected without overwriting it; a changed VM owner was also rejected.
- The fast checks passed **23 Rust tests and 16 Python tests**, formatting, Clippy with warnings denied, syntax checks, and actionlint. These include the 96 simulated recovery scenarios and all 21 redb fresh-process client boundaries.

This is a verified **five-case real smoke run**, not a run of the full 23-allocation real matrix. That smoke run did not test nested guest boot or image import; those are covered by the later `guest-test` extension. The earlier failed disposable runs exposed the host cloud-init seed-discovery problem and a multi-threaded-emulation kernel panic; neither was treated as a successful run or retried against production.

The legacy matrix created 23 allocations in full mode (one normal case, seven
creation steps × three client-process boundaries, and a server-reboot recovery
case) and five in smoke mode. It exercised per-person ZFS datasets with quota and
reservation, non-sparse storage entries, deterministic VM IDs and thick zvols with
ownership markers, Proxmox-generated cloud-init drives and blank nested QEMU
guests through `pvesh` over pinned SSH. Each recovery case used a fresh CLI
process and checked durable intents before submission, identity-preserving no-op
reruns, real quota refusal, recovery after a confirmed host reboot (including a
pending cloud-init action), and detection of orphan volumes, quota drift and
changed ownership markers. It has since been removed from the repository; the
Rust `control-test` and `guest-test` suites replace it.
