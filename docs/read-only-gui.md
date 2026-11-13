# Read-only physical-host viewer

The native app can now observe independent real Proxmox/ZFS hosts, alongside its existing lab-controller connections. This path is deliberately separate from controller state, authorization, ownership mapping and the privileged mutation helper. It does not install a server component or adopt a resource. All three management-authority flags remain false.

Swift starts `pve-dev-inventory` with a reviewed profile path and saved public binding. Rust loads that profile once, checks the binding before opening a connection, runs only the embedded discovery collector over SSH, validates the complete snapshot and emits a reduced typed read model. Swift never parses the host profile, reads a private key or database, constructs SSH arguments, or composes a shell command.

## Host profile

Create a separate local JSON file for each physical host:

```json
{
  "schema": "pve-dev-observation-profile/1",
  "destination": "root@192.0.2.10",
  "port": 22,
  "known_hosts": "/absolute/path/pve1-known-hosts",
  "known_hosts_sha256": "<64 lowercase hex SHA256 of exact known_hosts bytes>",
  "identity": "/absolute/path/ssh-key",
  "node": "pve1",
  "host_key_sha256": "SHA256:<43 character Ed25519 fingerprint>",
  "machine_id_sha256": "<existing collector host.machine_id_sha256>",
  "zpool_guid": "<existing collector zpool.guid>",
  "zfs_root": "rpool/data",
  "vm_notes": {"103": "Recovery copy · original HDD"}
}
```

Review the initial host key through the established trusted connection. Machine and pool pins come from an independently reviewed initial discovery snapshot. They are not learned automatically from the first refresh. The `identity` path is optional; an existing local SSH agent can be used instead. No password prompt or agent forwarding is permitted. A non-root collector account may be used if it can execute all the collector's read-only commands; this feature does not provision such an account.

Use a **dedicated known_hosts file with exactly one Ed25519 entry**. Rust verifies its bytes against the profile hash and independently computes the key fingerprint, which must equal `host_key_sha256`. It copies those validated bytes into a private temporary directory and passes that copy to SSH. This closes the original path-replacement race. The public binding includes node, host key, machine hash, pool GUID, ZFS root and known-hosts hash; changing any of these requires explicit review and trust again. The temporary copy is removed on normal completion, timeout and handled cancellation.

The destination, port and identity/known-hosts paths are checked before use. Rust invokes the fixed `/usr/bin/ssh` executable directly, with `-F none`, strict Ed25519 host-key checking, no global known-hosts/config hooks, no forwarding, no ProxyCommand, and key-only batch authentication. The only remote command is the fixed `python3 -I -`, receiving the embedded, validated collector source on stdin. Configured paths are arguments, never shell interpolation. OpenSSH's own `-o` path quoting preserves a single temporary known-hosts file even if the temporary directory contains spaces.

Informational `vm_notes` are explicitly per host and VMID. They label a stopped recovery copy independently of an active VM with the same ID elsewhere; they neither identify an owner nor grant authority. Refresh still reports the actual observed VM status, independently of the note.

## CLI and GUI

```sh
cargo build --bin pve-dev-inventory
target/debug/pve-dev-inventory observe-profile-info --profile /absolute/host.json
# Save/review the public `binding` returned by that local-only inspection.
target/debug/pve-dev-inventory observe --profile /absolute/host.json \
  --expected-binding <saved-binding> --timeout-seconds 90
```

The inspection checks only local profile/known-hosts inputs and does not connect. Each `observe` performs a fresh collection, accepts only the expected binding, checks collector consistency and node/key/machine/pool/root identity, and outputs `pve-dev-observation-result/1` JSON. It never opens either database. Failed collection/validation emits no accepted observation. Profile input is capped at 64 KiB, known-hosts at 1 MiB, collector stdout at 8 MiB and stderr at 64 KiB. Collection has a 5–600 second timeout; timeout, handled SIGTERM/interrupt and output overflow kill/reap the local SSH child. Closing collector stdin explicitly before waiting for output is required for Python to receive EOF.

