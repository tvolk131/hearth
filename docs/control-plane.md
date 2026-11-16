# Experimental control plane

This is an experimental control plane, not a production-qualified installation. It never connects to a Proxmox host automatically. The helper has three real backends, each bound to an explicit root-owned configuration: the disposable `lab` backend described here (only the authenticated `pve-lab` environment, `labpool`, and VM IDs 9001–9099, with tiny lab policy limits), the scoped `physical_trial` backend ([physical-trial-backend.md](physical-trial-backend.md)) and the `managed` backend for real hosts ([persistent-management.md](persistent-management.md)). A simulated backend serves local tests.

The project history, including dated validation runs, is in [CHANGELOG.md](../CHANGELOG.md).

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

1. An authenticated request is validated, deduplicated, and admitted in one redb write transaction. The resource claim, frozen job and queued operation are committed with immediate durability before success is returned.
2. An asynchronous worker submits the frozen job to the local helper. Repeated submissions carry the same operation ID and are deduplicated there too.
3. The helper commits an intent before each external effect, stores the actual Proxmox task ID when available, observes task completion and exact postconditions, and durably verifies the step.
4. The controller marks the operation verified only after helper verification. Disk and RAM claims remain charged while work is pending, blocked, or being deleted. Verified deletion releases claims; VM ID tombstones are retained and never reused.

Transport failures do not imply an operation failed. Retry a mutation with **the same request ID and identical parameters**. Reusing an ID with different parameters conflicts. IDs are scoped to the authenticated user. Client cancellation stops waiting; it does not cancel accepted server work. Revocation stops subsequent requests but does not roll back already accepted operations.

Not every external failure can be retried safely. An uncertain effect without sufficient durable proof is **blocked pending investigation**, retaining its claims. The helper checks GUIDs, VM identity, device topology, ownership markers, and task evidence; it does not adopt a resource solely because its name matches. Same-boot ambiguous absent effects do not trigger speculative retries. There is deliberately no generic force/retry/unblock endpoint in this prototype.

The helper sorts every step failure into one of three classes. **Retryable** failures (reads, observations and command transport while no effect intent is outstanding) back off from 2 s, doubling to at most 60 s, and block only once one step has kept failing for 30 minutes. The first-failure time and attempt count are journaled, so the budget survives restarts. **Permanent** refusals (validation, admission) block with that class recorded. Everything else is **ambiguous** and blocks as before. When an effect command itself returns an error, the helper reconciles it exactly like a crash: it observes once and verifies only on an exact match, otherwise it blocks. When `submit` refuses a job before journaling it, the helper returns the typed wire code `rejected`. The controller then ends the operation as terminal `failed` with the helper's reason and releases its claims; transport failures still leave it queued. Only the local helper's typed ambiguity moves a not-yet-committed migration to blocked; a peer's error text never does. On SIGTERM/SIGINT the helper starts no new step and lets the step in flight finish (up to 15 minutes); a second signal exits immediately. The service manager's stop timeout must exceed that grace period.

Do not delete or reinitialize either database to clear a blocked operation. Loss of one database is not a clean slate. Stop both services and preserve both journals and the actual infrastructure for investigation. `pve-dev-ops` now supports a paired offline logical export and a fresh, quarantined restore; see `deployment-recovery.md`. Both startup paths refuse restored state pending independent infrastructure reconciliation. Reconciliation and restore activation remain unimplemented; copying a live database is not a supported backup.

## Resource policy

