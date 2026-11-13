# pve-dev-host: privileged disposable-lab helper

`pve-dev-host` is the only component that changes Proxmox or ZFS. It runs as root **only inside the disposable QEMU Proxmox lab**. It listens on one local Unix socket and never opens a network listener. The non-root control daemon calls it through `pve_dev::control::host::rpc`.

## Commands

```sh
pve-dev-host serve --state-dir /var/lib/pve-dev-host --socket /run/pve-dev-host/host.sock --lab-config /etc/pve-dev-host/lab.json
pve-dev-host serve --state-dir DIR --socket DIR/run/host.sock --simulate --environment <32-hex nonce> [--world FILE]
pve-dev-host health --socket /run/pve-dev-host/host.sock
```

Lab config (`deny_unknown_fields`):

```json
{"environment": "<32 lowercase hex nonce>", "pool_guid": "<decimal guid from: zpool get -Hp -o value guid labpool>"}
```

Suggested systemd shape: `User=root`, `Group=pve-dev`, `RuntimeDirectory=pve-dev-host`, `RuntimeDirectoryMode=0750`, `StateDirectory=pve-dev-host`, `StateDirectoryMode=0700`, `UMask=0077`. The socket is chmod `0660` and gets the process group, so only `pve-dev` members (the auth daemon) can connect. The helper refuses a runtime or state directory that is a symlink or that is group-writable or accessible to others. It also refuses a runtime directory whose owner differs from the socket's owner.

## Wire contract

Each connection sends one JSON request, half-closes its write side, and reads one JSON reply until EOF. Both directions are limited to 256 KiB, with timeouts of 5 s to connect and 30 s for the exchange.

- `{"action":"health"}` checks the host identity and the journal. It returns `status: verified`, `step: healthy`, and the configured environment.
- `{"action":"submit","environment":E,"job":Job}` stores the whole job and its SHA-256 fingerprint in redb with `Durability::Immediate`, then replies. If the same operation ID arrives with the same payload, it gets the stored outcome. A different payload is an error.
- `{"action":"status","environment":E,"operation_id":ID}` returns the stored outcome. An unknown ID is an error.

Internally, replies use the envelope `{"ok":true,"response":HostResponse}` or `{"ok":false,"error":...}`. `rpc` turns `ok:false` into an `anyhow` error. The parent should treat that error as a transport or retry condition, not a terminal job outcome. A job outcome is terminal only when it is a `HostResponse` with `status: blocked`. Invalid profiles, names, VMIDs, budgets or kinds are recorded as `blocked` with `step: rejected`, and the outcome stays durable.

## Initialization and fail-closed rules

- The state directory contains `host-journal.redb` (exclusive redb lock: one helper at a time) and an `initialized` sentinel.
- If the journal exists, it must match the bound environment, backend kind and pool GUID.
- If the sentinel exists but the journal does not, the helper refuses to start rather than reinitialize.
- A fresh initialization is refused if `labpool/control` already exists, because that means a previous helper's history was lost.
- The host identity is checked at startup, by `health`, and before every job:
  - `/etc/pve-dev-lab-id` must equal the nonce.
  - The hostname must be `pve-lab`.
  - The `labpool` GUID must equal the configured GUID.
  - `labpool`'s `org.pve-dev:lab` property must equal the nonce.

  If the check fails, queued jobs stay untouched (they are not blocked) and the worker retries every 30 s.

## Resources

