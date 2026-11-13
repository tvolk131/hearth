# `qm remote-migrate` viability spike

Result: **conditional go for the stopped-transfer primitive; no production
migration qualification.** A clean two-node run passed on PVE 9.2.26,
qemu-server 9.2.15 and OpenZFS 2.4.4-pve1. No Rust migration API or production
write capability is introduced. This is separate from the abstract handoff model
in `migration-handoff.md`.

The later supervised two-host hardware trial passed a bootable stopped copy with full root/cloud-init byte checks, retained-source start refusal, guest identity preservation and complete cleanup. It required a narrowly guarded metadata-only repair because PVE retained a redundant cloud-init size annotation. Automatic trial reruns remain disabled; successful bootable-copy lost-ack and target host-reboot cases remain unrun. This supplies supervised primitive evidence, not a general Rust/CLI/GUI migration feature or unattended migration qualification.

The published Proxmox command reference calls `qm remote-migrate` experimental,
accepts a target API token and certificate fingerprint, and defaults to retaining
the original VM stopped. It also accepts explicit storage and bridge mappings.
Those documented options do not prove our thick-storage, identity, interruption
recovery or single-runnable-owner requirements.

Primary references:

- [Proxmox generated command reference](https://github.com/proxmox/pve-docs/blob/master/generated/qm.1-synopsis.adoc#cli_qm_remote-migrate)
- [Proxmox remote-migration CLI implementation](https://github.com/proxmox/qemu-server/blob/master/src/PVE/CLI/qm.pm)
- [Proxmox incoming migration tunnel implementation](https://github.com/proxmox/qemu-server/blob/master/src/PVE/API2/Qemu.pm)

## Isolation

`lab/remote_migrate_spike.py` accepts no host, SSH config, physical disk or user
credential. It creates two new qcow2 overlays of the existing verified local
base, plus two blank 2 GiB virtual mirror members for each node. All management
SSH/API forwarding binds the Mac's 127.0.0.1. A second virtual NIC uses a QEMU
socket connection bound to loopback and an isolated private /30 between the
two guests. There is no host bridge, public listener or production endpoint.

Both overlays begin from the same sealed package base. Before creating any VM,
each clone refuses existing guests or corosync configuration, replaces its own
throwaway pmxcfs database, generates new CA/certificate and SSH host keys, and
receives a distinct node name/machine ID. The old base remains unchanged. A
per-run nonce, fixed toy VM IDs 9001/9002 and each node's current name gate worker
operations. Distinct identities and one-node cluster status are checked before
the transfer. This is not a test of host reboot or production identity adoption.

The target API token has privilege separation enabled and expires after two
hours. Its ACLs are limited to the toy VM, destination datastore and bridge,
plus the required `Sys.Incoming` and read-only `Sys.Audit` privileges. It is not a
general host/root role, but the global audit right exposes more metadata than a
strict single-job capability.
Only token metadata is exported as evidence; secret values pass over pinned
SSH stdin and remain in private fixture files. Certificate fingerprints are
verified before authentication. Token scope/cleanup and transfer behavior must
be established by the actual run, not inferred from the fixture configuration.

The harness owns both QEMU child PIDs, uses the existing lab lock and 10 GiB free
space floor, and shuts down or terminates only those children. Images, private
keys, tokens and failed-run details stay under private `.lab/migration-*`.
Exported evidence is a narrow collection of status/config hashes and task
outcomes, never a copy of the private run directory.

## Initial experiment

The first run uses an explicitly stopped toy VM with a 64 MiB thick ZFS root
containing random test bytes, a Proxmox cloud-init drive, a stable SMBIOS UUID
and explicit source/target storage and bridge mappings. It checks exact root
bytes/size, source and destination VM identities, raw cloud-init bytes and
generated metadata, target refreservation, source retention/lock behavior,
target startability and token revocation. A running blank QEMU process does not
prove OS boot or SSH key preservation; no such claim is made by that fixture.

Remaining probes include interrupted transfer, retry after a lost successful
acknowledgement, explicit denial of unrelated target resources, custom snippet
handling and source reboot fencing. A clean baseline alone is not a production
go decision. Live migration, CPU portability, disk shrinking and cross-network
iroh traversal are outside this first spike.

Run only from `pve-dev/`:

```sh
nix develop ./nix --command python3 -m unittest discover -s lab -p test_remote_migrate_spike.py -v
nix develop ./nix --command python3 lab/remote_migrate_spike.py --timeout 1800
```

Ten harness safety/contract tests currently pass. The additions cover PVE's
successful human-readable `set` output and asynchronous task failure: accepted
UPID is never treated as completed migration. The actual PVE runs exposed these
requirements before a clean positive transfer was obtained:

- Independent-node preparation needs the base management NIC's cached MAC/PCI
  placement in each separate user-NAT domain. A fresh cloud-init instance on an
  installed PVE clone can race its root SSH authorization symlink into pmxcfs.
  The fixture therefore retains the cached boot seed until pinned SSH setup,
  then replaces host/machine/CA/cluster SSH identities in the private overlays.
- Fresh `pvecm` certificate initialization also needs an RSA host public key;
  the original minimal seed supplied only Ed25519. Missing types are generated
  inside the throwaway clone, and all active node identities are checked distinct.
- Destination access additionally needs global **`Sys.Audit`**. Without it,
  the source API returned HTTP 500 wrapping destination HTTP 403. This grants
  wider read-only host metadata visibility than a strict per-VM capability.
  `Sys.Modify` and blanket Administrator roles were not added. A future receiver
  proxy could keep this PVE credential local and expose only job-scoped actions;
  that proxy is not implemented here.
- A pre-existing destination VM can be rejected **asynchronously**, after the
  source returns a UPID. The clean negative experiment ended with a duplicate-VM
  error and preserved the existing destination's UUID, config, zvol GUID, exact
  disk hash and volume inventory. An earlier fixture incorrectly assumed this
  must fail synchronously; its cleanup-interrupted result is not counted as a
  clean negative transfer test.
- Deleting that owned collision fixture also deletes its per-VM ACL. The next
  positive experiment explicitly restores the same exact now-empty VMID's grant;
  it does not broaden permissions to other VMs.

## Clean selected real result

Evidence is in `evidence/remote-migrate-spike-20261110T211112-8d1502/`.
`summary.json` reports the selected run complete, while `qualification.json`
separately records the conditional conclusion and every untested boundary.
The actual data transfer completed with task `OK` in 25 seconds. The complete
two-node setup, probes, transfer, checks and shutdown took 805.9 seconds under
ARM-to-x86 software emulation; this is not a hardware performance benchmark.

- Source and destination were independent single-node PVE installations with
  distinct node names, machine IDs, SSH keys, cluster CA/certificates and root
  cluster SSH keypairs. Neither joined a cluster or contacted production.
- The 64 MiB root and 4 MiB generated cloud-init disk copied byte-for-byte.
  SMBIOS UUID, vmgenid and generated cloud-init metadata hash were preserved.
- Both source and destination roots retained 70,254,592 bytes of refreservation;
  both cloud-init disks retained 6,291,456 bytes. Both are thick. Storage and
  bridge mappings were applied: `src-alloc` to `dst-alloc`, `vmbr77` to `vmbr78`.
- The pre-existing target conflict ended in an authoritative failed source task.
  Its 32 MiB disk hash, zvol GUID, VM identities, config hash and volume inventory
  stayed unchanged. The owned fixture was deleted only after this comparison.
- Repeating the completed migration was refused, with destination identity and
  bytes unchanged. This is **not** a resumable/idempotent success API returning
  the original receipt; the outer controller still needs durable operation IDs,
  native task receipts and explicit reconciliation.
- The retained source stayed stopped with `lock=migrate`. An ordinary `qm start`
  was refused with that lock. The target QEMU process started successfully and
  was stopped after the probe. This proves target runtime startability, not OS
  boot or SSH readiness: the test root contains random bytes, not a guest OS.
- The post-copy ZFS inventory contained exactly the root and cloud-init zvols on
  each host, with no temporary migration snapshots or extra target disks. The
  source's separate scratch zvol and held snapshot belong to the quota probe.
- Both temporary API tokens were removed and both owned QEMU children shut down.
  Exported evidence passed a scan for every target-token value known to the Mac.
  Private keys, tokens, seed ISOs and virtual disks remain outside exported
  evidence, under the private `.lab` directory.

The source migrate lock is a useful physical building block, but its behavior
through an actual source reboot and through every failure point is **not tested**
by this run. A successful migration does not establish that every interrupted
handoff is safe or that a restored old state cannot permit two runnable copies.

## Data-plane implications for the real hosts

The actual source task selected WebSocket tunnel version 2. It forwarded both
storage streams through additional WebSockets for `/run/pve/9001.storage`.
During copy, all observed private-peer TCP connections targeted destination
port 8006; no separate inter-host disk-stream port was observed in this stopped
case. This agrees with the inspected Proxmox `fork_websocket_tunnel` path.

Your actual PVE APIs listen on loopback and must remain private. A job-scoped
controller/iroh tunnel carrying this API/WebSocket traffic to destination
127.0.0.1:8006 therefore looks feasible **as an inference**, but this spike did
not exercise that tunnel or bind the guest API itself to loopback. It opened no
public management port. A single raw TLS relay preserves the destination PVE
certificate pin but still requires the source to hold a PVE credential. Keeping
the broader PVE token entirely behind a target-side job proxy would require a
separately reviewed authenticated proxy/capability protocol, not just raw TCP
forwarding. Neither option is implemented here.

## Separate actual-ZFS policy evidence

On a fixed scratch dataset, separate from VM storage, OpenZFS 2.4.4-pve1 accepted
quota-first/reservation-second increases and reservation-first/quota-second
decreases. It rejected a quota below the existing reservation and a quota below
actual thick/snapshot use, preserving the dataset GUID. A 32 MiB thick zvol
charged 36,175,872 bytes of refreservation; an overwritten snapshot retained
8,495,104 bytes. Parent usage included those commitments plus 98,304 bytes of
parent metadata. Adding snapshot bytes to parent `used` again would double count.

On this installed version, explicit `quota=0` was refused with "use 'none' to
disable quota/refquota". Explicit `quota=none` succeeded and read back as numeric
0; both probes restored 96 MiB afterward. Numeric readback 0 is not an allocation
of zero bytes. These primitive property tests do not qualify the complete Rust
quota-editing state machine against real Proxmox.

## Imported guest dependencies remain a separate gate

The toy uses generated cloud-init. Heidi's actual VM uses a pinned `cicustom`
metadata snippet. Proxmox's own documentation requires custom files to be
available on every destination or the VM cannot start. A production migration
must explicitly stage and hash those dependencies and preserve the original
instance identity. Generated cloud-init copying is not evidence that local
snippets, hooks, ISO references or imported guest SSH identity are preserved.
See [the official custom cloud-init documentation](https://github.com/proxmox/pve-docs/blob/master/qm-cloud-init.adoc#custom-cloud-init-configuration).

## Remaining qualification gates

Before using this backend for automatic real-host moves, test mid-copy failure,
lost successful acknowledgement, expiry/revocation during transfer, source
reboot fencing, exact cleanup after uncertain import, bootable guest/SSH identity,
custom snippet staging and the real loopback-API/iroh route. Add the durable
two-controller handoff and job-scoped authorization before exposing a transfer
button. Live migration, large production disk sizes, CPU portability and disk
shrinking remain separate work. No existing real VM has been adopted into the
controller by this spike.

## Original combined qualification attempts (incomplete)

The original combined `lab/remote_migrate_qualification.py` fixture used
fresh independent overlays from the same sealed base, new host identities, two
8 GiB virtual mirror members per host, the exclusive shared lab lock, the same
10 GiB physical free-space floor and owned-PID cleanup. The historical runs and
sealed image remain untouched. Management SSH forwards and the QEMU private
Ethernet socket bind only to Mac loopback. The host private addresses are
172.31.77.1/30 and .2/30; isolated guest bridges use 10.77.0.1/24 without NAT or
forwarding. No production addresses or credentials are accepted.
The source uses single-thread TCG from its initial boot for the host reboot
gate, following the stable fixture used by the older reboot matrix; the target
uses multi-thread TCG and does not reboot. There are no performance claims.

The destination API is explicitly bound and checked on 127.0.0.1:8006. A fresh
no-shell SSH account accepts only one temporary public key and local forwarding
to that fixed address/port. The source's 127.0.0.1:18006 forward pins the target
SSH host key and verifies its PVE TLS fingerprint before any credential is sent.
The source's direct private-address HTTPS connection must fail. This is an SSH
tunnel qualification; an iroh tunnel remains unimplemented. The API token still
exists on the source, with the broader metadata permissions described above.
The custom-snippet fixture also grants `Datastore.Audit` and
`Datastore.AllocateSpace` on its fresh dedicated snippet storage. The latter is
the snippet-access permission used by the inspected
[official PVE storage implementation](https://github.com/proxmox/pve-storage/blob/master/src/PVE/Storage.pm).
It is not a read-only per-file capability. The generated-CI physical trial does
not need this additional snippet-storage grant.

First, VM 9001 receives a rate-limited real copy. After the private SSH
connection acknowledges more than 2 MiB and the source migration log shows a
copy phase, the owned SSH forward is killed. Target dataset visibility may lag
an in-progress ZFS receive, so visible allocation alone is not the byte-transfer
criterion.
Both source fixtures set `protection=1` and `onboot=0` before any copy. Native
`remote_migrate` keeps `delete=0`; source protection is never relaxed, and the
bootable target must preserve protection and disabled autostart. An immediate
protection-related refusal is captured by the small fixture before OS boot.
The test records the accepted source UPID before failure, waits for its native
terminal result, and captures both VM/config/zvol inventories. It never treats a
failed client connection as absence of a native task or automatically repeats
that transfer. If native cleanup retains a target config, its ordinary start
path must refuse to run. The follow-up bootable guest has separate VMID 9002.

The bootable guest is a lab derivative of the checksum-verified Debian parent,
using the existing exact boot-policy recipe. Its root is grown to 4 GiB and must
remain thick at the destination, including its cloud-init zvol. The same custom
metadata snippet is staged and hash-checked on both hosts before migration. A
real source guest boot establishes its SSH key, machine ID, cloud-init instance
ID and a sentinel file. The pristine prepared image is exported privately and
hash-verified before any guest imports or boots it; post-boot guest disks are
never exported as an image. The stopped copy deliberately discards its client-side
submission acknowledgement, retrieves a persisted lab source receipt and polls
that same native UPID without native resubmission. An fsynced exclusive intent
marker also refuses resubmission if native acceptance occurred before the UPID
could be persisted. This tests a persisted-receipt
lost-ack boundary; acceptance before receipt persistence remains an explicit
ambiguity window requiring production reconciliation design.

After the target boots and presents the pinned guest SSH key and unchanged
sentinel/instance/machine identities, the source PVE host reboots. Its boot ID
must change while host identity and retained VM configuration survive. Ordinary
`qm start` must still refuse the retained source's migration lock while the
target remains active. This tests the ordinary PVE start path, excluding root
unlock/override. Tokens and the temporary tunnel account are removed before the
owned emulator children shut down. A partial run reports its exact failed gate;
this plan and local harness regression tests do not themselves establish that
these physical gates passed.

Two actual attempts remain explicitly incomplete. In
`remote-migrate-qualification-20261110T231445-77b5a2`, the real protected
interrupted-copy phase passed before a Python API-route/property argument
collision prevented snippet storage creation. The helper argument was renamed
and a regression added. In
`remote-migrate-qualification-20261110T232843-1fae6e`, the same interruption
phase passed again, the pristine image was exported before guest import, and
the nested source guest then reached an initramfs emergency shell because its
root device appeared too late under single-thread outer TCG. Serial evidence
failed the gate immediately; there was no console input or boot retry. This is
the same fixture limitation documented in `docs/bootable-guests.md`. Neither
whole run is a successful bootable-migration qualification.

The interrupted copy had an authoritative terminal source UPID after more than
2 MiB of real acknowledged SSH transport data. Both VMs stayed stopped; ordinary
target start was refused. Native abort left the source's `@__migration__`
snapshot and a target placeholder locked `create` with its CI volume. Thus
interruption does not automatically remove all staging or release every
reservation. Both temporary API tokens and the SSH account were removed and
the owned outer emulators were reaped. That completed phase may be referenced
as an independent interruption gate; its parent report stays `complete=false`.

## Independent qualification profiles

The current harness separates the conflicting emulation requirements without
changing the prepared image or extending/masking boot errors:

```sh
nix develop ./nix --command python3 lab/remote_migrate_qualification.py --mode fence --timeout 1800
nix develop ./nix --command python3 lab/remote_migrate_qualification.py --mode guest --timeout 5400 --image-sha256 48bb8767a55e4909fe74a3bfc3f8a2d6fd859f12824c8b1a2b3d1393621451c1
```

The `fence` profile uses a single-thread source from initial boot, a protected
64 MiB stopped VM and a successful native cold copy. The target QEMU process
stays active while the source PVE host genuinely reboots. The new source boot
ID, stable machine identity, persisted migration lock/protection, ordinary-start
refusal and active target state must all be observed. It makes no guest OS boot
claim. Target QEMU is then stopped and both hosts' exact volume inventories and
authority/process cleanup are checked.

The `guest` profile uses multithreaded outer TCG on both hosts and makes no host
reboot claim. It reuses the exact pristine image from its verified private cache,
stages identical custom metadata snippets, boots the source, records SSH/file/
machine/instance identity, shuts it down cleanly and migrates protected VM 9002.
The lost-ack drill reconciles the persisted native receipt without submitting a
second native job. Exact cold bytes, UUIDs, protection, autostart and thick
root/CI backing are checked before booting the target with the pinned SSH host
key and unchanged guest identities. All owned authority and processes must be
cleaned up. The image is not qualified merely because it was prepared/exported.

`lab/remote_migrate_gate_matrix.py` combines these explicit independent reports
only after every required gate and its cleanup passes. It records report paths,
file/program SHA256 hashes, emulation modes and
`all_gates_same_vm_and_run: false`. It never changes the failed parent reports
into successful runs and never claims that one bootable VM exercised the entire
handoff through host reboot. Even a complete composite only supports a
supervised native-PVE disposable physical trial; Rust migration RPCs, durable
peer handoff and an iroh API tunnel remain unimplemented.

Current independent gate status:

| Gate | Actual result | Evidence |
| --- | --- | --- |
| Private API and interrupted transfer | Phase passed and authority/process cleanup verified; whole parent run remains failed at nested guest boot | `evidence/remote-migrate-qualification-20261110T232843-1fae6e/` |
| Protected cold copy and source reboot fence | Passed in 856.7 seconds; both owned outer QEMU exits were 0 | `evidence/remote-migrate-qualification-20261111T000246-d88d12/` |
| Healthy bootable source and current user-data schema | Passed source cloud-init exit 0/SSH/identity with the unchanged pristine image | `evidence/remote-migrate-qualification-20261111T014136-e3cfa0/` (whole run failed later) |
| Successful bootable copy and target identity | Not qualified: first CI import child missed native readiness deadline | Same failed guest report |
| Composite qualification | Pending all required gates | No complete composite yet |
| Physical preparation | Validated partial preparation; explicitly `complete=false` and `bootable_transfer_qualified=false` | `evidence/remote-migrate-preparation-20261111T022718/summary.json` |

The first independent guest attempt,
`remote-migrate-qualification-20261111T003759-408e03`, passed initramfs and owner
SSH but strict readiness rejected cloud-init exit 2 (`degraded done`). Its only
recoverable error was the deprecated scalar `user` key. The lab fixture had set
`ciuser=dev`; PVE emits `user: dev` when that override is present. The actual
Rust guest configuration leaves `ciuser` unset and already relies on this
pristine image's configured default user `dev`. The fixture now matches that
path, and checks the actual PVE-generated user-data for a default `users` list
and absence of scalar `user` before boot. The image bytes and strict exit-0,
non-degraded readiness requirement are unchanged. This failed attempt remains
incomplete and is not used as passing guest evidence.

The corrected independent guest attempt,
`remote-migrate-qualification-20261111T014136-e3cfa0`, verified the actual PVE
default-users text/hash before boot. The source then passed owner SSH and strict
cloud-init exit 0, `done`, with no recoverable errors; its machine/SSH/file/
instance identity was recorded, it shut down cleanly, and its protected stopped
4 GiB thick baseline was inspected. The image stayed exactly
`48bb8767a55e4909fe74a3bfc3f8a2d6fd859f12824c8b1a2b3d1393621451c1`.

Its subsequent native task failed on the **first 4 MiB CI disk import**, before
root data transfer. The target import child did not produce its readiness line
within PVE's internal five-second deadline. The accepted source UPID reached
authoritative `migration aborted` before credentials/processes were cleaned up.
The whole report remains failed; it is not successful copy, lost-successful-ack
or target-OS evidence. No PVE deadline or readiness policy was patched.

The fixed installed source was inspected read-only through the already pinned
`pve2-host` alias, without any guest/service/file mutation. Package versions
matched the lab: storage 9.1.15, guest-common 6.0.7 and qemu-server 9.2.15. File
hashes and installed package checksums are recorded separately in
`evidence/remote-migrate-native-timeout-diagnosis-20261111/summary.json`.
`Storage::volume_import_start` starts a fresh `pvesm import` and waits five
seconds for `ready`. The child prints and flushes that line after listening on
its UNIX socket, before accepting the client or receiving volume data. No child
stderr was appended to the captured failure. Emulated startup/scheduling delay
is plausible, but its exact cause is unproven; this is not evidence that a large
root disk or `cicustom` is unsupported.

`lab/remote_migrate_preparation.py` assembles only the completed independent
interruption/fence/healthy-source proofs and the precise terminal native
timeout. Its `pve-native-physical-preparation/1` schema revalidates every linked
report and cleanup, stores all file/program hashes, and keeps `complete=false`,
`preparation_complete=true`, `bootable_transfer_qualified=false` and
`all_gates_same_vm_and_run=false`. The successful bootable copy, target machine/
SSH/file/instance identity and completed-copy lost-ack checks remain mandatory
in the separately authorized, bounded fresh-VM physical trial with unmodified
PVE. This partial preparation does not relax the existing full composite gate
or qualify real-user VM migration.

The passed fence run used Proxmox 9.2.26. Its 64 MiB root and CI volume copied
with matching bytes/VM UUIDs and thick backing; source and target preserved
`protection=1` and `onboot=0`. After target QEMU started, the source host completed
a genuine reboot with a different boot ID and unchanged machine identity. Its
retained config still had the migration lock and ordinary `qm start` was refused
while the target was active. Final inventories contained exactly each VM's root
and CI zvol, with no migration snapshots or extra volumes. Both API tokens and
the temporary SSH account were removed. This is hypervisor-level fencing
evidence using a nonbootable test disk; it makes no guest OS or SSH claim.