- Each user receives `labpool/control/USER`, with ZFS quota and reservation set to that user's budget, and a non-sparse Proxmox storage entry `ctl-USER`.
- Administrators can edit an existing positive lab policy using revision-checked `update_user_policy`. The owner is frozen while pending/blocked, admission holds the larger previous/target commitments, and contraction releases capacity only after helper verification. See `quota-editing.md` for the actual usage/metadata floor, crash recovery, compatibility and qualification limits. This is not guest disk resizing.
- Each VM receives a thick root zvol and a cloud-init zvol. Admission charges root size plus cloud-init and metadata allowance. Per-user VM counts and aggregate CPU/RAM allocations include all non-deleted machines, conservatively including stopped machines.
- Minimum memory admission is bounded per user and globally; maximum configured memory is bounded per user. Proxmox receives VM memory/balloon settings. Guest ballooning is not a guarantee against host memory pressure; production needs operating headroom, ZFS ARC policy and pressure monitoring.
- CPU cores and `cpuunits` are set per VM. The prototype does not implement a per-person cgroup CPU scheduler, so many VMs can receive more total weight than one VM. CPU sharing is not strict per-user aggregate fairness.
- Administrator-only offline shrink for supported image guests and a distinct graceful shutdown action are documented in `disk-shrink.md`. Shrink retains the original and its workspace claim until explicit guest verification and recovery removal; it never contracts the user budget automatically. Offline disk expansion exists only under explicitly scoped configurations ([maintenance-features.md](maintenance-features.md)); snapshots, clones and automatic drift repair are not exposed. The default create flow retains blank root disks. An opt-in `guest` specification selects a distinct durable pinned-image import flow with owner SSH/cloud-init on a harness-provisioned isolated bridge; see `bootable-guests.md`. Both flows leave VMs initially stopped. A running status alone is not OS/SSH readiness.
- VM state in the catalog is the last verified operation outcome, not a continuous live inventory. Out-of-band changes are checked before mutations but are not continuously reflected in the dashboard.

## Commands

Build from the repository root with `cargo build --locked --bins`, or with `nix develop ./nix --command cargo build --locked --bins`. Use the small `nix/` flake path rather than snapshotting the project directory and its virtual disks into Nix.

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

For the real disposable Proxmox adapter:

```sh
nix develop ./nix --command bash scripts/build-lab-binaries.sh
nix develop ./nix --command python3 lab/lab.py control-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 1800
```

Both macOS and Linux produce static musl binaries at that same path, avoiding Nix dynamic-loader paths that would not exist inside the Proxmox guest. The build requires Rustup's 1.99.1 toolchain and its `x86_64-unknown-linux-musl` target (`rustup toolchain install 1.99.1 --profile minimal --target x86_64-unknown-linux-musl`), Zig and cargo-zigbuild. Zig and cargo-zigbuild come from the Nix shell. No Homebrew or global QEMU installation is needed. QEMU is provided by Nix. The lab creates sparse virtual disks under `.lab/`, with a fresh root overlay and two 4 GiB mirrored ZFS disks for each run. It reuses the sealed Proxmox base image; actual disk use grows as data is written. See `lab/README.md` for base-image creation and safety boundaries.

The real controller suite runs the network daemon unprivileged inside QEMU and the separate helper as root. It checks actual reservations for three users, ZFS quota rejection, ownership enforcement, create/start/stop/delete, post-acceptance daemon SIGKILL recovery, device revocation and exact VM/zvol inventories. Forwarded SSH and iroh ports bind only to host loopback. The CLI speaks real iroh across the virtual network. Successful evidence is copied to `evidence/real-controller-*/`, excluding private keys and disks; failed-run diagnostics remain in `.lab/run-controller-*/evidence/`. These tests are not evidence of production suitability or of bootable cloud-image provisioning.

`.github/workflows/checks.yml` declares Linux Rust/recovery/process tests, macOS Swift-to-Rust integration, and a real Proxmox software-emulation suite. The Linux and macOS jobs run on every pull request and every push to `main`; the real-Proxmox job runs nightly and on manual dispatch.

## Remaining production gates

The `managed` backend ([persistent-management.md](persistent-management.md)) adds an explicit root-owned environment binding, real-sized allocations and reviewed adoption of existing VMs. Still open: reviewed networking/SSH enrollment and image lifecycle policy beyond the pinned image catalog, helper privilege/socket review and an independent security review, audited reconciliation for blocked operations other than a failed start, independently reconciled restore activation, versioned database/operation-plan upgrade migrations, and tested restore plus host reboot/failure behavior. Add continuous inventory drift observation, bounded history/pagination and request rate limits. Public relay traversal, distributable app bundling/signing/notarization and actual Linux service installation/reboot are not claimed as tested. Lab systemd templates exist in `deploy/systemd/`; they do not install or qualify a production service.
