# Safe resource-budget editing

The lab controller, Rust CLI and native GUI support administrator changes to an existing user's **positive resource policy**: storage budget, minimum/maximum memory admission budgets, configured core count and VM count. The explicit bounded `physical_trial` backend passed the workflow on disposable Proxmox and later on fresh test allocations on both Hetzner hosts; ordinary physical-host observation remains read-only. Existing production-user quotas were unchanged. See [the supervised trial](physical-trial.md).

A budget is an allocation contract, not guest filesystem utilization. A 750 GiB thick disk remains a 750 GiB allocation even if the guest stores only 60 GiB of files. Editing a policy never shrinks a zvol/filesystem, changes an existing VM's RAM/CPU configuration, removes data or moves a VM. CPU admission limits are not strict per-user runtime CPU scheduling.

## Accepted and refused changes

- Growth must fit the controller's host admission envelope, including unfinished policy changes, and the helper's current allocation space plus the pool safety floor. A physical trial uses its already reserved parent's `available` space without charging the same reservation twice against globally unreserved pool space. The helper rechecks capacity during execution and recovery.
- A contraction must still cover every nondeleted VM, including stopped machines, thick root and cloud-init commitments and metadata allowance, configured minimum/maximum memory, cores and VM count.
- An owner with pending or blocked operations cannot start a policy edit. Once an edit is accepted, new VM creates/actions for that owner are refused until it is verified. Helper checks enforce the freeze independently of GUI controls.
- The helper rechecks the actual owner dataset GUID, creation identity, ownership marker, storage mapping, VM identities/profiles/attachments, exact managed descendants and current ZFS usage. Unmanaged/missing descendants and thin root/cloud-init backing are refused. This is a conservative precondition; a sparse cloud-init volume is not silently accepted.
- Zero, unlimited values and removing an allocation through policy editing are unsupported. Separate empty-account deletion is documented in `user-deletion.md`; it requires all VMs to be explicitly deleted first. A zero input is never converted to ZFS `quota=none`, which removes the limit. Numeric `quota=0` was refused in the disposable real-ZFS probe and is not treated as a portable unlimited-value alias.

The deliberately small lab limits remain: disk 64–8192 MiB, minimum memory 128–1024 MiB, maximum memory at most 2048 MiB, 1–16 configured cores and 1–8 VMs. This is not a production-sized resource policy adapter.

### Storage floor

The controller's per-VM disk charge is root size plus 4 MiB cloud-init allowance plus the larger of 8 MiB or 5% root overhead. The helper reads actual root/cloud-init `refreservation` and `volsize` values and requires every volume to remain fully reserved.

For a storage-budget change, the execution-time lower bound is:

```text
actual_owner_ZFS_used
  + sum over managed VMs(max(controller_disk_charge - actual_VM_backing_reservations, 0))
```

The deficit is calculated per VM: an unusually large reservation inside VM A is not transferable unused metadata headroom for VM B. ZFS `used` already includes descendants, their reservations and snapshot/metadata growth, so this adds only the controller allowance not already physically backed. Simply checking `max(actual_used, controller_commitments)` would lose that allowance when snapshots consumed otherwise unused space.

A memory/CPU/count-only edit keeps its existing disk contract and does not impose new disk headroom. It still checks actual ZFS usage, thick backing, identities and logical commitments. This avoids freezing an otherwise valid owner solely because an unrelated limit changed; no storage claim is released or property rewritten in this case.

A disk reduction or unchanged-disk policy does not allocate extra space and may proceed while the pool is below its normal safety floor if all other checks pass. Genuine expansion requires both the additional reservation and the floor throughout the operation. ZFS independently enforces each quota/reservation assignment; a racing external change can cause a blocked outcome rather than speculative success.