In Settings, choose the inventory executable (or keep its sibling default), **Add Read-only Host**, choose that JSON profile, then **Save and Trust Profile**. Select the host in the sidebar and refresh. These entries offer Overview and Virtual Machines only. The normal controller paths preserve their existing saved server/environment/device pins, stale-form guards and pending-request ledger bindings.

The overview shows Proxmox version, ZFS pool health, root dataset used/available accounting, VM counts, observed filesystem quotas/reservations and collector warnings. VM rows show actual status, configured CPU/RAM/disk allocation and informational notes. IDs include the host identity, so VMID 103 on two hosts does not collide. The viewer does **not** measure host CPU load, current RAM use, SMART counters, guest service readiness or backup health. Dataset available space follows ZFS reservations and is not presented as uncommitted physical pool capacity.

Every page identifies the host and clearly labels **Observed** (host capture time), **Refreshed** (local successful refresh time), and the snapshot digest. Cached content stays visible during refresh. A failed refresh retains the old timestamps and explicitly says the previous observation is still displayed. Automatic refresh starts off at launch and is separate for each host. Observations are point-in-time snapshots, not a continuously reconciled management catalog.

## Isolated local review app

The helper below builds a separate ad hoc signed app without launching it or replacing the user's running app/settings:

```sh
python3 macos/scripts/build-read-only-review.py \
  --inventory-executable /absolute/pve-dev-inventory \
  --host 'HDD · pve1=/absolute/pve1.observation-profile.json' \
  --host 'SSD · pve2=/absolute/pve2.observation-profile.json'
```

It asks Rust for each public identity, seeds only read-only entries, and uses the separate `dev.pve.readonlyreview` preferences suite plus a private review ledger location. On first launch the review app loads those app settings from its resources and refreshes the selected host; it does not read host profiles in Swift. The production source remains unchanged except in a temporary copy's app entry point, where the stores are injected. A source-comparison receipt in the bundle proves the other Swift files match the production source. The wrapper is only for local review, not a Developer ID signed/notarized release. Use a new `--app-output` when retaining an earlier review build.

## Validation and boundaries

- All 27 discovery integration tests passed, including new known-hosts/profile binding, SSH route injection, machine/pool identity refusal, host-scoped summary and false-only authority checks.
- Four process-boundary tests passed: collector EOF, timeout kills/reaps its child, bounded stdout/stderr, and nonzero exit output refusal.
- Swift debug compilation, the isolated review release build and 78 tests passed (one opt-in Rust controller integration test skipped in that run). New checks cover typed observation invocation, trust refusal, changed-host/authority refusal, controller-RPC configuration refusal, independent same-ID host caches, read-only/stale-controller mutation refusal before creating a ledger or starting a process, and refusing an existing controller pending retry without changing its ledger bytes.
- A fresh actual-host CLI check on November 10, 2026 (Chicago; `2026-11-11T01:18:37Z`) passed on both Hetzner machines: both pools ONLINE; pve1 had running 101/102 and stopped recovery 103; pve2 had running 103. All identity pins matched and all management-authority flags were false. Operational profiles/captures are private under `.lab/read-only-hosts-20261111/` and must not be published.
- After explicit user approval, the separate review app also passed a native visual smoke check against both actual hosts. Overview and VM pages showed the correct host, status, recovery labels and capture/refresh timestamps, with no management controls. No mutation ledger was created. See `evidence/read-only-gui-20261111/native-visual-qa.md` for scope and limits.

The collector still supports only its documented single-node ZFS topology. Origin rests on pinned SSH and trusted local profile review, not a signed observation attestation. The existing account's SSH authority is broader than this fixed collector; this does not create a restricted server-side SSH account. Direct/public iroh reachability and production controller installation are separate tracks. No production write adapter, VM transfer capability, continuous drift repair, ownership adoption, or multi-host private network is introduced here.
