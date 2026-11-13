# Read-only discovery and observation import

`pve-dev-inventory` (library: `pve_dev::discovery`) is an **observation-only** phase. It:

- prints a fixed, read-only Python collector and the exact SSH command an operator can run;
- validates collector output offline against an explicit host pin;
- proposes an ownership mapping with every owner left **null**;
- imports observed state only when the mapping is explicitly confirmed, into a dedicated redb file.

The original offline workflow below never opens a network connection or runs SSH. The separate, explicit `observe` command now runs only the embedded collector over pinned SSH for the native read-only viewer; see [read-only-gui.md](read-only-gui.md). No command has a Proxmox write adapter or touches the controller `state.redb` or helper journal. Imported records and GUI observations **never** create controller users, VMs or operations, and they never authorize management: `managed`, `authorizes_management` and `creates_controller_state` are always `false`. The type (`AlwaysFalse`) refuses to deserialize `true`.

## Workflow

```sh
B=target/debug/pve-dev-inventory
$B collector > /secure/collector.py            # review it; optional --zfs-root rpool/data
$B ssh-command --destination root@HOST --known-hosts /abs/known_hosts \
   [--identity /abs/key] --collector /secure/collector.py --output /secure/snap.json
# The operator (not this tool) runs the printed command once:
#   ssh -F none -T -p 22 -o BatchMode=yes -o StrictHostKeyChecking=yes \
#     -o UserKnownHostsFile=/abs/known_hosts -o GlobalKnownHostsFile=/dev/null \
#     -o HostKeyAlgorithms=ssh-ed25519 -o UpdateHostKeys=no -o ForwardAgent=no -o ForwardX11=no \
#     -o ClearAllForwardings=yes -o PermitLocalCommand=no -o ProxyCommand=none -o RequestTTY=no \
#     root@HOST 'python3 -I -' < collector.py > snap.json
PIN="--snapshot /secure/snap.json --node pve1 --host-key SHA256:<ed25519 fingerprint>"
$B validate $PIN                               # prints snapshot_digest + warnings
$B propose  $PIN --output /secure/mapping.json # new file only; owners/dispositions null
# Edit mapping.json: set every allocation "owner", and every VM "disposition"
# ("owned" + "owner", or "unmanaged" with owner null).
$B import --db /secure/observations.redb $PIN --mapping /secure/mapping.json \
   --confirm-snapshot <snapshot_digest>
$B record --db /secure/observations.redb $PIN  # optional: store a snapshot without a mapping
$B verify --db /secure/observations.redb       # integrity check + summary (never creates a DB)
$B show   --db /secure/observations.redb --host-id <id> --snapshot-digest <digest>
```

All commands print JSON to stdout and exit nonzero on refusal. `-F none` ignores SSH config files, so their `ProxyCommand` or `Match exec` hooks cannot run. The host-key pin is the Ed25519 fingerprint, for example from `ssh-keygen -lf` run against the pinned known_hosts entry.

## Collector (`pve-dev-discovery-collector/2`)

The collector runs only `hostname -s`, `ssh-keygen -l` (public host key fingerprint), `pveversion`, `zpool list`, `zfs get` and `pvesh get`. A test runs it against fake commands and checks every call is one of these read-only forms. It reads `/etc/machine-id`, but emits only a domain-separated SHA-256 of it. It does **not** read snippets, `authorized_keys`, private keys, `sshd -T`, or guest disks.

Only allow-listed VM/CT option **values** are emitted: CPU, memory, boot, disks, `netN`, `ipconfigN`, `nameserver`, `ciuser`, `tags`, `lock`, `digest` and similar. Every other option, including `sshkeys`, `cipassword`, `cicustom`, `description`, `args`, `hookscript`, `smbios1` and `vmgenid`, appears **by name only** in `omitted_keys`. Pending-change keys are likewise reported by name only. Storage, pool and interface objects are reduced to allow-listed fields.

Per-command failures abort with a nonzero exit (stderr is not echoed). The collector does not fill in missing values: ZFS `-` becomes `null`, and an absent property aborts collection. Configs come from `--current 1`.

**Consistency:** ZFS datasets, ZFS snapshots, guests, storage, pools and network are collected twice. `consistency.{zfs_stable,guests_stable,config_stable}` report whether structure is unchanged: ZFS type, GUID, quota, reservation, refreservation, volsize and origin; full guest records; and configuration. `used`/`available` are excluded from that comparison. Any `false` value is refused at validation.

