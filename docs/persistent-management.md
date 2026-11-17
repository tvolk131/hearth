# Persistent management

The `managed` backend runs an unprivileged `hearthd` and a separate root `hearth-host` helper on each independent Proxmox host. The macOS app connects directly through authenticated iroh RPC. Client profiles pin the server identity, environment and administrator device identity. There is no shared database or consensus group between hosts.

The November 12 deployment uses a separate application, settings suite and request ledger:

```sh
open "$HOME/Applications/PVE Dev Management.app"
```

Stable profiles and the private administrator device identity live under `~/Library/Application Support/PveDevApp-Managed`. They are not bundled into the app. Keep that directory private; its paired-state backups also contain server identities.

## Supported operations

Administrators can create allowlisted member accounts, edit positive resource budgets using revision checks, delete verified-empty accounts, and create bootable VMs for themselves or members. New VMs support start, graceful shutdown, forced stop and explicit deletion. Empty-account deletion never cascades into VM deletion. Quota contraction releases only unused allocation and does not shrink a VM disk. Disk charging includes cloud-init and conservative storage overhead.

The initial root-approved usernames are `erin`, `ivan` and the disposable qualification owner `setup-check`. Member ownership does not grant controller credentials in this deployment; mutations are administrator-only. Additional usernames, image sources and host capacity require an explicit root configuration update rather than a client-side override.

The image picker obtains a strict catalog from the selected authenticated controller. Initially it offers Ubuntu 24.04 LTS, default SSH user `ubuntu`, minimum disk 4096 MiB. The source image is pinned by SHA-256 and installed in the root-owned content-addressed cache. GUI creation requires a canonical Ed25519 public key; passwords and private keys are not requested. Creation finishes with a stopped VM; request Start separately.

The deployed namespaces reserve new VM IDs 9201–9216 on each host. IDs remain retired after deletion. Their guest addresses are pinned to 10.50.1.101–116 on the existing private bridge. Identical VM IDs on different hosts represent different resources. The managed storage ceilings are 256 GiB on HDD and, following the November 13 shrink, 440 GiB on SSD, separate from existing guests' allocations. Heidi's SSD VM has its own 425 GiB fixed allocation for a 400 GiB disk. These are reservations within a mirrored pool, not the sum of both physical drives.

The administrator initially holds 64 GiB on HDD and 12 GiB on SSD. After qualification-account cleanup, HDD has 192 GiB available for additional managed account allocations. SSD now accommodates Ivan's planned 425 GiB allocation alongside Erin's 12 GiB provisioning allocation and 3 GiB of namespace overhead. The SSD limits allow a 400 GiB new disk and a 425 GiB user budget. A 400 GiB disk plus conservative charging and cloud-init fits that budget. The SSD provisioning namespace now admits up to 32 GiB total maximum VM RAM and eight total vCPUs, with 16 GiB of aggregate minimum-memory budget. Individual user/VM ceilings permit 12 GiB minimum RAM, 32 GiB maximum RAM and eight cores. These namespace ceilings are separate from the existing adopted Heidi VM. Ivan was subsequently provisioned through the authenticated CLI: account `ivan` has a 435200 MiB disk budget and VM 9202 `dev-ivan` has a 409600 MiB disk. His restricted SSH route is documented below. VM ID 9201 is retired on both hosts following qualification; the next available ID after Ivan is 9203.

The Virtual Machines page combines both hosts in one table, with a Server column and an optional server filter. New VM explicitly selects its destination host. Every lifecycle review remains bound to that server, environment, device and VM ID; VM 103 on HDD is distinct from VM 103 on SSD. Refresh remains at the right end of the toolbar. Calls sharing a device identity are queued within the app so concurrent short-lived iroh endpoints do not compete for a relay route. Cancelling a queued call prevents its launch; no mutation is automatically resubmitted.

The persistent backend does not yet enable disk resizing, VM migration or member credentials. Those features have separate lab/scoped qualification records and cannot be enabled merely by sending their RPCs to a managed controller. Existing and newly provisioned VMs now share the enabled lifecycle operations.

## Existing VMs

