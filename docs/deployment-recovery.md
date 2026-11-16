# Offline recovery and service templates

`pve-dev-ops` provides a paired offline export, verification and **quarantined**
restore of a stopped controller/helper pair, plus the `prepare-adoption` step
used by the managed backend (see [persistent-management.md](persistent-management.md)).
The repository also ships lab systemd unit templates in
[`deploy/systemd/`](../deploy/systemd). None of this installs anything on a host
by itself, and there is no command that activates a restored backup.

Backups accept the `lab`, `physical_trial` and `managed` helper bindings, and a
simulated backend for local tests.

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
terminal result, refuses helper jobs absent from the controller (except
finished jobs whose operation was archived by `pve-dev-ops prune` and survives
as a request-ID tombstone; see `storage-retention.md`), and verifies
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

## Lab service templates

`deploy/systemd/` contains two unit templates: `pve-devd.service` for the
unprivileged controller and `pve-dev-host.service` for the root helper.
`tests/maintenance.rs` checks the properties described below. Build the
static Linux binaries with `scripts/build-lab-binaries.sh` (output in
`target/x86_64-unknown-linux-musl/release`) and keep their build provenance.

There is no staging or packaging tool in this repository. Earlier development
used a separate `deployment/stage.py` script to assemble binaries, units and a
SHA-256 manifest into a reviewable tree. That script was never committed here,
so record the binary and unit hashes yourself before installing.

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

1. Verify the recorded binary/unit hashes and executable provenance; create a
   system `pve-dev` group/account with no login shell or home. Do not grant
   sudo/PVE roles.
2. Install `pve-devd` and `pve-dev-host` (which the units run), plus
   `pve-devctl` and `pve-dev-ops` for administration, under root-owned
   `/usr/local/libexec/pve-dev/` (0755), and the units under
   `/etc/systemd/system/` (0644). The binary directory must not be writable by
   the daemon account.
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

The paired backup tests in `tests/maintenance.rs` cover queued accepted intents,
helper-submitted work and interrupted helper intents with their exact
fingerprints, deduplication, identity and revocation preservation, terminal-proof
mismatch, active database locks, corruption, missing files, wrong pins,
symlinks/hardlinks, private modes, writable output parents and ancestors, and
direct-binary quarantine. Schema-specific cases check that policy edits, user
deletion, shrink holds, growth claims and partially committed profiles survive a
quarantined restore without activating. `scripts/test-maintenance-local.py` runs
a five-check process drill with real binaries: it kills the daemon after durable
acceptance, exports both stopped journals, verifies the pinned bundle, checks
that the accepted operation and VM identity are preserved, and confirms both
restored binaries refuse to start.

These are separate from actual systemd/PVE boot tests and do not claim
operational restore activation is solved. For the November 10, 2026 validation
run, see [CHANGELOG.md](../CHANGELOG.md#2026-11-10).
