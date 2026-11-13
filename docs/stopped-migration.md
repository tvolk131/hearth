# Durable stopped migration

This implementation moves one stopped managed guest between two independent
controllers. It does not form a cluster or use Raft, broad Proxmox API tokens,
shared storage, or live migration. Each administrator separately approves the
same immutable plan. Destination approval binds the exact signed source approval,
including its renewal nonce. Peer grants expire in 60 seconds to 24 hours.

The public CLI commands are `migration describe`, `plan`, `authorize`, `start`,
`list`, `status`, `renew`, `abort`, and `finish`. The CLI exposes typed JSON files
for reviewed plans and grants. The GUI uses independently pinned administrator
profiles for the two hosts. The migration-only iroh endpoint uses the server's
existing signing identity and a separate ALPN. Every data request verifies its
peer, recipient environment, exact locally authorized grant, plan, and expiry.
It exposes no general control RPC or filesystem paths.

The destination first reserves quota and creates an empty, inactive staging VM
using a migration-specific helper capability. It never reimports the original
base image: a source may have been shrunk below that image's original size. The
source retains a persistent `migrate` lock, disabled autostart, and readonly ZFS
volumes while exporting a full stopped manifest. The transfer includes root and
cloud-init bytes plus supported custom snippet dependencies. Source-generated
cloud-init metadata is captured as an immutable custom metadata dependency, so a
change of destination VMID does not silently change the guest's instance ID.
The target preserves source SMBIOS, VM generation ID, MAC, network configuration,
and guest provenance, while rebinding only locally owned disks and snippet paths.
Consequently the target keeps the source guest IP; callers must not infer its
address from its new VMID.

Data uses authenticated bounded frames, normally 128 KiB of raw data per chunk,
with zero ranges coalesced up to 8 MiB. Host RPC messages remain below 256 KiB.
Target writes persist an exact chunk intent, fsync data, and then persist offsets.
An identical last-chunk replay verifies existing bytes; changed or out-of-order
replays fail. No caller supplies a local device path. Helper-owned creation and
GUID records choose and attest every file descriptor. Restarted helpers recheck
pool, machine, stopped configuration, volume identity, and fence before reopening.

Sealing is queued durably. The destination background worker hashes the final
bytes, pins the imported configuration and identities, and signs readiness while
still locked and readonly. The source persists its signed relinquishment before
sending it. The destination persists acceptance before unlocking the target.
A lost reply retries the same committed handoff, even after the original grants
expire; it cannot restore source ownership. The target may then run normally for
guest verification. The original source stays fenced and quota-charged until an
administrator explicitly finishes the transfer and confirms recovery deletion.

`abort` and `finish` return a durable `cleaning` status promptly. Poll `status`
until the terminal result. A source abort first restores and verifies its original
VM, then issues a signed cancellation receipt. Destination abort requires that
exact receipt and finishes only after staging cleanup. A ready destination must
never be discarded based only on its own missing handoff: the source may already
have committed an undelivered ownership decision. Cancellation is forbidden once
source relinquishment exists.

Native effects persist intent before submission. Ambiguous stage/fence/seal
outcomes remain retained for inspection instead of being automatically replayed.
An interrupted cleanup reconciles completed VM/config/process/task absence and
original volume GUID absence globally; it never resubmits an unknown destroy.
Metadata cleanup deletes only the exact owned storage entry and proven file
inodes after disk/VM absence. Unexpected references, files, GUIDs, mounts,
snapshots, hooks, extra devices, or storage mappings fail closed. An accepted
target's activation reconciles only its exact committed readiness identities.

The initial adapter is limited to the existing managed VM range 9001–9099,
independent thick root and 4 MiB cloud-init zvols, the configured `vmbr77` network,
and root-owned snippets inside the private scoped helper state. Physical use
additionally requires the root-configured migration feature and exact disposable
owner, VMID, dataset, machine, and pool binding. Ordinary managed creations keep
their existing limits; the incoming capability admits only its reviewed plan.
Existing unscoped VMs cannot be imported implicitly. This is stopped migration;
there is no automatic cross-host rollback, filesystem conversion, or online cutover.

Validation:

```sh
nix develop ./nix --command cargo test --test migration
python3 -m unittest discover -s lab -p test_migration_adapter.py
```

The Rust suite includes actual loopback iroh connections between independent
controller/helper pairs, byte transfer, destination lifecycle after acceptance,
explicit asynchronous source cleanup, exact renewal retries, source-first abort,
partial-target seal refusal, and changed chunk replay rejection. Cryptographic
and durable transition cases cover domain separation, forged/crossed proofs,
expiry, lost handoff acknowledgements, and retained ownership holds. Python tests
exercise native configuration mapping, immutable metadata dependencies, cleanup
scope, global GUID absence, and active task refusal. These tests do not establish
physical Proxmox qualification; physical evidence is recorded by the parent
trial harness using fresh resources only.