The four original VMs are now first-class managed records owned by `erin`: HDD Erin101, Frank102 and Heidi103's stopped recovery copy; SSD Heidi103's active VM. This is control-plane ownership only: existing guest accounts, coworker SSH access, configurations, power states and disks are unchanged. Historical import records remain internal audit provenance and are no longer exposed as a separate read-only VM list.

Adoption requires an explicit root binding for each existing profile, complete Proxmox configuration, storage configuration, allocation/disk/cloud-init dataset identities and properties, and hashes of referenced cloud-init snippet files. It refuses pending configuration or resource drift. Existing lifecycle operations use the normal durable intents, task receipts, verification and recovery paths. Deletion refuses retained snapshots, bookmarks or unexpected resources before stopping a guest; this protects the HDD recovery copy's retained snapshots.

Existing allocations remain physically separate from the new provisioning namespace. The Users page counts every owned VM and totals all allocations: Erin currently has 3 VMs and 4672 GiB on HDD, and 1 VM and 437 GiB on SSD. Those totals include existing fixed allocations plus 64/12 GiB provisioning allocations. Create admission and ordinary quota edits continue to use only the new allocation. **Edit Allocation** identifies that allocation; changing an existing allocation requires a reviewed root configuration update. Adoption never reduces ZFS reservations or pools old disks into unused provisioning capacity.

The Servers page shows both hosts together: CPU model/count, RAM, physical disk and partition layout, ZFS mirror topology, physical free versus unreserved space, datasets/volumes, user allocations and each allocation's VM commitments. Written data and snapshot figures come from ZFS; they are not guest filesystem free-space measurements. After provisioning Ivan, the global sidebar counts five VMs; scoped Users, Operations and Pending Requests retain a visible server selector. The informational deployment banner is removed; actionable failures and pending requests remain visible.

Installation, adoption and qualification preserve original VM configurations, disk and dataset identities, reservations, snapshots, host network configuration and SSH policy. The separately authorized November 13 maintenance changed only Heidi's SSD disk and its accounting, as recorded below; no physical host was rebooted.

## Failed starts and recovery

A blocked operation keeps its resource claims and original task evidence. The supported managed recovery action, `acknowledge_failed_start`, is deliberately narrow: an administrator reviews a fresh inspection and authorizes closing a failed Start attempt. The root helper requires the exact retained failed `qmstart` receipt, the original protected VM and storage identities, unchanged owner policy, a stopped guest with no lock/PID, and no active native task for that VM. It repeats those checks under its execution lock before committing the decision.

The second managed recovery action, `acknowledge_refusal`, closes a blocked start, shutdown, stop or delete that the helper refused before it began: every step is still planned, with no effect intent, Proxmox task receipt, identity or witness recorded. A typical case is a delete refused because the VM's disks still have snapshots. The helper requires a managed binding, the original job to pass validation, the machine record to be unchanged, and the VM to exist, unlocked, with no active native task. It rechecks this under its execution lock and commits the cancellation together with the decision. The operation becomes `cancelled` at step `refusal_acknowledged` and keeps its refusal reason. The VM returns to the state it had when the operation was requested, and keeps its data and allocation. Fix the cause, then request the action again.

Acknowledgment cancels only that failed attempt. It preserves its error and UPID, retains the VM and budget, and performs no guest power operation. A new Start is a separate request. Running, successful, missing or uncertain tasks, changed resources and stale inspections do not authorize acknowledgment. Other blocked operations continue to require investigation; there is no generic force/reset button.

The real qualification exposed a service sandbox incompatibility: Proxmox writes `/proc/sys/net/ipv6/conf/<tap>/disable_ipv6` while activating a new guest's tap interface. The managed helper unit installed by that deployment retains `ProtectKernelTunables=yes` with a required writable exception for `/proc/sys/net/ipv6/conf`. The lab template in `deploy/systemd/` does not include that exception. The unprivileged controller receives no such exception. Existing bridge configuration is unchanged.

