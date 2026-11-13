# Experimental control plane

This is an executable prototype, not a production installation. It never connects to the Hetzner server automatically. The real mutation adapter accepts only the authenticated disposable `pve-lab` environment, `labpool`, and VM IDs 9001–9099. Tiny lab policy limits intentionally prevent applying the real 1.5 TiB user allocations.

## Components and authority

```text
SwiftUI app ── typed JSON over stdin/stdout ── pve-devctl
                                               │
                                authenticated iroh/QUIC
                                               │
                                     pve-devd (non-root)
                                      │              │
                              state.redb       restricted Unix socket
                                                     │
                                            pve-dev-host (root)
                                              │            │
                                        helper redb    Proxmox + ZFS
```

- The server derives the user from the authenticated iroh device public key, never a caller-supplied user name. Erin is bootstrapped as administrator; only administrators can create accounts and enroll/revoke devices. Members see and manage their own resources only.
- Clients hold their own private device key and a profile pinning the server public key, environment nonce, and connection route. Private keys are mode 0600. A route is not an authentication credential.
- Loopback is the default transport. `direct` enables explicit IP connectivity without relays/discovery. `public` opts into iroh's public network configuration; it is not exercised by the lab tests. No public listener or production service is installed by a local test.
- The daemon owns authorization, request deduplication, admission accounting, and accepted intents. It has no Proxmox root credentials. The root helper independently validates the lab, operation shapes, resource ownership and immutable job fingerprints before executing a fixed operation set. There is no arbitrary shell/command RPC.
- Proxmox and ZFS are the physical enforcement layer. The daemon's admission policy also prevents accepting collectively over-budget pending requests. Neither enforcement layer replaces the other.

## Durable state and recovery

The authoritative `state.redb` lives on the controller host in its private data directory, not in each GUI. In the disposable real lab this is `/var/lib/pve-dev/controller/state.redb`; the helper's separate journal lives under `/var/lib/pve-dev-host`. Clients need only their identity key and profile. The Swift app additionally stores a private pending-request ledger locally; that ledger is not authoritative infrastructure state.

`state.redb` holds one table per entity (`catalog_users_v2`, `catalog_vms_v2`, `catalog_operations_v2`, `catalog_requests_v2`, `catalog_audit_v2` and so on, plus `catalog_meta_v2` for schema, binding and revision), with JSON row values. Each admission loads the catalog inside its write transaction, applies the change in memory, and writes back only the rows that differ. A request that changes nothing (every read, and every rejected mutation) aborts its transaction: nothing is written and the catalog revision does not move. `Store::open` refuses the retired single-record `controller_catalog_v1` layout and any table it does not own.

1. An authenticated request is validated, deduplicated, and admitted in one redb write transaction. The resource claim, frozen job and queued operation are committed with immediate durability before success is returned.
2. An asynchronous worker submits the frozen job to the local helper. Repeated submissions carry the same operation ID and are deduplicated there too.
3. The helper commits an intent before each external effect, stores the actual Proxmox task ID when available, observes task completion and exact postconditions, and durably verifies the step.
4. The controller marks the operation verified only after helper verification. Disk and RAM claims remain charged while work is pending, blocked, or being deleted. Verified deletion releases claims; VM ID tombstones are retained and never reused.

Transport failures do not imply an operation failed. Retry a mutation with **the same request ID and identical parameters**. Reusing an ID with different parameters conflicts. IDs are scoped to the authenticated user. Client cancellation stops waiting; it does not cancel accepted server work. Revocation stops subsequent requests but does not roll back already accepted operations.

Not every external failure can be retried safely. An uncertain effect without sufficient durable proof is **blocked pending investigation**, retaining its claims. The helper checks GUIDs, VM identity, device topology, ownership markers, and task evidence; it does not adopt a resource solely because its name matches. Same-boot ambiguous absent effects do not trigger speculative retries. There is deliberately no generic force/retry/unblock endpoint in this prototype.

Do not delete or reinitialize either database to clear a blocked operation. Loss of one database is not a clean slate. Stop both services and preserve both journals and the actual infrastructure for investigation. `pve-dev-ops` now supports a paired offline logical export and a fresh, quarantined restore; see `deployment-recovery.md`. Both startup paths refuse restored state pending independent infrastructure reconciliation. Reconciliation and restore activation remain unimplemented; copying a live database is not a supported backup.

