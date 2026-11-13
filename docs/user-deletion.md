# Delete an empty controller account

`delete_user` removes an **empty member account's managed allocation scaffolding**. It never cascades into VM deletion. Existing VMs must be deleted explicitly through their normal identity-checked operations and reach verified deletion first. Controller accounts and enrolled iroh devices are separate from Linux accounts, guest users and Proxmox login accounts; this command does not delete those logins.

## Preconditions and acceptance

Only an authenticated administrator may request deletion. Self-deletion and deletion of administrator accounts are refused. The request includes the exact current policy revision; missing users, stale revisions, inactive/not-ready accounts, nondeleted VMs and any pending/blocked owner operation refuse acceptance. A stopped VM still exists and prevents account deletion.

```json
{"action":"delete_user","request_id":"unique-request-id","user_id":"alice","expected_revision":2}
```

```sh
pve-devctl --profile /absolute/lab-profile.json delete-user \
  --request-id unique-request-id --user-id alice --expected-revision 2
```

Acceptance atomically records a durable operation with the frozen current user/policy, sets `state=deleting` and `ready=false`, and revokes every enrolled device belonging to that user. New enrollment and VM/policy actions for the inactive owner are refused. Authentication also independently rejects inactive accounts, even if an inconsistent device record remained. A lost response is retried with the same request ID and exact payload.

Disk and minimum-memory admission commitments remain charged throughout pending or blocked deletion. Only verified helper completion changes the controller account to `state=deleted` and releases those commitments. The user record, device revocations, operations, request IDs and VM tombstones remain. A fresh request cannot reuse the deleted username. Replaying an earlier successful account-creation request returns historical proof, not a new account or reactivation.

## Independently verified helper cleanup

The helper validates current policy/revision and its immutable original `ensure_user` role proof; a forged member payload cannot authorize deleting the administrator's allocation. It independently refuses any unfinished owner job or any nondeleted machine in its journal.

The durable steps are:

1. `owner_preflight`: verify the allocation is empty, then record a helper deletion fence.
2. `owner_storage`: remove only the exact recorded empty Proxmox storage entry.
3. `owner_dataset`: remove only the exact recorded empty owner filesystem using nonrecursive `zfs destroy DATASET`.
4. `owner_release`: verify both are still absent and atomically retain the helper's deleted-account tombstone.

Every boundary checks shared control identity, owner dataset GUID/creation/ownership/current policy, storage binding, child datasets, snapshots/bookmarks, guest inventory and both current and pending-effective configurations, historical VMIDs, storage contents and pool references. Embedded storage references also block cleanup. Another `zfspool` storage ID pointing to the exact owner dataset or a descendant is refused even after the managed entry has been removed; ancestor pools remain allowed. A foreign/missing/replaced resource blocks cleanup; it is neither adopted nor overwritten. Existing Proxmox pools are not managed by `ensure_user`, so this command does not delete them. A pool still referring to the storage must be detached explicitly before cleanup.

### Filesystem contents must be proven empty

No byte-size threshold is used: an empty ZFS filesystem has metadata, while a small nonempty filesystem can contain real data. Nonrecursive `zfs destroy` alone is also insufficient because it can remove files inside a childless filesystem.

The real backend requires `mountpoint=none`, an unmounted owner filesystem, no descendants and no snapshots/bookmarks. It creates a fresh private probe directory under the helper state directory, mounts the exact filesystem with fixed `ro,zfsutil` options, and uses `findmnt` JSON to attest source, target, ZFS type and read-only options. It examines only the root directory: any file or directory other than virtual `.zfs` refuses deletion. Inspection stops on the first nonvirtual entry and does not read file contents. A successful probe is unmounted and its directory removed before cleanup continues. The check repeats immediately before destructive effects.

Unsupported mount behavior, failed read-only attestation, unmount failure or a leftover inspection path after a crash blocks cleanup and retains the allocation/claims. There is no speculative forced unmount or recursive cleanup. The exact primitive passed on disposable real ZFS: an empty dataset was accepted, a known file was refused and preserved, a write-open returned EROFS, and identities/properties remained unchanged after unmount. See the primitive result. The complete final V3 Rust workflow also passed in the explicit `physical_trial` backend on disposable Proxmox VE 9.2.26, including actual read-only inspection, storage/dataset removal, verified tombstones, revocation and capacity release. See the complete mode report. An unsupported probe can never silently fall back to destroying the dataset.

This assumes no simultaneous external administrator modifies the allocation between its final check and effect. Such check/effect races remain an explicit production boundary. The helper also still retains its lab/namespace/environment guards; account deletion does not authorize adoption of unrelated infrastructure.

## Recovery and compatibility

Each storage/filesystem removal has durable intent before its effect, exact absence verification afterward and independent crash hooks. A same-boot intent with no completion proof while the resource remains blocks for investigation. On a new boot, supported deterministic reconciliation rechecks all identities and emptiness before retrying. Actual absence following an existing intent can be verified without another removal effect. Reappearing scaffolding, changed identity, new files or references block recovery; uncertainty never releases capacity or recreates a resource.

An accepted deletion can remain frozen with credentials revoked after a preflight refusal or partial cleanup. There is no automatic cancellation, re-enrollment, force-unblock or rollback command. Preserve journals and inspection evidence for investigation; deleting databases or removing a quarantine marker is not a recovery procedure.

`state=active` is the backward-compatible default and is omitted from legacy snapshots, preserving old frozen job bytes/fingerprints. The first accepted deletion raises controller schema to3; its helper job raises journal schema to3 before effects. Current binaries accept schemas1–5; schema4 persists the immutable physical-trial binding and schema5 adds lab disk-shrink recovery holds and graceful shutdown. Owners retaining shrink recovery cannot be deleted. Older binaries refuse unsupported edited state. Upgrade both components before using deletion.

Offline paired backup/restore preserves pending and completed deletion, revocations and tombstones. Validation requires inactive users to remain frozen/revoked and deleted users to have a matching verified helper/controller deletion proof. Restored workers remain quarantined.

## Validation boundary

The 15 `tests/user_deletion.rs` tests cover empty-member completion, immediate revocation/freeze, capacity held until proof, no name reuse, required explicit VM deletion, member/admin/self/stale refusal, forged administrator-role refusal at the helper, files/snapshots/foreign children/mount/storage/GUID drift, current/pending/embedded guest references, exact/descendant storage aliases before and after entry removal, allowed ancestor pools, immutable provenance, post-policy-update deletion, and a 16-case crash matrix. Paired schema-3 backup tests preserve queued and completed deletion under quarantine. The integrated final source passed all 167 Rust tests, formatting and Clippy with warnings denied.

The passing disposable mode run first completed quota edits and explicit VM start/stop/deletion, then retired the empty member through the actual portable CLI/controller/helper. It independently verified dataset/storage absence, denied the revoked member, reopened the original schema-4 scope, inspected stopped ledgers, and compared unmanaged VM/dataset/global configuration with exact baselines. All executed binary hashes matched the frozen V3 bundle and the owned emulator was stopped. That disposable run contacted no physical Hetzner host.

The later supervised two-host trial also verified account-with-VM refusal, normal explicit source VM deletion, empty-member retirement on both hosts, revoked-member refusal and complete scaffold removal. Native migration required a supervised cloud-init metadata repair before normal Rust cleanup could continue. Automatic trial reruns remain disabled. Physical power loss, every real deletion crash boundary, external-admin coordination and generic blocked-state reconciliation remain separate qualification boundaries.