Storage `content` and `nodes` are comma-separated sets, so collector v2 sorts those lists before comparison. Real PVE returned equivalent content sets in different orders on consecutive reads; this must not be treated as drift. A changed member still fails consistency. The original rejected v1 snapshot was not imported or patched into a valid observation.

## Snapshot schema (`pve-dev-discovery-snapshot/1`)

Top-level keys:

| Key | Contents |
|---|---|
| `schema`, `collector`, `captured_at` | `YYYY-MM-DDTHH:MM:SSZ`, from the host clock |
| `host` | `node`, `host_key_sha256`, `machine_id_sha256`, `pve_version`, `cluster_nodes` |
| `zfs_root`, `zpool` | `name`, `guid`, `health` |
| `datasets` | Name → `type`, `guid`, `quota`, `reservation`, `refreservation`, `volsize`, `origin`, `used`, `available`, `mountpoint`, `canmount`. Raw strings or null, and every key must be present. |
| `zfs_snapshots` | `ds@snap` → `guid`, `used`, `createtxg` |
| `storages`, `pools`, `network` | Allow-listed configuration objects; `pools` include member IDs |
| `vms` | vmid → `status`, `config`, `omitted_keys`, `pending_keys`, `snapshots` (Proxmox snapshot names) |
| `containers` | vmid → `status`, `config`, `omitted_keys` |
| `consistency` | Stability flags described above |

Unknown or missing fields are refused (`deny_unknown_fields`, with nullable fields that must still be present).

**Digest:** `snapshot_digest` is SHA-256 of `"pve-dev-discovery-snapshot/1\n"` plus canonical JSON with recursively sorted keys and no whitespace. It is independent of formatting and key order, and it changes with any content change. `raw_sha256` of the exact bytes is kept as provenance.

### Validation refuses

- A wrong `--node` or `--host-key` pin, or a cluster with more than the local node.
- Unstable collection or a malformed timestamp or fingerprint.
- Datasets outside the root, missing parents, missing/invalid/duplicate GUIDs, unknown dataset types, missing quota/reservation/volsize, or snapshots of uncollected datasets.
- Enabled `zfspool` storage whose pool was not collected, a disk on unknown storage, or a disk whose zvol is missing. These cases are **incomplete inventory**.
- A zvol attached to two guests, a VM config without `digest`, or non-allow-listed config keys.
- Secret-bearing field names anywhere, or strings containing private-key, `ssh-… AAAA` public-key, or `#cloud-config` material.

Warnings are recorded but do not cause refusal. They cover a non-ONLINE pool, unattached volumes, a disk named for another guest, a Proxmox snapshot missing on a disk, and unknown bridges.

## Mapping (`pve-dev-discovery-mapping/1`)

```json
{"schema":"pve-dev-discovery-mapping/1","snapshot_digest":"…","node":"pve1","host_key_sha256":"SHA256:…",
 "allocations":[{"dataset":"rpool/data/dev-frank","guid":"…","owner":"frank","name_hint":"storage dev-frank"}],
 "vms":[{"vmid":102,"config_digest":"…","disks":{"scsi0":"<guid>","ide2":"<guid>"},
         "disposition":"owned","owner":"frank","name_hint":"dev-frank"}]}
```

`name_hint` is informational only. It is excluded from `mapping_digest` and never used for authorization. Import requires all of the following:

- `--confirm-snapshot` and `mapping.snapshot_digest` equal the validated digest, and the mapping host matches the pin.
- Every allocation has an explicit owner (lowercase letters, digits and hyphens, at most 48 characters, starting with a letter and ending with a letter or digit), an exact dataset GUID, is a filesystem below the root, and is not nested in another allocation.
- **Every** observed VM appears exactly once, with a matching `config_digest` and an exact `disks` slot→GUID map equal to its actual ZFS attachments.
- An `owned` VM has an explicit owner, no `lock`, and no pending changes. Each of its ZFS disks lies inside exactly one allocation with the **same owner**, and it has at least one such disk. Only CD-ROM media may be non-ZFS. Passthrough or external disks are refused.
- An `unmanaged` VM has no owner and no disk inside an owned allocation.
- No container uses a dataset inside an owned allocation.

## Observation store

The store is a dedicated redb file whose meta `kind` is `pve-dev-discovery-observations/1`. It opens only files containing its own tables (`discovery_meta_v1`, `discovery_hosts_v1`, `discovery_snapshots_v1`, `discovery_imports_v1`, `discovery_log_v1`), so a controller DB or other file is refused. Commits use `Durability::Immediate`.