## Resource policy

- Each user receives `labpool/control/USER`, with ZFS quota and reservation set to that user's budget, and a non-sparse Proxmox storage entry `ctl-USER`.
- Administrators can edit an existing positive lab policy using revision-checked `update_user_policy`. The owner is frozen while pending/blocked, admission holds the larger previous/target commitments, and contraction releases capacity only after helper verification. See `quota-editing.md` for the actual usage/metadata floor, crash recovery, compatibility and qualification limits. This is not guest disk resizing.
- Each VM receives a thick root zvol and a cloud-init zvol. Admission charges root size plus cloud-init and metadata allowance. Per-user VM counts and aggregate CPU/RAM allocations include all non-deleted machines, conservatively including stopped machines.
- Minimum memory admission is bounded per user and globally; maximum configured memory is bounded per user. Proxmox receives VM memory/balloon settings. Guest ballooning is not a guarantee against host memory pressure; production needs operating headroom, ZFS ARC policy and pressure monitoring.
- CPU cores and `cpuunits` are set per VM. The prototype does not implement a per-person cgroup CPU scheduler, so many VMs can receive more total weight than one VM. CPU sharing is not strict per-user aggregate fairness.
- Administrator-only offline shrink for supported image guests and a distinct graceful shutdown action are documented in `disk-shrink.md`. Shrink retains the original and its workspace claim until explicit guest verification and recovery removal; it never contracts the user budget automatically. General disk growth, snapshots, clones and automatic drift repair are not exposed. The default create flow retains blank root disks. An opt-in `guest` specification selects a distinct durable pinned-image import flow with owner SSH/cloud-init on a harness-provisioned isolated bridge; see `bootable-guests.md`. Both flows leave VMs initially stopped. A running status alone is not OS/SSH readiness.
- VM state in the catalog is the last verified operation outcome, not a continuous live inventory. Out-of-band changes are checked before mutations but are not continuously reflected in the dashboard.

## Commands

Build from this directory with `cargo build --locked --bins`, or from the parent with `nix develop ./pve-dev/nix --command cargo build --manifest-path pve-dev/Cargo.toml --locked --bins`. Use the small `nix/` flake path rather than snapshotting the project directory and its virtual disks into Nix.

```sh
export PATH="$PWD/target/debug:$PATH"
pve-devctl identity new --path /absolute/private/device.key
pve-devctl identity show --path /absolute/private/device.key
pve-devctl profile --identity /absolute/private/device.key \
  --server /trusted/server.json --expected-server VERIFIED_PUBLIC_KEY \
  --output /absolute/private/lab-profile.json

pve-devctl --profile /absolute/private/lab-profile.json dashboard
pve-devctl --profile /absolute/private/lab-profile.json budget
pve-devctl --profile /absolute/private/lab-profile.json vm list
pve-devctl --profile /absolute/private/lab-profile.json vm create \
  --request-id SAME_ID_ON_RETRY --name dev-frank --disk-mib 32
pve-devctl --profile /absolute/private/lab-profile.json operations
pve-devctl --profile /absolute/private/lab-profile.json vm start \
  --request-id ANOTHER_ID --vm-id 9001
pve-devctl --profile /absolute/private/lab-profile.json vm stop \
  --request-id STOP_ID --vm-id 9001
pve-devctl --profile /absolute/private/lab-profile.json vm delete \
  --request-id DELETE_ID --vm-id 9001 --confirm-delete
```

Verify the server key and environment through a trusted channel before creating a profile; copying values from an untrusted descriptor does not establish trust. Account/device commands are under `create-user`, `enroll-device`, and `revoke-device`; use `--help` for their required flags. The GUI uses `--profile PATH rpc` with one typed JSON request on stdin, a JSON response on stdout, exit 0 for success and 2 for a server rejection. Exit 1 means a local/transport failure, not proof that a mutation was not accepted.

Device enrollment uses new iroh identities, not the existing SSH keys. SSH access to a guest is separate from permission to manage that guest. There is no automatic import of the three existing production users or VMs.

`pve-dev-inventory` provides a separate read-only inventory and explicit ownership-mapping workflow (`discovery.md`). Its dedicated redb file stores observations only: even a confirmed import never creates controller users, VMs or operations, and never authorizes management. It is not a production adoption/write path.

## Rerunnable tests

