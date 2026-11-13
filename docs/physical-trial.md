# Bounded physical-host trial

Status: **the supervised two-host hardware trial completed, including cleanup.**
It required an explicit metadata-only repair; unattended reruns remain disabled.
This is a supervised test
of fresh resources, not adoption of existing Hetzner guests or a general Rust
migration API. The existing GUI observation profiles remain read-only.

## Boundaries

The helper has a separate `physical_trial` backend. Its root-owned configuration
pins the machine ID digest, node, ZFS pool GUID, a dedicated parent dataset GUID,
environment marker, storage prefix, two account names and at most four explicit
VM IDs. Controller and helper persist and attest the complete binding before a
job can be submitted. Physical state uses schema4, so older binaries cannot drop
the scope metadata and continue. Legacy lab/simulated bytes remain compatible.

The hardware harness accepts only the two existing pinned SSH aliases. It uses
`trial-admin` and `trial-user` in independent new controller environments. Each
host gets a fresh `rpool/pve-dev-trial-ENV` parent capped/reserved at 12 GiB;
the managed control dataset is its child. No old user, storage allocation or VM
is imported. The server still uses small positive trial policies rather than
production-sized allocations.

Before setup it refuses occupied VM IDs 9001/9002, their pre-existing ACL/HA/
firewall configuration, existing trial storage names, and existing private bridge
or subnet configuration. The test creates only an isolated `vmbr77` with no
uplink/NAT. Existing PVE APIs continue listening on loopback. No public management
port or firewall allowance is added.

The network daemon runs under a fresh Linux service account; the helper remains
privileged. Test client keys stay root-private and are not readable by the daemon.
Separate iroh keys test administrator/member API authority; this does not test a
human member's Linux-login isolation. CLI requests travel over pinned SSH and
then host-local loopback iroh, not a public iroh relay or direct Mac-to-server UDP
path. The dedicated Linux account is an implementation detail of the temporary
services, separate from the controller's member account.

## Required qualification

The completed run used `deployment/physical_trial.py --execute` with reviewed
one-off continuations. Its readiness switch is disabled again because routine
cloud-init metadata reconciliation remains unfinished. Its evidence gate requires a complete scoped
Proxmox report covers real quota growth/contraction, below-commitment refusal,
effect-present helper restart, read-only empty-filesystem inspection and verified
empty-account deletion. All three Linux binary hashes must equal those actually
qualified. The native report must cover private API forwarding, interrupted-task
reconciliation, source reboot fencing, and a healthy source guest boot on the
exact pristine image. A full bootable-copy matrix can satisfy this requirement.
Alternatively, `pve-native-physical-preparation/1` explicitly keeps `complete=false`
and `bootable_transfer_qualified=false` while proving `preparation_complete=true`;
the authorized disposable hardware trial then performs the remaining copy,
target-boot and identity qualification. These paths cannot silently substitute a
failed full matrix for success. Before any SSH contact, the driver reconstructs
the selected contract from detailed native task, identity, reboot and cleanup
records and compares every evidence-file digest.
The scoped report must also prove emulator shutdown, unchanged external
resources/lab identity and refusal/reopen behavior for the immutable scope.

Interrupted attempts remain incomplete evidence. A marker-wait timeout is not a
successful crash injection. Mocked tests do not establish real ZFS or migration
behavior. The exact portable artifacts and worker programs are retained privately
for attribution; only narrow result allowlists are exported.

The complete scoped run passed on November 11 against disposable Proxmox VE
9.2.26 and a real ZFS mirror on fresh virtual disks, in 3,773.5 seconds. Its
report proves all
five required behavior gates, changed-scope refusal/reopen, exact preservation of
unmanaged resources and global configuration, and owned emulator shutdown. All
three executed portable binaries match the final v3 build manifest, whose source
manifest digest is `b9bdcca5eb5a45429ec0b67c8bdaddf633f06e3080e4f19e8192671a9972ce7f`.
It used blank guest disks and host-local iroh; it makes no guest-OS or public
relay/NAT claim. The paired runner's guest component failed independently on a
fixture-only cloud-init setting; its parent run remains incomplete, while the
scoped report is complete. Both process groups were retired without forced
descendant cleanup before the corrected guest-only retry started.

That corrected retry booted the unchanged image with clean cloud-init (exit0,
status and extended status `done`) and verified source SSH/identity/data. Native
migration then aborted at the first 4MiB cloud-init disk import: its child did
not report readiness within Proxmox's internal deadline. The
attempt remains incomplete.
No PVE timeout or package is patched. The separate preparation contract permits
finishing this remaining gate inside the already authorized fresh-resource
hardware trial, with every namespace, provenance, fencing, credential and cleanup
guard retained. Failure keeps the controllers paused and the trial evidence;
partial imports without verified GUID proofs require supervised reconciliation.

## Supervised sequence

