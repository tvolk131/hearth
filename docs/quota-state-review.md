# Independent quota transition review

This review covers the lab/simulated quota-edit implementation while it was
being built, including the controller/helper transitions, schema and maintenance
interaction, and GUI submission/trust boundaries. It did not contact a physical
host or launch/change an app. No implementation files were edited by this review.

Result: **no additional unaddressed concrete blocker was found in the reviewed
paths.** This is a focused source review and four independent regressions, not
proof of airtightness or a real-ZFS qualification.

## Review observations

- Controller acceptance authenticates the actor inside the catalog transaction,
  requires administrator permission and the expected current policy revision,
  refuses unsettled owners and freezes the owner before acknowledgement.
- Pending/blocked changes hold the maximum of old and target disk/minimum-RAM
  commitments independently. A disk reduction cannot finance a new allocation
  before verification, and simultaneous RAM growth cannot be ignored.
- The helper independently retains/checks the full current resource policy as
  well as the storage budget and revision. Legacy revision-zero policy is read
  from the referenced immutable initial `ensure_user` job. A new stale or forged
  job is distinct from an idempotent retry of already completed historical work.
- Quota and reservation changes are separate durable steps with exact old/target
  preconditions. Recovery revalidates owner, VM, disk and topology identities.
  Helper policy commit and its verified marker are one journal transaction;
  interrupted completion may resume against that already committed target only
  when its own commit marker proves it.
- Controller policy changes and admission release occur only after helper
  verification. An uncertain or drifted edit keeps the old catalog policy and
  conservative holds; it does not silently restore a ready owner.
- The first accepted controller/helper policy operation upgrades that journal
  to schema 2. Older binaries reject that schema. New binaries still accept
  legacy state, and zero/absent added fields preserve historical job payloads.
- Paired backup validation accepts both schema versions while retaining the
  existing fresh-target, matching-payload and mandatory restore-quarantine
  boundaries. It does not implement restored-state activation.
- The GUI captures a current user policy/revision and controller origin, rejects
  limits below known commitments locally, and relies on server-side validation
  for authoritative current usage. Read-only observation entries cannot produce
  controller RPC configuration or submit a mutation.

The parent review identified an initial insufficient `max(actual used, modeled
disk charge)` floor. The implementation now adds per-VM unbacked metadata
allowances to actual ZFS usage, with thick backing/provenance checks. That issue
was already tracked by the parent and is not counted as another finding here.
During implementation, the author also strengthened full-policy equality in
helper history; the independent same-revision forged-payload test below guards
that boundary.

## Independent regression evidence

All four tests in `tests/quota_adversarial.rs` passed against the integrated
implementation:

1. A mixed disk contraction and minimum-RAM expansion keeps both conservative
   admission envelopes. Separate competing requests prove each bound matters;
   the disk capacity becomes available only after helper/controller verification.
2. After the first physical contraction effect, a reboot and same-name disk-GUID
   replacement block the second property update. Quota remains at the old value,
   the owner stays frozen and its old catalog budget remains charged.
3. A nonstorage policy change with unchanged disk budget rejects a newly
   submitted old-revision lifecycle job without starting the VM, while an
   already completed original create remains an idempotent historical retry.
4. A forged previous nonstorage policy at the same storage budget/revision is
   blocked by helper history, without changing dataset properties or revision.

```sh
cargo test --locked --test quota_adversarial -- --nocapture
cargo clippy --locked --test quota_adversarial -- -D warnings
rustfmt --edition 2024 --check tests/quota_adversarial.rs
```

These checks complement the implementation's own quota crash/accounting tests;
they do not replace the parent's aggregate suite. Focused Clippy and the own-file
format check passed. An initial Clippy large-enum warning caused by the added
policy payload was addressed by the implementation author by boxing the optional
payload, preserving its JSON representation; all four checks passed again afterward.
Compact results are in `evidence/quota-state-review-20261111/summary.json`.

### Parent integration follow-up

The parent added a fifth regression after this independent review: an unchanged
disk budget with a full logical allocation and additional small actual metadata
usage could cause a CPU-only edit to block and freeze the owner. The baseline
failed with the disk-headroom preflight error. The parent limited the extra
unbacked-margin admission to actual disk-budget changes; actual usage, thick
backing and logical commitments remain checked for every edit. All five
adversarial tests and all twelve quota tests pass after that fix, with unchanged
dataset properties in the CPU-only case. This follow-up was tested by the parent,
not independently re-reviewed.

## Limits retained

- No new real-Proxmox quota mutation was performed by this review. The simulated
  external-effect model is narrower than an actual ZFS/PVE host and power loss.
- Identity checks and property updates are separate operations. Simultaneous
  external administrative changes still need an operational ownership/locking
  policy and cannot be excluded by this review.
- Same-boot ambiguous intents or unexpected drift block conservatively; a
  generic unblock/reconciliation mechanism remains unavailable. The owner can
  consequently remain frozen pending investigation.
- The pool safety floor applies to genuine expansion; nonallocating reductions
  may improve a low-space pool while retaining the other identity/usage checks.
- Positive budgets only: zero/unlimited values, allocation/account removal,
  guest disk shrink and automatic deletion remain unsupported. CPU limits are
  allocation admission, not a strict per-person CPU scheduler.

No finite review establishes that all possible transitions are safe. The evidence
here is specific to the reviewed implementation and reproducible checks above.