- **Per-host storage:** `host_id = sha256("pve-dev-discovery-host/1\n" + host_key_sha256)`. Snapshots and imports are keyed `host_id/snapshot_digest`. A host record pins node, machine-id hash and zpool GUID. A changed identity for a known key, or a new key for a known node, machine or pool, is refused.
- **Import record:** `schema`, `host_id`, `snapshot_digest`, `mapping_digest`, `import_digest`, `provenance`, `observed`, `warnings`, `imported_at`, and the three always-false flags.
  - `provenance` contains snapshot schema, collector version, `expected_collector_sha256` (the importer's embedded source), `raw_sha256`, `captured_at`, node, host key, machine-id hash, zpool GUID, PVE version, and importer version.
  - `observed` contains allocations (GUID, owner, quota/reservation, storage IDs, zvols with size/refreservation/attachment/ZFS snapshots), VMs (disposition, owner, allocation, CPU/memory strings, disks, NICs, Proxmox snapshot names, omitted key names), unowned volumes and container IDs.
- **Dedup:** importing the same snapshot with the same mapping digest is a `Duplicate` no-op, including after reopen. A different mapping for an imported snapshot is refused.
- **Ownership stability:** ownership is tracked per host by `dataset:GUID` and `vm:VMID`, with `unmanaged` for unmanaged VMs. A later import that changes recorded ownership is refused. A snapshot not newer than the latest import (by `captured_at`) is refused as stale.
- **Tamper evidence:** every snapshot/import write appends a hash-chained log entry containing the record's SHA-256. `open` and `verify` recompute the chain, every record hash and digest, and the host records by replaying the log. Any mismatch refuses opening.

## Real read-only validation (November 9, 2026)

Collector v2 ran over strictly pinned, non-forwarding SSH against the existing Hetzner PVE **9.2.26** node. The capture at `2026-11-10T01:32:56Z` is November 9 in America/Chicago. It validated without warnings: ten datasets, six volumes, three VMs, no containers or snapshots, and all double-read consistency checks true. No Proxmox/ZFS configuration, guest state or access policy was changed; SSH can still generate ordinary access logs.

The explicit mapping binds VM 101 and `dev-erin` to Erin, 102 and `dev-frank` to Frank, and 103 and `dev-heidi` to Heidi, using dataset GUIDs and exact disk-slot maps rather than names as authority. Each observed owner allocation has a 1.5 TiB quota and reservation; all six zvols are fully reserved. The first import recorded the observation, a separate-process re-import returned `Duplicate`, and reopening/verifying the database found one snapshot, one import and two intact log entries. All management flags remain false and no controller state was created.

Private local artifacts are under `.lab/hetzner-discovery-v2-20261110T013233Z/`: `snapshot.json`, the reviewed `mapping.json`, final importer outputs, `receipt.json`, and **`observations-final.redb`**. Use this final database; the earlier `observations.redb` is retained as intermediate evidence from before correcting missing NIC model/MAC extraction. Do not publish this operational inventory or the `.lab` directory. The final receipt checks that the collector actually sent has the import record's expected source hash.

The initial v1 capture was correctly refused because PVE returned equal storage-content sets in different orders. It was retained as rejected evidence, not modified to bypass validation. A new v2 capture sorts only set-valued storage `content`/`nodes`; regression tests still refuse actual membership changes.

## Known gaps

- The collector's real PVE paths and flags are now exercised on this specific single-node host, not every Proxmox version or storage topology. No management adapter is validated by this observation-only test.
- A snapshot has no cryptographic attestation. Origin rests on the operator running the pinned SSH command; the host fingerprint inside the snapshot is self-reported, not proven. `expected_collector_sha256` records what this importer would print, not what actually ran.
- Tamper evidence is not tamper-proofing. Nothing is signed or keyed, so anyone who can rewrite the whole file consistently can forge history. Back up or restore only by copying a stopped file.
- There is no supported remapping or ownership-transfer path. A legitimate owner change requires a new observation database after human review. A host re-key or rename is refused the same way.
- Staleness relies on the host clock (`captured_at`, one-second resolution).
- Single-node clusters only. Containers are recorded but cannot be mapped. Nested filesystems inside an allocation are refused. Non-ZFS VM disks (directory/LVM/Ceph) cannot be owned.
- Guest internals are not observed: Linux accounts, keys, cloud-init content and jump-account policy.
- Unknown option *names* are listed, so a newly introduced secret-bearing option name is never emitted with its value. However, a secret embedded inside an allow-listed value, such as a `tags` string, would be emitted. The validator scans only for key/cloud-init patterns.
- Nothing in this phase feeds the controller. Converting observations into managed resources would be a separate, explicitly authorized design.
