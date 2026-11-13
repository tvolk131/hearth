# Scoped maintenance features

The Rust controller, privileged helper, CLI and native GUI now have explicit enrollment, offline disk expansion, receipt-based resize recovery, durable stopped migration, separate-`/boot` shrink support and operation timing. These paths require an explicitly configured scope; observation profiles never acquire mutation authority.

For physical trials, a root-owned configuration pins the node, machine identity, pool GUID, fresh parent dataset GUID, owner/VM allowlists, feature capabilities and resource ceilings. The legacy scope keeps its existing limits. A schema-6 scope can enable `resize`, `recovery`, `enrollment` and `migration` separately. A larger mathematical disk limit does not enlarge a configured scope or allocate disk space.

## Disk maintenance

Resize requires a stopped guest with a supported ext4 root on GPT. A separate ext4 `/boot` is supported and preserved byte-for-byte, together with its UUID and partition metadata. Root selection uses a unique supported Linux-root partition type or an explicit partition UUID; it never guesses the last ext4 filesystem. Unsupported layouts, snapshots, clones, extra dependencies and uncertain identities are refused.

Shrink copies the source, checks and resizes only the replacement, and retains the original until explicit guest verification and recovery deletion. Its reservation includes the conservative copy workspace. An SSD host with insufficient workspace can therefore require a stopped transfer to a host with more room, shrinking there, then transferring back. A VMID tombstone cannot be reused; return placement may need a fresh VMID while preserving the guest's disk, MAC, IP and cloud-init identity.

Deleting the verified recovery copy releases the VM's extra allocation. The user's reserved budget remains a separate administrator policy: reclaiming that reservation for another coworker also requires an explicit reduction of unused quota.

Expansion reserves the increased charge before work starts. It records separate prepare, zvol, partition, filesystem and unlock phases. The controller changes the visible size and releases the pending increase only after verified completion. A partial expansion remains charged and fenced. Neither path supports live filesystem resizing or arbitrary guest layouts.

## Inspection and recovery

`pve-devctl operations inspect` returns a review token, evidence and the actions currently eligible for full verification. Quick inspection does not claim to verify disk bytes. `operations recover` queues an administrator decision bound to that token. Under the helper lock it checks durable private receipts, current GUIDs, configuration and disk contents before accepting a previously completed resize phase or aborting a preparation that has not swapped/expanded the source.

Recovery does not replay an uncertain copy, force-unlock a VM, discard a claim without proof, or reconcile arbitrary operation kinds. Ambiguous or unsupported outcomes remain blocked for investigation. Original operation IDs, cancelled decisions and resource tombstones remain durable.

## Enrollment

Enrollment is an explicit reviewed action for a stopped, previously unmanaged VM already inside the configured owner's storage namespace and VM allowlist. The plan pins configuration, resource profile and independent thick root/cloud-init volume identities. Execution rechecks it before adding ownership metadata and helper provenance. Merely discovering a VM does not enroll it. Extra devices, foreign claims, snapshots and unsupported custom configuration are refused.

## Stopped migration

Controllers keep independent databases and administrator lists. There is no Raft cluster or permanent peer administrator. Each transfer uses a signed immutable plan and two independently issued, expiring grants bound to controller identities, environments, owners, VMIDs and byte limits. The destination grant binds the exact source authorization, including its renewal nonce.

A separate iroh ALPN carries bounded chunks. Physical trial control endpoints remain loopback-only; a separately enabled public migration endpoint can use outbound relay/hole punching without opening a general control service. The helper derives all paths locally and checks its own scope.

The source stays stopped/fenced. The destination receives inactive staging, verifies complete hashes and metadata, and signs readiness. A durable source relinquishment precedes destination activation. The destination's signed acceptance must be retained before an explicit source recovery deletion can complete. Cancellation requires source restoration and a signed source-abort proof before the destination may discard staging. Grant expiry or a lost acknowledgement never restores source ownership automatically.

Hashing, sealing, cancellation and explicit cleanup run as durable background work; the GUI and CLI inspect progress/status. Destination activation leaves the guest stopped for explicit start and health verification. This is offline copying, not memory/live migration. The first adapter supports the isolated `vmbr77` network and managed root/cloud-init disks with bounded owned metadata dependencies; it is not general Proxmox-cluster migration.

## Qualification boundaries

The new isolated host harness is `deployment/six_feature_trial.py`; its reviewed plan binds runtime/source/binary hashes and offline validation before any setup. It reserves fresh VMIDs and a nonce-scoped capped dataset, independently preserves existing VM configurations/states and ZFS identities/reservations, and never reboots a physical host. Uncertain results retain only the test resources for investigation. The old automatic physical driver remains gated independently.

Physical tests use small real guest disks. Separate admission and GPT CRC/bounds tests cover a 750 GiB profile; that is not a claim that 750 GiB was allocated, copied or resized on hardware. Simulated crash matrices, real transport tests and disposable hardware tests strengthen confidence but are not a formal proof or a physical power-loss qualification.

An initial physical migration trial exercised a lost destination-preparation reply: native preparation outlasted a 30-second helper RPC deadline, completed durably, and the next exact request reconciled that result without repeating preparation. Preparation now uses the longer maintenance deadline. A later metadata-storage API argument collision was fixed with a regression at the real wrapper boundary. All transferred slot hashes matched, and source-first cancellation safely retired that trial before a new build. Native sealing failures now retain their first bounded diagnosis across restart and refused retries.

An earlier source transport error can still remain visible while byte offsets continue advancing until the coordinator completes its pass. Treat the phase and durable offsets together when inspecting such a transfer. Queued preparation would further improve responsiveness during slow native inspection.

The busy HDD trial also exceeded a client deadline while issuing a return plan. The controller had durably stored the authorization; retrying the identical request ID returned the identical signed grant. A later read-only status poll timed out while the independent backend transfer continued. Read-only observations may be retried with a bounded policy. An uncertain mutation requires its original request ID or inspection of retained state; a client timeout does not authorize starting a second transfer. These small-disk drills do not qualify bulk-transfer performance on production-size guests.