From the project directory:

```sh
nix develop ./nix#checks --command bash scripts/check.sh
nix develop ./nix#checks --command python3 scripts/test-control-local.py
nix develop ./nix --command python3 scripts/test-control-local.py --swift
```

The process suite starts real local daemon/helper/client processes with a simulated host backend, authenticates devices over loopback iroh, kills/restarts the daemon after durable acceptance, restarts the helper, checks duplicate request identity and live revocation, and cleans up its private temporary directory. `--swift` additionally builds the debug/release macOS app and runs its tests against the live Rust CLI, including an idempotent create request. It requires macOS Command Line Tools or Xcode. The helper's unit tests separately inject crashes around external effects and exercise ambiguous-state blocking.

To try the GUI without a hypervisor, add `--serve` to the process suite. It prints the CLI executable and temporary administrator profile paths and keeps the simulated services alive until Ctrl-C. In another terminal, run `cd macos && /usr/bin/swift run PveDevApp`, then select those paths in Settings and refresh. This does not start QEMU or contact any Proxmox server. Closing the demo invalidates its profiles and deletes its temporary keys/state; it is not a persistent management environment.

A local UI smoke check on 2026-11-10 reproduced and fixed an oversized VM-page layout that moved both table content and sidebar navigation outside the window. The native table is now bounded by the detail area's actual size. Dashboard loading, all sidebar destinations, returning to VMs, sorting/filter controls, window zoom, and opening/cancelling the New VM sheet were checked on macOS 26.6.2 against the simulated backend. No mutations were submitted. Debug/release builds and the Swift suite passed; its opt-in Rust CLI integration test was skipped in this rerun. This is local smoke coverage, not a complete visual or interaction audit.

For the real disposable Proxmox adapter:

```sh
nix develop ./nix --command bash scripts/build-lab-binaries.sh
nix develop ./nix --command python3 lab/lab.py control-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 1800
```

Both macOS and Linux produce static musl binaries at that same path, avoiding Nix dynamic-loader paths that would not exist inside the Proxmox guest. The build requires Rustup's 1.99.1 toolchain and its `x86_64-unknown-linux-musl` target (`rustup toolchain install 1.99.1 --profile minimal --target x86_64-unknown-linux-musl`), Zig and cargo-zigbuild. The Rust toolchain/target are already installed on this Mac; Zig and cargo-zigbuild come from the Nix shell. No Homebrew or global QEMU installation is needed. QEMU is provided by Nix. The lab creates sparse virtual disks under `.lab/`, with a fresh root overlay and two 4 GiB mirrored ZFS disks for each run. It reuses the sealed Proxmox base image; actual disk use grows as data is written. See `lab/README.md` for base-image creation and safety boundaries.

The real controller suite runs the network daemon unprivileged inside QEMU and the separate helper as root. It checks actual reservations for three users, ZFS quota rejection, ownership enforcement, create/start/stop/delete, post-acceptance daemon SIGKILL recovery, device revocation and exact VM/zvol inventories. Forwarded SSH and iroh ports bind only to host loopback. The CLI speaks real iroh across the virtual network. Successful evidence is copied to `evidence/real-controller-*/`, excluding private keys and disks; failed-run diagnostics remain in `.lab/run-controller-*/evidence/`. These tests are not evidence of production suitability or of bootable cloud-image provisioning.

`.github/workflows/checks.yml` declares Linux Rust/recovery/process tests, macOS Swift-to-Rust integration, and a real Proxmox software-emulation suite on every PR. Those jobs will run only after this directory becomes a GitHub repository with Actions enabled. No repository has been created or published, and the workflow has not been verified on GitHub runners.

## Verified local result: November 9, 2026

The complete selected controller suite passed against Proxmox VE **9.2.26**, with `complete: true` and all eight integration checks in `evidence/real-controller-20261109T173920-5183e4/summary.json`. It reported 2,353.7 seconds (about 39 minutes) on this ARM Mac under single-threaded x86 software emulation, excluding final diagnostics/shutdown. The QEMU process exited successfully; no demo services or lab QEMU processes remained afterward.