| Resource | Name | Ownership marker / constraints |
|---|---|---|
| Control root | `labpool/control` | `org.pve-dev:control=pve-dev-control:NONCE`, `mountpoint=none` |
| Owner dataset | `labpool/control/OWNER` | `org.pve-dev:owner=pve-dev-control:NONCE:OWNER`, `quota = reservation = budget MiB` (64..8192). Before it is created, the pool must have the budget plus 512 MiB free. |
| Owner storage | `ctl-OWNER` | `zfspool`, `pool` = exactly the owner dataset, `content=images`, `sparse 0`, `nodes pve-lab` |
| VM | VMID 9001..9099 | `description pve-dev-control:NONCE:OWNER:VMID`, `cpu qemu64`, `kvm 0`, `scsihw virtio-scsi-pci`, `cpuunits 100`, `protection 1`, memory 128..512, balloon 128..memory, cores 1..4, name 1..48 `[a-z0-9-]` |
| Root disk | `labpool/control/OWNER/vm-VMID-disk-0` | thick zvol, 16..512 MiB, created with `org.pve-dev:owner=…:OWNER:VMID` in one `zfs create` |
| Cloud-init | `labpool/control/OWNER/vm-VMID-cloudinit` | allocated by Proxmox via `ide2 ctl-OWNER:cloudinit`; identified by GUID/createtxg |

Owner names are 1..24 characters from `[a-z0-9-]`, must start with a letter, and must end with a letter or digit, so that `ctl-OWNER` is a valid storage ID. VMs are created **stopped**, with `boot order=scsi0`. Blank fixtures have exactly `{scsi0, ide2}`; opt-in image guests add `{net0, serial0}`. Image guests allow up to 8192 MiB disk and 2048 MiB RAM and a separate journaled image import before attachment. Proxmox generates their NoCloud contents from pinned owner SSH/network settings. See `bootable-guests.md` for source trust, recovery and readiness limits.

## Execution model

There is a single worker. Jobs run strictly in submission order. Each journal update is a short, separate redb transaction, and none is held across external calls. Plans:

- `ensure_user`: control → dataset → storage
- `create`: preflight → vm → volume → attach → cloud_init → verify
- `start` / `stop`: verify → start|stop
- `delete`: verify → stop → unprotect → destroy → release

Each step moves through `planned → intent → verified`:

- The boot ID is recorded at intent and at verification.
- For an asynchronous API call, the exact task UPID is stored before waiting. The UPID is parsed from the final JSON line of `pvesh` output. The helper waits on that UPID only.
- On verification, the helper records the resource identity: ZFS GUID/createtxg/properties, VM SMBIOS UUID/vmgenid/profile, and attachment strings. VM IDs, owners and identities live in the journal's `owners` and `machines` records.

Recovery rules:

- **Verified, then resumed:** persistent identities are re-observed. Any disappearance or drift blocks. Nothing is recreated or overwritten.
- **Verify step (start/stop/delete):** checks the owner dataset/storage and the VM config and volume identities against the recorded GUIDs and UUIDs. It also checks the exact device set and the absence of locks or forbidden keys (`args`, `hookscript`, `cicustom`, snapshots, templates, …).
- **Intent with a stored UPID:** waits for that exact task, including after a restart. A failed task, an unknown status or a timeout blocks.
- **Intent without a UPID, resource present and exact:** verified. The proof is the intent, the preflight absence witness and the exact marker or profile.
- **Intent without a UPID, resource absent, same boot:** **blocked pending investigation**. No blind retry.
- **Intent without a UPID, resource absent, new boot:** the effect is re-executed deterministically. A cloud-init volume that is allocated but not attached is re-attached by exact name.
- **Planned, but the resource already exists:** refused. The exceptions are the shared control, dataset and storage records already in the journal, and runtime start/stop/unprotect state.
- **Create preflight:** refuses any existing VMID in `/cluster/resources` and any `vm-VMID-*` dataset anywhere under `labpool`. The ownership marker alone never leads to adoption.
- **Delete:** never touches the owner dataset or storage. It runs `DELETE /nodes/pve-lab/qemu/VMID --purge 1 --destroy-unreferenced-disks 0`, then checks that the VM and both volumes are absent, then marks the machine `deleted`. The tombstone stays, and the VMID is never reused. If the VM is already missing, delete converges only when this delete's own destroy intent was recorded first. Otherwise the job blocks.
- **Completed operations:** never re-run after a reboot. VM run state reported by the parent is last-known.

