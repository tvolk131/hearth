# Proposed independent-controller handoff

This is a **protocol design and executable specification**, not an implemented
migration API or permission to move a real VM. The independent `qm remote-migrate`
spike decides whether Proxmox can supply a suitable data-transfer adapter.
Neither a passing model nor a successful disk copy qualifies production migration.

## Authority and identities

Each controller remains authoritative for its own resources and durable journal.
There is no shared database, Raft group, automatic leader election or partition
failover. The GUI obtains authorization at both ends before a source-side job
coordinates the transfer; the GUI is not required to remain connected.

A reviewed transfer plan binds a unique migration ID, both controller public
keys and environment IDs, source VM UUID, source pool/disk identities, target
pool and reserved staging identities, destination account, storage charge and
network mapping. Local VMIDs are host-scoped and cannot establish identity.
Source and destination disk GUIDs need not be equal. Guest identity, cloud-init
metadata and final data consistency require separate checks.

Reserved staging identity can initially mean a job-owned namespace and VMID.
When the native copier allocates disk GUIDs during import, the destination must
durably bind those observed GUIDs before issuing final readiness. The model's
preselected GUIDs are an abstraction, not evidence that Proxmox can preallocate
or choose them.

Each end must authorize the operator independently. Matching user names or
knowing another host's public key does not convey permissions. Short-lived
peer grants cover only fixed migration actions for this immutable plan, with
recipient binding, expiry, replay protection and a resource/byte ceiling. They
do not enroll the other controller as an administrator, grant a shell, authorize
arbitrary dataset paths or let the peer expand its storage budget. Renewal must
be separately authorized for the same plan; changing peers/resources needs a new
review. Grants, receipts and operation IDs must be frozen before effects.

Authenticated iroh identifies a peer but does not implement this authorization.
Separate peer message types, grant validation and key/signature formats remain
unimplemented. The specification abstracts authenticated messages; it implements
no cryptography, network listener, helper commands or capability token format.

Native Proxmox API tokens are not automatically equivalent to these narrow
grants. The disposable spike found that remote migration requests also require
global `Sys.Audit` read access at the destination, beyond exact VM/storage/bridge
ACLs and `Sys.Incoming`. A future implementation must disclose that access or
keep the broader local Proxmox credential behind a job-scoped helper/proxy with
fixed operations. A token scoped to one VM can still authorize more operations
than one migration; expiry alone does not restrict those operations. The spike
does not implement this proxy or prove a minimum privilege set.

## Prepare and transfer

1. The destination durably claims capacity and creates inactive, non-autostarting
   staging resources bound to the migration ID. Pending and blocked copies stay
   charged. A grant cannot override local quotas or safety floors.
2. The source may pre-copy data while the VM runs. Such a copy is not final.
3. The source records a fencing intent, installs a persistent local fence and
   stops the VM. The adapter must verify both the fence and actual stopped state.
   A completed stop alone is insufficient because a host reboot could restart it.
4. The final stopped-data manifest is transferred and independently checked at
   the destination, which durably records a readiness receipt tied to its exact
   staging identities. An earlier pre-copy receipt cannot authorize later data.

Physical fencing is a critical **unproven adapter contract**. It must survive
process/host restart, prevent controller starts and Proxmox autostart/HA starts,
and be checked before committing the handoff. Disabling `onboot` alone does not
exclude other start paths. Possible Proxmox locks and their recovery/override
semantics must be tested. Arbitrary external root actions remain outside any
controller-only guarantee; production ownership and administrative coordination
need an explicit policy.

