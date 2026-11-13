# Offline recovery and service staging

This delivers a paired offline export, verification and **quarantined** restore
tool, plus a reviewable Linux service payload. It does not install anything on
either Hetzner host. The helper still accepts only its authenticated disposable
`pve-lab`/`labpool` environment. There is no production write adapter or host
adoption path here.

## What is backed up

`pve-dev-ops backup` exclusively opens **both** stopped redb databases before
reading either logical record, and holds both locks through export. It exports
the controller catalog, complete helper journal, controller configuration and
server identity key together in one new private directory. A simulated backend
also requires its separate external-world file. An active controller or helper
causes refusal; a live raw database copy is never used.

The catalog includes enrolled/revoked devices, accepted intents, deduplication
keys, resource claims and VM tombstones. The journal includes frozen jobs,
fingerprints, resource identities and task receipts. Validation compares actual
frozen operation payloads, requires helper evidence for every controller
terminal result, refuses helper jobs absent from the controller, and verifies
binding, server public identity and pool GUID against independent expected
pins. A helper may legitimately be ahead of an unfinished controller operation.
The tool does not require identical status when an acknowledgement was lost.

The export is one versioned JSON bundle with a payload SHA-256. Retain its
printed receipt independently, and require that digest at verification/restore.
The digest detects accidental corruption and mixed files; it is **not a
signature** or an encryption scheme. The bundle contains the server's private
key and all enrollment metadata. Keep it mode 0600 in a mode 0700 directory and
use encrypted, access-controlled off-host backup storage. Do not publish it.

## Quiescence is more than stopping two processes

Stop the controller first to prevent admission. Stop the helper second, then
inspect outstanding Proxmox task UPIDs and orphan `pvesh`, `qm`, ZFS/image-import
commands, including tasks accepted by Proxmox before the helper died. They can
outlive a killed helper or its service. Wait for a settled outcome and preserve
the corresponding task/resource evidence; do not kill a storage operation just
to obtain a backup. Confirm no other automation can mutate this environment.

`--external-effects-quiesced` is the operator's explicit assertion of this
condition, not an automatically established fact. Database locks prove the
writers are absent; they cannot prove physical infrastructure is quiescent.
Pending durable jobs may remain, provided no external effect is still running.
Record the quiescence procedure/task evidence alongside the separate receipt.

Examples (placeholders must come from independently recorded setup information):

```sh
pve-dev-ops backup --controller-dir /var/lib/pve-dev/controller \
  --helper-dir /var/lib/pve-dev-host --output /secure/new-backup \
  --external-effects-quiesced --expected-environment ENVIRONMENT \
  --expected-server SERVER_PUBLIC_ID --expected-pool-guid POOL_GUID

pve-dev-ops verify --bundle /secure/new-backup --expected-bundle RECEIPT_SHA256 \
  --expected-environment ENVIRONMENT --expected-server SERVER_PUBLIC_ID \
  --expected-pool-guid POOL_GUID

pve-dev-ops restore --bundle /secure/new-backup --output /secure/new-restore \
  --expected-bundle RECEIPT_SHA256 --expected-environment ENVIRONMENT \
  --expected-server SERVER_PUBLIC_ID --expected-pool-guid POOL_GUID
```

Use absolute canonical paths (`/private/tmp`, not the macOS `/tmp` symlink).
Output parents must be 0700 and owned by the offline operator. Their ancestors
must belong to that operator or root and must not be group/world-writable,
except for sticky trusted temporary directories. A private child inside an
ordinary publicly writable directory is refused because the child can be renamed.
Existing output directories, linked/nonprivate source files, partial state,
wrong pins, corrupt bundles and inconsistent pairs fail closed. A failed write
can leave a partial new staging directory; it is never retried into or activated.

## Restore remains quarantined

Restore writes fresh `controller/` and `helper/` subdirectories and, for
simulation only, `simulated-world.json`. It does not overwrite a service state
directory, change socket paths or other configuration, change ownership, contact
Proxmox, start a worker, clear a blocked operation or reset either journal.

Each restored state directory contains `restore-review-required` before any
database is written. Both executable startup paths reject this marker, including
symlinks, directories and marker inspection errors. Service units also refuse
quarantined paths. There is deliberately **no activation or marker-removal
command**. This is a recoverable offline inspection artifact, not a supported
unattended disaster-recovery workflow.

An old consistent pair can still be inconsistent with *current infrastructure*.
For example, a backup predating a completed create/delete or device revocation
would lose that later evidence. Same-environment pairs with identical histories
up to their split cannot always be distinguished by internal consistency checks.
Neither hashes nor schema validation solve rollback. Restore activation requires
a future independently reviewed reconciliation procedure comparing current
resource GUIDs, VM UUIDs, task receipts, revocations and BOTH journal histories.
Until that exists, preserve the original disks/state, keep the restore isolated,
and investigate; do not restart workers against physical resources from an old
backup. There is no automatic replay/force-unblock path.