1. Provision the two test members, exercise 6144 → 7168 → 6144 MiB policies and
   prove the member cannot administer accounts.
2. Create a fresh 4 GiB Debian guest owned by the source member. Refuse a 4096 MiB
   quota and account deletion while that VM exists. Boot it, check cloud-init/SSH,
   write and flush a unique sentinel, then stop it.
3. Pause both controllers. The supervised native adapter captures helper-created
   source UUID/disk GUID proofs and the destination's exact empty allocation.
   Fresh privilege-separated PVE tokens authorize the exact test VM/storage/
   bridge plus destination incoming/audit access. Two owned SSH forwards carry
   the destination loopback API through the Mac, preserving its TLS fingerprint.
4. Submit one stopped native migration with an exclusive durable intent and
   receipt. Never resubmit an uncertain acceptance. Verify root/cloud-init bytes,
   guest identities and thick backing; prove the retained source's ordinary
   start is refused before starting the destination. Verify guest SSH and data.
5. Stop and remove only the proven imported destination, await its task and exact
   disk absence, then release the source lock. Revoke all temporary tokens and
   owned grants/roles and stop the owned forwarding processes. Only then resume
   the controllers. This transfer is not adopted into the destination catalog.
6. Explicitly delete the original source VM through Rust, then delete both empty
   member accounts through their revision-checked API. Check device revocation,
   storage absence and retained tombstones.
7. Stop admission, inspect the stopped controller ledger, prove the helper has no
   unsettled work, and stop it. Fresh topology/reference/task checks and read-only
   filesystem inspections authorize only nonrecursive removal of the remaining
   proven empty admin/control/parent scaffolds. Verify native authentication is
   gone before erasing its plans. Remove the trial services, private files, image
   copy, bridge and Linux service identity; compare existing guest configurations.

## Limits and recovery

The final physical result
records successful real quota growth/contraction on both hosts, below-commitment
and account-with-VM refusals, a booted fresh source, a byte-exact protected stopped
copy to SSD, retained-source start refusal, target guest health and preservation
of SSH/machine/cloud-init instance/sentinel identity. The target and both imported
volumes were removed before source release. The original source VM was then
explicitly deleted through Rust; both empty members were deleted and their
devices refused. Offline ledgers, native auth retirement and guarded teardown
completed on both hosts. Existing guests were never adopted.

Proxmox added the accurate `size=4M` annotation to the cloud-init attachment. The
initial guard refused that difference. A narrow comparison accepts only this
addition with a proven 4MiB volume and all other configuration/identity checks
unchanged. Proxmox's normal config setter retained the annotation, while frozen
Rust helper history requires the original text. A supervised repair therefore
loaded the complete source configuration under Proxmox's config lock, checked
its digest/UUID/fence and every field, changed only that annotation, preserved
pending/snapshot/special sections, and re-proved both disks' identities and full
hashes. It never unlocked, resized, detached or deleted a disk. Existing release
guards then independently verified the repaired state and target absence.

Independent after-state checks matched every existing guest state/configuration,
ZFS identity/reservation, permission and network/API baseline on both hosts.
All temporary remote resources were absent. Original storage settings matched
the full read-only reference after normalizing capability ordering and excluding
the API's global digest; raw whole-file storage digests did change. The final
SSD pool had 952 GiB total, 40.87 GiB physically allocated and 65.49 GiB globally
unreserved. Heidi's unchanged 825 GiB allocation and the host's 32 GiB reserve
account for most of the difference; mirror capacity is not the sum of both disks.

The initial run and first continuation remain incomplete evidence. The final
continuation is separately attributed and complete. No migration was resubmitted,
no PVE timeout/package was patched, and no host reboot or persistent network
change was made. This validates the supervised workflow; it does not make the
current automatic driver or a general CLI/GUI migration API ready. Normalization
of semantically equivalent cloud-init metadata needs a reviewed routine path.

The native copy is a trusted root-supervised two-host drill. Its cross-host
receipts are not a signed capability protocol, and neither GUI nor Rust exposes
a migration RPC. No Raft/shared database or permanent peer administrator is added.
The successful bootable lost-ack variant and a reboot test using a bootable target
remain unrun; the independent reboot/fence gate used a protected blank guest.

A failed Proxmox migration can leave an unlocked source with retained migration
snapshots and a locked, incomplete destination. A stopped copy is not necessarily
clean or independently fenced. The harness retains evidence and leaves controllers
paused on uncertainty; it does not automatically retry, destroy staging or fail
back. Full automation still needs its own durable authority/fencing and audited
reconciliation implementation. Custom snippets are tested separately in the lab;
the hardware trial uses generated cloud-init.

The safety checks assume one trusted driver and no concurrent external root
administrator modifying configurations between a check and its effect. Normal PVE
VM deletion frees only preflighted test disks; its internal storage implementation
is not replaced by private Proxmox config-file manipulation. Interrupted setup or
cleanup can require manual investigation. There is no general force/reset path.