Custom cloud-init snippets are external dependencies, not ordinary guest disk
blocks. [Proxmox requires those files to be available on migration targets](https://github.com/proxmox/pve-docs/blob/master/qm-cloud-init.adoc#custom-cloud-init-configuration).
Our existing guests can use `cicustom`, including a pinned metadata instance ID.
Their authorized snippet contents/storage mapping/hashes and resulting instance
identity must be pre-staged and verified separately; a successful generated-seed
toy transfer does not qualify them. Hooks, local ISO references, passthrough and
other host-specific resources likewise need explicit support or refusal.

The first adapter should handle stopped guests and preserve disk size. It must
verify the destination remains inactive and cannot autostart before the explicit
handoff. Proxmox has its own multi-stage migration task; the model's transitions
cannot be assumed to map one-for-one to externally accessible API checkpoints.
Live guest state transfer and filesystem/disk shrinking are separate features.

## Commit and recovery

The source commits relinquished ownership to its own journal **before** a
handoff proof may leave it. The proof binds the exact plan, final manifest and
target readiness/staging identities. It is a durable ownership decision, not a
time-limited lease. The source can never infer permission to restart from a
timeout, an expired grant or a missing destination acknowledgement.

The destination verifies the proof and records its acceptance durably before
starting. Repeated identical proofs return the same result; changed proofs,
different peers/resources or cleaned-up staging identities are refused. Once
accepted, completion/reboot uses that durable ownership record. Expiry of the
old peer grant cannot roll ownership back. Status/renewal policy and permitted
post-expiry peer requests still need implementation and review.

Lost messages can leave both copies stopped indefinitely. This is intentional:
without a shared consensus/fencing authority, availability is sacrificed when
ownership cannot safely be established. A crashed source may resend the same
committed proof after authorized renewal; it may not invent a second transfer
or recreate a job from an empty database. Restored journals remain quarantined
pending independent reconciliation, as in the existing recovery tools.

Cancellation before source commit records an abort that permanently excludes
later commit for that job. The target stays inactive. Cleanup has separate
intent, physical removal, absence observation and claim-release boundaries.
Once source commit exists, automatic rollback is forbidden. A destination may
already have accepted the proof despite a lost response. Returning service to
the original host requires an independently reconciled reverse handoff that
accounts for any new destination writes; the retained source is a recovery copy,
not an automatically safe failback instance.

## Executable specification

`lab/migration_handoff_model.py` separates two local journals, an unreliable
message channel and selected physical effects. It has no production adapter.
`lab/test_migration_handoff_model.py` checks:

- Losing a committed handoff leaves the source fenced, including after reboot
  and authorization expiry; authorized resend can finish the same job.
- Crashes at each happy-path transition and duplicate messages preserve one
  running owner.
- Expiry cannot reverse accepted ownership or authorize deleting the new owner.
- A stale pre-copy cannot be accepted as the final stopped-data manifest.
- Another job, peer, VM UUID or staging disk cannot reuse the proof.
- Inactive staging cannot autostart; cleanup retains capacity until absence is
  observed and a tombstoned job cannot reserve again.
- Explicit pre-commit abort cannot be followed by a late commit.
- Thirty-two deterministic seeds each exercise eight independent fixtures at
  varied progress boundaries, with 64 actions per fixture (16,384 scheduled
  actions). They include reordered/dropped messages, expiry, renewal, crashes,
  reboots, retries and rejected transitions. Fixtures restart independently so
  an early abort does not turn the whole campaign into a terminal-state test.

Run from `pve-dev/`:

```sh
python3 -m unittest discover -s lab -p 'test_migration_handoff_model.py'
```

The fault model assumes honest controllers, uncompromised authentication keys
and enforcement of the local fencing contract. An authenticated peer's assertion
is not independent proof that an untrusted or compromised machine stopped a VM.

This is bounded testing of the proposed specification, not exhaustive verification
of the Rust controller, signatures, storage crash durability, arbitrary physical
effects or real Proxmox task behavior. Journal "crash" reloads are JSON round
trips; they do not simulate a torn filesystem write or old-backup activation.
Selected transitions are atomic abstractions and an actual adapter will require
finer-grained failure injection. No implementation may remove these assumptions
by merely translating the model's state names into production methods.
