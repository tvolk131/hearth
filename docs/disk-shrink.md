# Offline disk shrink (disposable lab only)

The Rust controller, privileged helper, CLI and Swift app implement a two-stage
shrink for controller-created image guests. The selected real-Proxmox workflow
and copy-interruption test passed; see the qualification report.
Existing physical VMs cannot be adopted or resized by this adapter.
This qualification did not modify Heidi's VM or either Hetzner host. A later, separately authorized [November 13 one-off](persistent-management.md#heidi-ssd-shrink-november-13) shrank Heidi's SSD VM to 400 GiB; it does not expand this lab adapter's supported layouts or enable managed resize RPCs.

## Supported layout and capacity

The first adapter accepts only an independently thick-backed ZFS root zvol,
GPT with 512-byte sectors, one ext4 filesystem with 4 KiB blocks, and that root
partition last by extent. Other partitions may be BIOS boot or EFI/vfat. VM/ZFS
snapshots, clones, LVM, LUKS, XFS, Windows, extra VM devices and foreign current or
pending root references are refused. The helper verifies its normal lab hostname,
environment file, pool GUID/marker, controller creation history, owner policy,
full VM configuration, volume identities and actual stopped state.

Disks with another ext4 partition, such as a separate ext4 `/boot`, are also
refused by this first adapter. Supporting those layouts needs its own selection
and preservation checks and qualification; the current boot fixture is Debian's
single-root-filesystem cloud layout.

Targets are at least 2048 MiB and smaller than the current admitted disk, with
the existing 8192 MiB image-guest cap. Filesystem headroom is checked offline;
the controller's plan is a capacity estimate, not a filesystem-fit certificate.

Each pending shrink reserves the source-sized work volume plus target-sized
volume, 5% overhead for each, and 64 MiB for workspace metadata. This independent
host claim is added to all user budgets. The helper also checks real globally
unreserved capacity plus its 512 MiB safety floor, and creates a quota/reservation
on a fresh operation-owned workspace outside the user's allocation. This is
deliberately conservative and may refuse a shrink even when guest data is small.
The present SSD host lacks this much duplicate thick capacity for Heidi; this
lab adapter is not a solution to that production workspace constraint.

## Workflow

1. Use **Shut Down**, distinct from forced **Stop**, and await verification. The
   helper submits Proxmox's graceful shutdown task with a 600-second timeout and
   no forced fallback. The shrink independently requires a clean unmounted root.
2. An administrator submits the expected current disk size and a smaller target,
   explicitly acknowledging downtime. An atomic controller intent freezes owner
   creates, deletion and policy edits and retains the workspace claim.
3. The helper validates the actual filesystem/layout and available space, fences
   the VM with `lock=migrate`, marks its original root read-only, and makes an
   independent full copy into its new reserved workspace. Full byte comparison
   precedes any filesystem changes. Nothing truncates the original disk.
4. On the replacement only, it checks ext4, shrinks the filesystem, shortens the
   root partition, hides and shrinks the zvol, relocates GPT backup structures,
   verifies both header/entry CRCs, fills the new root partition and checks ext4
   again. Filesystem/partition UUIDs, disk GUID, other partition metadata and BIOS
   bootstrap bytes are preserved; the original's full hash must remain equal.
5. With the VM still fenced, it retains the original in the workspace and moves
   the replacement to the canonical root name. The only VM configuration change
   is the root size annotation. Rust independently re-observes the new root's
   GUID/creation, thick backing, attachment and image provenance, preserves the
   original VM/cloud-init identities, and durably commits the new helper profile
   before releasing the fence. The VM stays stopped.
6. **Preparation is verified; the overall workflow still awaits guest review.**
   The dashboard exposes the retained recovery hold. Normal start, graceful
   shutdown and stop remain available for this target; other owner mutations
   remain frozen. Boot the guest and verify its data and services.
7. Shut down cleanly again and explicitly finish, acknowledging both guest
   verification and permanent recovery-copy deletion. This is an administrator
   attestation, not an automatic guest-health claim. The helper checks the actual
   target, original GUID/full hash, workspace identity, references, clean ext4
   and read-only-proven empty workspace filesystem. It removes only the exact
   original volume and then the childless, empty workspace, nonrecursively.
   Capacity is released only after verified removal.
8. The user's quota/reservation remains unchanged. An ordinary revision-checked
   policy contraction can now release its unused portion for another user.

## CLI and GUI