These semantics follow [OpenZFS quota/reservation documentation](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Datasets/Quotas%20and%20Reservations.html) and [the `used` property definition](https://openzfs.github.io/openzfs-docs/man/v2.3/7/zfsprops.7.html). A separate scratch-dataset probe in the disposable Proxmox 9.2.26 lab confirmed the property ordering, unchanged GUID, below-used/below-reservation refusal, and inclusion of thick backing plus snapshot retention in parent `used`. See `evidence/remote-migrate-spike-20261110T211112-8d1502/real-zfs-quota-semantics.json`. The same two-node run verified thick root and cloud-init backing on both nodes.

The final V3 Rust build passed the complete selected workflow against actual disposable Proxmox/ZFS in `physical_trial` mode: 6→7 GiB growth with independently observed effect-before-acknowledgement/helper-SIGKILL recovery, 7→6 GiB contraction with an unchanged configured VM, below-commitment/stale/zero/member/foreign refusal, historical retries, CPU-only changes, VM lifecycle and empty account retirement. A 12 GiB prepaid parent admitted the 6 GiB member while only about 3 GiB remained globally unreserved. All exact identities/configurations, unrelated resource baselines and executed binary hashes passed, and QEMU stopped. See the complete report and [qualification details](quota-real-qualification.md).

The earlier generic-lab frozen run remains incomplete in `evidence/quota-partial-20261111/`: its fixed 300-second marker deadline expired without a conclusive effect/acknowledgement observation. It retains old-policy freeze and conservative claims; it supplies no recovery success claim. The current V3 report is a separate actual run with a full operation deadline and effect-present recovery on the empty owner.

## Request and durable transition

Read the current user from `dashboard`; an absent `policy_revision` is legacy revision zero. Submit the complete desired policy with that exact expected revision:

```json
{"action":"update_user_policy","request_id":"unique-request-id","user_id":"alice","expected_revision":0,"policy":{"budget_mib":256,"min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":4,"max_vms":4}}
```

Equivalent CLI:

```sh
pve-devctl --profile /absolute/lab-profile.json update-user-policy \
  --request-id unique-request-id --user-id alice --expected-revision 0 \
  --budget-mib 256 --min-memory-budget-mib 256 --max-memory-budget-mib 1024 \
  --max-cores 4 --max-vms 4
```

Acceptance commits the request ID, immutable previous/target policies, queued operation and owner freeze together before acknowledging. A retry with the same ID and exact payload returns the same operation; changing that payload conflicts. A stale policy revision, including an ABA change back to old numeric values, conflicts. Policy revision advances only after verified completion.

Until completion, host admission retains `max(previous,target)` for disk and minimum-memory capacity; the visible user policy remains the previous policy with `ready=false`. A blocked/uncertain edit retains those claims and the freeze. The helper independently keeps the durable current full resource policy; legacy revision-zero owners derive it from their immutable original `ensure_user` job.

The helper plan has four durable steps:

1. `policy_preflight`: verify identity, topology, commitments, actual usage and capacity.
2. `policy_first`: increase quota first for growth, or reduce reservation first for contraction.
3. `policy_second`: then increase reservation, or reduce quota, respectively.
4. `policy_commit`: verify the exact target pair again and atomically update helper policy revision/current policy/dataset identity with its verified step.

Each individual external property assignment has intent-before-effect, independent crash hooks and exact postcondition verification. Multi-property `zfs set` atomicity is not assumed. A same-boot intent with an absent effect and no authoritative completion proof blocks for investigation. A new boot permits supported deterministic reconciliation only against unchanged resource identities and the frozen old/target property values. Completed property steps are not replayed; recovery rechecks their current values and the full state. After helper proof, one controller transaction commits the target policy and removes the freeze. No capacity is released on an uncertain result.

An owner can remain frozen after a rejected execution-time preflight or a partial assignment because the existing operation model has no audited unblock/rollback endpoint. There is no automatic reset, no force switch and no supported manual database editing procedure. Preserve evidence and investigate. Blocked-operation reconciliation remains a production gate.

## GUI

The Users page offers **Edit…** for a ready account. The editor keeps the origin server/profile identity and expected policy revision through confirmation. Its local preview rejects limits below the visible VM commitments and explains why guest free space does not reduce thick allocations. The server remains authoritative and checks pending work and actual ZFS usage again.

A policy edit is a durable pending-ledger request. Read-only host routes, stale origins and unprivileged routes refuse submission before creating a ledger or starting a process. Retrying a policy edit requires a loaded administrator dashboard and the original identity-bound route. The Users grid is bounded to the detail viewport with horizontal overflow rather than expanding the split view. No new native GUI was launched for this change; debug/release compilation and automated tests cover this pass.

## Compatibility and qualification

Current binaries support schemas1–5, including explicitly scoped physical-trial schema4 and lab disk-shrink/graceful-shutdown schema5. The first accepted controller policy intent raises the catalog to at least schema2; the first helper policy job does the same before effects. Empty-account deletion requires at least schema3, and physical scope metadata requires schema4 throughout every transition. Owners retaining shrink recovery cannot edit their policy until explicit verified recovery removal; see `disk-shrink.md`. Earlier binaries refuse newer schemas rather than silently dropping authority metadata. Upgrade both controller and helper before using new commands; mixed-version execution is unsupported. See `user-deletion.md` and `physical-trial.md` for those separate operations and boundaries.

The optional boxed policy payload is wire-transparent and omitted from old jobs. Revision zero is likewise omitted from old user snapshots, preserving exact legacy frozen-job serialization/fingerprints. Pending legacy helper plans retain their old step sequence. Paired offline export/restore supports both schemas and preserves pending/verified policy operations; restored workers remain quarantined.

Tests cover unused contraction/growth, lifecycle after edits, zero/stale/below-commitment refusal, owner freezing, concurrent admission, actual usage/unmanaged/GUID drift, additive and per-VM metadata floors, partial contraction, low-space contraction, a 24-case property/commit crash matrix, legacy serialization/plan recovery, schema2 paired restoration and GUI wire/draft/read-only/admin checks. Independent adversarial tests additionally cover mixed-direction policies and stale helper payloads. See `evidence/quota-editing-20261111/summary.json` and `quota-state-review.md`.

This remains bounded evidence. The complete selected Rust operation passed on disposable real Proxmox using a stopped configured VM and a blank disk; it supplies no guest OS boot or cross-host iroh claim. Actual bounded physical-host trials, physical power-loss testing, concurrent external-admin coordination, blocked-state reconciliation and production adoption remain separate requirements.