- Three real 128 MiB owner reservations and per-user storage entries were provisioned. Frank's two VMs shared one owner budget; a third request was refused while the second was still durably queued, with the helper deliberately paused.
- Device-key authentication, unknown-device refusal, cross-user action/read/owner-spoofing refusals, and live device revocation passed over real iroh connections.
- Actual Proxmox create/start/stop/delete passed, and disk admission claims were released only after verified deletion.
- The controller was SIGKILLed after accepting an unfinished create. A new process recovered the same operation and VM ID without duplication.
- A 256 MiB thick-zvol allocation failed within Frank's 128 MiB quota while the pool still had 3,488,788,480 bytes available. The failed probe left no volume. Final physical VM/zvol inventories matched the catalog exactly, and all three remaining VMs were stopped.
- Fast checks passed **52 Rust tests**, **22 Python harness/safety tests**, formatting, Clippy with warnings denied and workflow lint. Separately, **57 Swift tests** passed, including actual Rust CLI/daemon integration and fail-closed handling of pending-ledger read/write failures. Debug and release Swift builds passed; the GUI was not launched or signed.

Earlier disposable attempts were retained as failures, not counted as passing runs. They exposed the required root primary group for Proxmox IPC, a read-only boot-poll timeout under emulation, and an overly specific quota-error assertion. These were corrected and regression-tested before the clean full run. No production SSH/API calls, production mutations, repository publishing, or persistent service installation on this Mac occurred in this phase.

## Follow-up regression results

The November 9–10 follow-up fast validation passed **84 Rust tests**, formatting, Clippy with warnings denied, and workflow/shell syntax checks; the updated lab suite passed **36 Python tests**. The local process harness also passed all five checks, including daemon SIGKILL recovery, helper persistence, live revocation and real Swift-to-Rust RPC. Swift debug/release builds and **57 Swift tests** passed. These simulator/process results are separate from the real Proxmox results above and from the bootable-guest extension in `bootable-guests.md`.

A later full validation attempt failed once in the unchanged legacy simulator test `exclusive_lock_blocks_a_second_writer`, at reopening state immediately after dropping the first writer. The isolated rerun and the subsequent complete `scripts/check.sh` both passed, without changing the test or lock implementation. The cause is not established; the passing rerun does not establish that this intermittent failure is resolved. Private logs are retained in `.lab/final-fast-checks.log` and `.lab/final-fast-checks-rerun.log`.

The November 10 combined real-Proxmox run also passed **all eleven checks** in `evidence/real-controller-20261110T000212-44dcfa/summary.json` (`complete: true`, `nested_guest_boot_verified: true`, Proxmox 9.2.26, 3,576.5 seconds excluding final diagnostics/shutdown). In addition to the eight baseline checks above, it independently compared a pinned image with its thick root zvol, booted a real Debian guest unattended with clean cloud-init and healthy systemd, verified owner SSH/sudo and foreign-key refusal, and verified stop/delete with no guest volumes left behind. This used the explicitly documented lab-only image derivative and multithreaded emulation, not the unchanged upstream image or production guests. See `bootable-guests.md` for the image recipe, evidence, earlier failures and remaining boundaries.

## Remaining production gates

Before enabling any production writes: implement explicit production environment binding and an audited transition from observations to managed resources, use normal-sized resource policies, extend lab-only image guests to reviewed production networking/SSH enrollment and image lifecycle policy, complete helper privilege/socket review and independent security review, implement audited blocked-operation reconciliation and independently reconciled restore activation, add versioned database/operation-plan upgrade migrations, and test restore plus host reboot/failure behavior. Add continuous inventory drift observation, bounded history/pagination and request rate limits. Public relay traversal, distributable app bundling/signing/notarization and actual Linux service installation/reboot are not claimed as tested. Lab systemd templates and an offline payload stager exist; they do not install or qualify a production service.

## November 10–11 recovery audit and read-only host support

The independent state-machine audit reproduced two high-severity recovery identity gaps and an overlapping-executor gap, all in disposable simulation. Resumed start/stop could reuse an earlier verification after VM replacement; resumed delete could destroy a newly attached foreign volume. The helper now revalidates complete recorded identities at action reconciliation/effect boundaries, checks stopped/unprotected state before destruction, and rechecks absence before releasing capacity. An async execution mutex prevents overlapping `run_pending` calls. See `state-machine-audit.md` for baseline failures, ten passing regressions/model tests and the remaining check/effect race with external administrators. This is not a formal proof or a new real-Proxmox mutation qualification.