Qualification also exposed a legacy extended-job submission assignment that lowered the managed helper schema marker from 7 to 6. Managed validation paused the helper before the queued acknowledgment could execute. Management admission was stopped and both state directories were cold-copied. The fix preserves the maximum schema version. The explicit offline repair requires a pinned raw journal digest and matching private cold backup, validates the complete schema-7 state against the unchanged root binding, and promotes only that marker; it does not reset databases, discard accepted requests or replay VM effects.

An unclean service shutdown can require redb's native recovery. The repair command refuses that by default. Explicit `--allow-native-recovery` first validates recovery on a memory copy, then checks that the original recovers to the identical journal record before the schema-only transaction. The original cold copy remains unchanged. Unknown work, missing state, links, quarantine markers, mismatched pins and active database locks still refuse repair. This is an offline incident tool, not an automatic startup fallback.

## State backups and restarts

The operator tooling that installed and adopted the managed deployments was never committed to this repository. The historical V6 deployment was recorded by `deployment/persistent_management.py`, and the fleet generation used `deployment/managed_adoption.py` with exact saved plan digests, frozen artifacts and a `managed_adoption_status_worker.py` status worker. This repository contains the Rust side: the managed backend (`src/control/managed.rs`, `src/control/adoption.rs`) and `hearth-ops prepare-adoption`.

Adoption works as follows. The adoption worker first verifies the previous installation, closes management admission, refuses unfinished work and relevant active Proxmox tasks, stops only the owned helper/controller, and retains cold state plus a verified paired backup. `hearth-ops prepare-adoption` then writes a fresh quarantined pair from that backup and a hash-pinned managed configuration, retaining all identities, accepted requests, jobs and historical records. Independent live reconciliation authorizes that specific adoption stage before replacing only the owned databases, configurations and binaries. This is not generic activation of restored backups. Exclusive intents and retained evidence prevent blind replay after an incomplete activation.

In the deployed fleet, status, restart and backup plans live under each environment's `fleet-v1/status` directory. The status worker verifies the installed binary/unit/configuration pins and includes adopted VM IDs when checking active external tasks.

The restart and backup actions stop only the owned controller/helper after their work is settled. Paired backups include the controller catalog, helper journal and server identity. Verification checks the saved payload digest and host/environment/server pins. Restoring always creates a fresh quarantined tree with activation barriers on both databases; it does not replace active state or restart workers from restored data. These are control-plane state backups, not VM disk backups.

## Fleet UI action safety

The November 12 review of the fleet UI is recorded in [CHANGELOG.md](../CHANGELOG.md#2026-11-12).

Fleet mutations and creation forms capture an explicit server origin; opening a form for another host no longer changes the scoped sidebar selection. A mutation rechecks that same route after obtaining the device queue, immediately before launch. Replaced/removed routes, changed capabilities or newly conflicting VM observations refuse without launching a child. The original request ID remains in Pending Requests. A queued attempt can be cancelled from its action sheet with Escape; cancellation after launch remains an uncertain outcome rather than pretending server work was undone.

Start additionally requires a successful peer dashboard from every other configured controller, no more than 60 seconds old and not currently refreshing. A same-named peer VM in any state except stopped or deleted refuses Start. This guards the stopped HDD recovery copy while the SSD VM is active. It is a conservative GUI observation guard, not distributed exclusion between independent control planes; unavailable peers deliberately prevent Start until refreshed. Historical imports on unrelated unadopted legacy profiles are not an authoritative live fleet inventory.

Every fleet deletion, and forced Stop of an existing allocation VM, requires the exact displayed server/VM name. Existing disk data is called out in that confirmation. Narrow windows use compact table columns and two-line resource cells, with Actions visible. Scoped admin confirmations name their server and distinguish total fixed allocations from editable provisioning capacity. Notices identify their route, successes expire, and at most three cards cover content. Pending not-sent entries remain until the administrator explicitly retries or discards them; they are never resent automatically.

## Guest access

Guest creation installs the supplied public key in the approved image's default account. Coworker SSH jump-account onboarding remains a separate administrator step. Existing Frank/Heidi jump accounts and policies are preserved. Qualification uses the administrator's pinned host SSH connection and an ephemeral guest key to access only newly created test guests. Do not distribute an administrator root jump command as a coworker's normal login method.