Commands run with argv only (`tokio::process::Command`): no shell, cleared environment, fixed `PATH`, `kill_on_drop`, a 600 s command timeout and a 900 s task timeout. Child commands explicitly use primary GID 0: Proxmox cluster IPC requires root UID and root GID for privileged access, even though the helper's own group remains `pve-dev` for socket ownership. API paths, keys and values are checked against strict character sets. Nothing from the network becomes a path or a command.

## Simulated mode and tests

`--simulate` keeps a separate durable "external world" JSON file (fsync + rename). By default it lives in `<state-dir>/simulated-world.json`. It models ZFS (GUIDs, createtxg, quotas, reservations, thick refreservation), storages, VMs, locks, UPID tasks, boots and injected crash points (`before_effect`, `after_effect`, `after_receipt`). The test constructors are `HostConfig::simulated`, `Host::open`, `Host::handle`, `Host::run_pending`, `Host::journal`, `SimulatedWorld::{create,new,load,edit}` and `World::{new_boot,zfs_create}`.

`nix develop ./nix --command cargo test --test control_host` covers:

- idempotent and conflicting submission, restart persistence, and durable rejection of invalid jobs
- a shared owner dataset across VMs, and refusal of quota overcommit
- the pool safety floor
- refusal of foreign VMs (even with our marker) and volume collisions
- nine drift cases (quota, description, extra device, unused disk, lock, removed or recreated zvol, storage retarget, vanished VM), each blocking start and delete with nothing overwritten
- ambiguous same-boot intent blocking
- new-boot reconciliation
- an orphan cloud-init volume (blocks on the same boot, reconciles after reboot)
- a lost-UPID adoption check, and resuming an exact stored UPID
- task-timeout blocking
- the full lifecycle with a tombstone and refusal to reuse a VMID
- delete convergence only for the helper's own pending destroy
- no restart of completed operations after a reboot
- non-lab identity, journal loss and environment mismatch failing closed
- the single-instance lock
- UPID parsing
- lab-config validation
- the Unix socket (0660 socket in a 0750 runtime directory, bounded single replies, unknown actions rejected)

## Known gaps (honest)

- **The selected real-lab suite passed on Proxmox VE 9.2.26.** Evidence is in `evidence/real-controller-20261109T173920-5183e4/`. Storage creation, cluster-resource field types, sparse/balloon/cpuunits normalization, create/start/stop/delete, physical quota refusal and daemon restart recovery were exercised. This is not a production qualification, a real helper mid-effect crash test, or a guest-OS boot test. Other Proxmox versions and public iroh relay traversal are unverified.
- Under `pvesh`, a CLI worker task runs in the foreground, so the UPID can only be stored after `pvesh` returns. A crash while `pvesh` is running leaves "intent without receipt". That blocks on the same boot, by design.
- A VM left holding Proxmox's `create` lock, even after a new boot, blocks. There is no automatic unlock. An ambiguous same-boot attach, unprotect or cloud-init intent also blocks. Blocked jobs have no repair or unblock tool yet; investigate by hand.
- Preflight checks VMIDs cluster-wide and datasets under `labpool` only. It does not check other storages (`local`, `local-lvm`) for stray `vm-VMID-*` volumes. Destroy passes `destroy-unreferenced-disks 0` so it never sweeps those.
- The helper does not verify the peer of a socket connection (no `SO_PEERCRED`). Access control is the socket file's mode and group.
- The journal is a single JSON document in one redb table, rewritten on each update. That is fine at lab scale, but not designed for long histories. redb calls run on async worker threads.
- Existing positive lab owner budgets can be edited through the separate journaled policy plan in `quota-editing.md`; ordinary jobs cannot silently change a budget or revision. VM resize, snapshots, production image provisioning, a continuous guest-readiness service and HA/replication remain unsupported. Pinned lab image import is described in `bootable-guests.md`. The controller and helper both require owner/VM names to end in a letter or digit, so invalid names are rejected before admission.
- The simulated world is an approximation. Tasks finish instantly unless `stall_tasks` is set, and refreservation is modelled as volsize + 1 MiB. It is not a security boundary.