```sh
pve-devctl --profile lab.json vm shutdown --request-id clean-down --vm-id 9001
# Wait for that operation to verify.
pve-devctl --profile lab.json vm shrink-plan --vm-id 9001 --target-disk-mib 3072
pve-devctl --profile lab.json vm shrink --request-id shrink-one --vm-id 9001 \
  --expected-disk-mib 4096 --target-disk-mib 3072 --confirm-downtime
# Wait, boot the replacement, inspect its data/services, and shut it down cleanly.
pve-devctl --profile lab.json vm finish-shrink --request-id finish-one --vm-id 9001 \
  --preparation-id PREPARATION_OPERATION_ID \
  --confirm-guest-verified --confirm-delete-recovery
```

Retry a lost reply with the exact original request ID and parameters. A retry
returns the historical operation; it does not resize again. `dashboard` includes
`disk_shrinks`; `operations` distinguishes `shrink_vm` preparation from the
separate `finish_vm_shrink` operation. The GUI exposes **Shrink Disk…** for eligible
stopped lab guests and **Finish Shrink…** while recovery is retained. Its pending
ledger binds both operations to the original server identity/profile and VM.
Read-only observation routes and members cannot send these administrator actions.

## Persistence, recovery and limits

The first accepted shrink or graceful-shutdown operation raises both state
schemas to 5, so older binaries refuse the new state. Optional fields are omitted
from legacy jobs/catalogs/journals; prior frozen fingerprints are preserved.
Paired offline backups retain queued/prepared holds, operation proofs and the
simulated external world. Restores remain quarantined with workers disabled.

Every phase has durable intent before its effects. A verified phase can be
continued after restart, with later phases rechecking its actual dependencies.
An unverified intent blocks on **every** host boot; no filesystem tool, disk swap
or recovery deletion is blindly replayed. The original, fence and capacity holds
are retained wherever their effects have not been explicitly authorized and
verified. A finalizer interrupted after its authorized deletion can retain a
conservative capacity hold even when the original is already gone.

Blocked-operation reconciliation, automatic rollback, live shrink, production
adoption, thin/clone space optimizations and remote scratch storage are not
implemented. Recovery is not a hardware-independent backup, and rolling back
after writing to the smaller disk would discard those later writes. External
root administrators modifying resources concurrently remain outside the current
coordination model. Guest filesystem tools run in the trusted disposable lab;
this is not a hardened parser boundary for hostile production disk images.

## Qualification

`tests/disk_shrink.rs` covers normal lifecycle/finish/contraction, member/stale/
running/unconfirmed/capacity refusal, retained claims and lost-reply identity.
It exercises 18 preparation crash/reboot combinations plus three finalizer
boundaries. `tests/maintenance.rs` verifies queued/prepared schema5 paired restore
without activation. Python tests cover layout refusal, partition identities,
GPT CRCs/bounds and foreign references. Swift tests cover typed wire requests,
both finish acknowledgements, retry identity, server-specific pending state and
read-only/unprivileged submission refusal.

The hardware-facing fixture uses one new loopback-only Proxmox overlay, a fresh
20 GiB virtual mirror and frozen Linux binaries. It imports the pinned image,
checks its actual layout before boot, and prepares 4096 to 2560 MiB while the
image has never booted. It independently checks that ext4 actually shrank and
its disk/partition/filesystem identities stayed equal, verifies the exact
retained-original/target volume set, restarts both services, and probes the empty
workspace using the finalizer's read-only mount/unmount check. No guest health
or recovery deletion is attested during that early phase.

The full fixture then boots the 2560 MiB target and requires cloud-init, systemd,
owner-key SSH and refusal of an unrelated SSH key. It writes random sentinel
data, records the initialized guest's identities, and shuts down cleanly. Only
then does it explicitly finish the first preparation and remove its 4096 MiB
recovery original. It prepares a second shrink, 2560 to 2048 MiB, restarts both
services again, boots and compares the initialized guest identities and sentinel,
shuts down, explicitly finishes, and contracts the owner's quota/reservation
from 5120 to 3072 MiB. This checks repeated shrink history as well as preserving
data across an offline shrink of an initialized guest.

After verified deletion and an empty real VM/zvol inventory, a second fresh
image guest holds a real completed copy effect before its reply. The helper is
killed/restarted; blocked recovery, unchanged original bytes/GUID, retained fence,
identical retry and no orphan copy process are required. The intentionally
blocked fixture remains in the retired disposable overlay for investigation;
it is not reported as normal VM cleanup.

Run from `pve-dev/` with a matching frozen bundle containing a native CLI:

```sh
# Full qualification runs the short preparation check before cold guest boot.
nix develop ./nix --command python3 lab/disk_shrink_suite.py \
  --linux-bin-dir .lab/disk-shrink-binaries-20261111-v6 --timeout 7200
# To run only preparation (no guest boot or recovery deletion):
nix develop ./nix --command python3 lab/disk_shrink_suite.py \
  --linux-bin-dir .lab/disk-shrink-binaries-20261111-v6 --prepare-only --timeout 3600
```

The parent holds the existing global lab lease and pins the fixture process
group with the shared session anchor. It retires every owned local group before
reaping that anchor/releasing the lease. No public iroh/NAT qualification or
physical-host write is claimed by this test.

The current automated checks pass: 174 Rust tests, 97 Swift tests (including
the real CLI integration), 115 Python lab tests, 87 deployment tests, 10 local
controller/helper/iroh process checks, formatting and Clippy. These checks do
complement the completed real Proxmox fixture; they do not establish a formal
proof or cover every hardware failure boundary.

V3 allows valid GPT layouts
with reserved usable-space gaps while checking both tables, CRCs, disk GUIDs,
partition bounds and overlap. A prior V2 source passed boot/health checks and a
controlled V3 helper upgrade preserving the controller and guest identities,
but timed out on a read-only identity query before shrinking. A continuation
after outer-host reboot stopped at power-on because its temporary bridge was
absent. Both failed attempts were retired cleanly and their reports are retained;
neither qualifies the shrink. Their disposable disk images were later removed
under the lab lease to restore local free space. The current identity-query deadline is 600 seconds
to accommodate nested emulation, with the comparisons unchanged. The native CLI
and controller binaries are unchanged between these bundles.

The fresh V3 fixture passed source health, sentinel creation and graceful
shutdown, then refused the shrink before fencing/copying because its GPT type
allowlist omitted the pinned Debian image's Linux x86-64 root GUID. V4 admits
that specific type while retaining the ext4/4 KiB/last-root requirements. It
also derives the actual GPT partition number from the zvol partition node,
rather than assuming populated GPT slots are consecutive. Both have regression
coverage. Unknown types still fail closed and are included in the error.

Every shrink fixture now probes its actual imported image using the same frozen
adapter's read-only layout checks before booting any nested guest. The new
preparation-only fixture reduces 4096 to 2560 MiB, independently checks that the
ext4 filesystem shrank and its identities stayed equal, verifies exact backing
and recovery volumes, and restarts both services. Its report explicitly excludes
guest boot and recovery deletion. It must not be used as full qualification.

V4's preparation-only run then found an installed-ZFS compatibility issue before
copying any guest bytes: explicit `refreservation=auto` on `zfs create -V` returned
`ENOSPC` despite sufficient pool/workspace capacity. A separate fresh mirror
reproduced failure for an explicit-auto 16 MiB volume while normal thick creation
of a 4 GiB volume succeeded inside the same quota/reservation. Shrinking that
empty test volume to 2560 MiB retained correct thick backing, including an
explicit `zfs set refreservation=auto`; every diagnostic dataset was removed.
The fixed-version [OpenZFS 2.4.4 source](https://raw.githubusercontent.com/openzfs/zfs/zfs-2.4.4/lib/libzfs/libzfs_dataset.c)
resolves the auto sentinel for setting/cloning, but lacks the later create-path
fix. V5 uses normal thick creation and independently checks actual reservation,
identity and ownership before copying; it does not use sparse volumes or loosen
the workspace formula. V5 completed the real copy and reduced the replacement to 2560 MiB, then safely blocked before swap because the GPT validator assumed fixed entry-array locations. V6 accepts entry arrays within the reserved regions while checking their CRCs, matching contents, header identities and partition bounds. V6 passed the full fresh qualification run described above.


The final V6 run completed in 12,129.9 seconds under nested CPU emulation on
Proxmox 9.2.26 / OpenZFS 2.4.4-pve1. Both 4096→2560 and initialized
2560→2048 MiB cycles passed, including service restarts, independent guest
health/data checks, graceful shutdowns and explicit recovery deletion. Real
quota/reservation contraction to 3072 MiB and normal VM deletion left no orphan
volumes. A separate fresh VM then passed completed-copy/lost-ack helper-SIGKILL
recovery: no replay, unchanged original bytes/GUID, retained start fence, stable
retry identity and reaped copy process. All owned local process groups retired
without forced cleanup. The blocked fault fixture is intentionally retained in
its retired overlay; no real server was contacted.

This qualifies those selected lab workflows only. Physical-host execution,
existing-VM adoption, Heidi's separate-boot layout and required workspace,
generic blocked reconciliation, public iroh NAT/relay and untested real crash
boundaries remain outside the implemented/qualified scope. The Swift app was
built and tested; no new native visual check was performed for this feature.