The read-only GUI path (`read-only-gui.md`) successfully collected fresh, pinned observations from both real hosts. It creates no controller resources or management authority. The controller/helper remain undeployed on those hosts. Offline paired recovery and direct-mode iroh restart/revocation are covered by focused tests and a local-process crash/export/quarantined-restore drill; cross-machine public relay/NAT behavior remains unverified. The historical legacy lock flake did not recur during bounded stress, but its cause remains unresolved.

Integrated validation passed 116 Rust tests, 78 Swift tests including the real Rust CLI integration, 36 Python lab tests, four Python staging tests, both Swift build configurations, formatting, all-target Clippy with warnings denied, Python/shell syntax checks and workflow lint. The local controller process harness passed all five checks. After explicit launch approval, the isolated review app's native visual check also passed against both real hosts, with correct per-host VM placement, recovery labels and read-only controls. Evidence is in `evidence/parallel-hardening-20261111/summary.json` and `evidence/read-only-gui-20261111/native-visual-qa.md`.

The legacy `pve-dev` planner and SQLite simulator remain separate tools for the existing inventory; neither is the controller's database. `cargo run` still defaults to that legacy binary. The legacy gated real-lab reconciler also remains separate from the multi-user daemon/helper test suite.

## November 10–11 quota and migration follow-up

The lab controller, CLI and GUI now support administrator policy edits with expected-revision checks, immutable old/target policies, conservative capacity holds and an owner freeze until helper verification. Reductions cannot discard configured allocations or actual ZFS use. The helper journals quota/reservation effects separately, rechecks physical identities during recovery, and retains claims on ambiguous outcomes. Memory/CPU-only edits preserve storage properties. Zero/unlimited/removal and guest disk resizing remain unsupported. See `quota-editing.md` and `quota-state-review.md`; production host mode remains read-only and the complete new Rust quota operation still needs real-Proxmox qualification.

Final integrated validation passed **137 Rust tests**, **84 Swift tests** with actual Rust RPC, **54 Python lab tests**, four offline staging tests, nine local-process checks, both Swift build configurations, formatting, all-target Clippy with warnings denied, syntax checks and workflow lint. This includes twelve quota tests with 24 crash combinations and five adversarial quota tests (four independent review cases plus a parent follow-up). Evidence is in `evidence/quota-integration-20261111/summary.json`. No new native quota-editor visual check was performed.

Integration also reproduced the legacy simulator's inherited-descriptor lock-retention failure class and fixed normal-drop release without weakening live-writer exclusion. All aggregate checks pass after correcting a parallel-inheritance assumption in the new proof test. The original intermittent fork timeline remains untraced, and hard termination can still leave recovery delayed by an inheriting child. See `legacy-lock-recovery.md`.

The separate two-independent-node Proxmox spike passed a stopped toy-VM copy with byte-exact root/cloud-init disks, preserved guest identities, thick backing at both ends, target collision/repeat refusal and retained-source start refusal. Both API tokens were revoked and both owned QEMU hosts shut down. This is a conditional go for the native transfer primitive, not a migration API or production qualification. Interruption/lost-ack recovery, reboot fencing, bootable guest identity, custom snippets and the loopback API over iroh remain gates. See `migration-spike.md` for real evidence and `migration-handoff.md` for the proposed protocol and bounded executable model.


## November 11–12 disk-shrink qualification

The administrator-only controller/helper/CLI/GUI shrink flow is implemented for
controller-created single-ext4 GPT lab guests. The fresh real-Proxmox fixture
passed two shrink cycles (4096→2560→2048 MiB), paired service restarts, guest
cloud-init/systemd/SSH health, unchanged guest identities and random sentinel
data, graceful shutdown, explicit recovery deletion, real unused-quota
contraction and orphan-free normal deletion. A completed-copy/lost-ack helper
kill blocked without replay and preserved the original. All owned local groups
retired without forced cleanup. See [disk-shrink.md](disk-shrink.md) and the
report.

Validation passed 174 Rust tests, 115 Python lab tests, 97 Swift tests including
real CLI integration, 87 deployment tests and ten local process checks, plus
formatting, Clippy and both Swift build configurations. No new native GUI visual
check was performed. This supersedes the earlier checkpoint's unsupported
guest-resize status only for this lab adapter. Production execution/adoption,
Heidi's layout/workspace needs and generic blocked reconciliation remain
unsupported; no production host or VM was changed.