## Reviewable service payload

`deployment/stage.py --bin-dir /abs/linux-x86_64-binaries --output /abs/new-stage`
creates a new tree with four ELF binaries, two systemd units and a SHA-256 file
manifest. It never writes `/etc`, creates accounts or starts services. Native
macOS binaries are refused. Build the Linux artifacts using the existing
cross-build/lab workflow, retain build provenance, then review the payload.

The daemon uses a dedicated `pve-dev` account/group, no capabilities, private
state, no access to home directories/devices and a restrictive systemd sandbox.
Only that account/group can use the helper's 0660 Unix socket. The root helper
retains the privileges required for ZFS/Proxmox; it has no network listener.
Its unit requires `ConditionHost=pve-lab`, a separate lab config and lab identity
file. The executable additionally attests hostname, nonce, labpool property and
exact pool GUID before accepting effects. The service templates do not bypass
any of these checks. They are lab templates awaiting a real systemd smoke test,
not a production packaging qualification.

For an explicitly provisioned disposable Linux lab only, the reviewed installation
sequence is:

1. Verify the staged manifest and executable provenance; create a system
   `pve-dev` group/account with no login shell or home. Do not grant sudo/PVE roles.
2. Install the four binaries under root-owned `/usr/local/libexec/pve-dev/`
   (0755), and the units under `/etc/systemd/system/` (0644). The binary directory
   must not be writable by the daemon account.
3. Create root-owned `/etc/pve-dev-host` (0700) and install the independently
   reviewed nonce/pool-GUID/image allowlist `lab.json` (0600). Preserve image
   hashes, identity and pool pins; initialization is not an upgrade operation.
4. Create `/var/lib/pve-dev` owned by `pve-dev` (0700), then run the existing
   `pve-devd init` **as that account** to create its new `controller/` with the
   exact lab binding, socket `/run/pve-dev-host/host.sock`, admin public key and
   explicitly chosen transport. Keep the default loopback route until tested.
5. Run `systemd-analyze verify` on both units, `systemctl daemon-reload`, start
   the helper, inspect `pve-dev-host health`, then start the daemon. Validate
   actual socket mode/ownership, unprivileged daemon UID, descriptor identity,
   enrollment, auth refusal and a disposable VM lifecycle before enabling boot.
   Enabling services is a separate operational step after these checks.

Upgrade: preserve both private state directories and independently pinned
identities; quiesce, make a paired backup, verify compatible schemas/operation
plans and restart in helper-then-daemon order. No schema migration tool exists,
so a version needing a migration is a stop condition. Do not downgrade against
newer state. On failed upgrade preserve evidence, never reset/reinitialize state.

Uninstall: stop/disable the daemon, stop/disable the helper, inspect external
tasks, then remove units/binaries and daemon-reload. Retain both state directories,
configuration, receipts, identities and resource ownership evidence. Uninstall
must not delete VMs, datasets, accounts with retained evidence or a journal.

## Transport evidence and limits

The direct-mode regression test uses real authenticated iroh UDP endpoints with
temporary local state, restarts the endpoint with the same identity and a new
route, then verifies device revocation. This checks direct-mode endpoint setup
and reconnection on one machine. It does **not** exercise two machines, packet
loss, restrictive NAT, public discovery or relay fallback. Public mode still
uses the iroh N0 preset, but public relay/NAT reliability is unverified. A
cross-network matrix and a reviewed public-listener deployment are still needed.

The paired backup tests cover queued accepted intents, helper-submitted work,
deduplication, identity and revocation preservation, terminal-proof mismatch,
active database locks, corruption, missing files, wrong pins, symlinks/hardlinks,
private modes and direct-binary quarantine. These are separate from actual
systemd/PVE boot tests and do not claim operational restore activation is solved.

## Local validation, November 10, 2026

All nine `cargo test --locked --test maintenance` checks passed, including real
Direct-mode iroh. `python3 scripts/test-maintenance-local.py` also passed its
five-check actual-process drill: it killed the daemon after durable acceptance,
exported both stopped journals, verified the pinned bundle, preserved the same
accepted operation/VM identity, and observed both restored binaries refuse to
start. The simulated fixtures and processes were removed afterward.

Four `python3 -B deployment/test_stage.py` checks passed for exact manifest
coverage, non-Linux binary refusal, symlink refusal and unsafe-parent refusal.
Those tests use synthetic ELF headers; they do not execute or qualify Linux
binaries. Focused Clippy with warnings denied, Rust formatting, all-bin debug
build and Python compilation passed. Socket tests ran outside the restricted
sandbox; the initial sandbox attempt could not bind UDP (`EPERM`). The process
drill initially used an incorrect CLI identity subcommand; it was corrected to
the existing `identity new --path` interface before the passing run.

The non-secret receipt summary is `evidence/service-recovery-20261110/summary.json`.
