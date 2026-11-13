# Bounded physical-host trial backend

`physical_trial` is an explicit backend for a small, separately owned test allocation on a physical Proxmox host. It does not adopt existing VMs, storage entries or owner datasets. The ordinary `lab` and `simulated` paths retain their original names and serialized defaults.

The root-owned private trial config pins the actual node, pool GUID, exact `/etc/machine-id` file SHA-256, and a precreated root dataset's GUID. That root must be named `<pool>/pve-dev-trial-<environment>`, have the matching `org.pve-dev:trial` marker, `mountpoint=none`, and equal quota/reservation of 8–16 GiB. The helper checks those actual identities and limits at startup and physical operation boundaries. It never changes the hostname, lab ID file or pool-wide marker to make a physical machine resemble the disposable lab.

All managed resources lie under `<trial-root>/control`, which must be absent when a fresh helper journal starts. Only `trial-admin` and `trial-user` are supported; their storage IDs use the environment-specific `trial-<nonce-prefix>-` prefix. At most four explicitly listed VM IDs in 9001–9099 can be allocated. Both controller admission and the helper independently enforce these bounds, including loaded journal/catalog history. Existing guests and allocations remain unmanaged.

Physical trials initialize catalog and helper schema 4. Older binaries reject that schema; policy edits and account deletion retain the schema-4 fence. Legacy backends keep schemas 1–3 and omit the new optional binding field, preserving default binding JSON. Upgrade controller and helper together.

## Startup and transport

The explicit entry points are:

```sh
pve-dev-host serve --state-dir /absolute/helper-state \
  --socket /absolute/helper.sock --physical-trial-config /absolute/scope.json
pve-devd init --data-dir /absolute/controller-state --admin ADMIN_ENDPOINT \
  --environment ENVIRONMENT --host-socket /absolute/helper.sock \
  --backend physical_trial --physical-trial-config /absolute/scope.json \
  --disk-capacity-mib CONFIGURED_ROOT_CAP_MIB --network loopback
```

Controller capacity must match the immutable parent cap. The helper config and ancestry must be root-owned, without writable group/world permissions or symlink aliases; the config is private and single-link. The root operator initializes the controller, then gives its dedicated service user ownership of controller state before starting the unprivileged daemon.

The parent's reservation is already a storage grant. New child reservations and owner growth use that parent's actual `available` capacity, while independently retaining the global pool safety floor; they do not require the same reserved bytes again from globally unreserved pool space. Nonallocating reductions keep the existing low-space behavior. This distinction follows OpenZFS's [dataset available/reservation accounting](https://openzfs.github.io/openzfs-docs/man/v2.3/7/zfsprops.7.html). The actual mode run exercised a 12 GiB parent on a 16 GiB mirror: a 6 GiB child was admitted with about 3 GiB globally unreserved but over 11 GiB available inside the prepaid parent.

Physical-mode iroh is restricted to host loopback. An operator may invoke the portable CLI over pinned SSH; this does not create a public UDP listener or require a firewall change. Startup health includes the helper's complete backend/scope binding. Every new controller submission carries the already attested helper binding, which the helper checks before persisting the job. A same-environment Unix socket pointing at another backend, pool or scope cannot silently redirect work.

## Read-only operator evidence

`pve-devd inspect-offline --data-dir PATH --expected-server ID --expected-environment ENV` opens existing stopped controller state and verifies the public identity pins. It prints the server ID, binding, users, VM state/profiles without guest keys, and public operation records. It neither initializes missing state nor sends network requests, and emits no private keys or frozen job payloads.

`pve-dev-host provenance --socket PATH` reads the running helper's durable journal. Its `resources` object contains the full binding; dataset and storage identity maps; machine records with their recorded VM/disk identities; and `unsettled_jobs`, a map of every nonverified operation ID to its status. Dataset/storage identities are historical journal proofs, not adoption of current API objects. Machine profiles contain only public guest keys where applicable. No private key material is returned.

An operator must stop controller admission before checking the offline ledger, require no unsettled helper jobs, stop the helper, and independently compare current resources with the captured provenance before any external cleanup or supervised copy. PVE storage entries have no native GUID; their exact registered configuration plus the pinned parent dataset is the available provenance boundary. These commands do not add a migration/adoption or force-cleanup API.

Paired export/restore validates matching physical scopes and preserves schema 4, but restored workers remain quarantined. There is no supported restored-state activation command.

## Qualification boundary

Focused tests cover owner/VM allowlists, independent helper role checks, changed identity/cap pins, exact legacy JSON, path/task-node selection, backend-bound submission before effects, loopback enforcement, unchanged offline inspection, and scope-preserving quarantined restoration. Empty-owner cleanup now checks current and pending-effective guest configurations before removing an allocation.

Cleanup also rejects embedded guest storage references and independent `zfspool` storage IDs pointing to the owner's exact dataset or a descendant, including recovery after its managed storage entry has gone. Ancestor pools remain valid. This preserves dependencies despite [Proxmox's supported storage aliasing](https://github.com/proxmox/pve-docs/blob/master/pvesm.adoc). Physical VM destruction uses `purge=0` and `destroy-unreferenced-disks=0`; the legacy lab default is unchanged.

The exact `ro,zfsutil` empty-filesystem primitive passed on a separate fresh scratch filesystem in the disposable Proxmox VM. Empty and nonempty inspections preserved GUID/creation/properties, writes were refused with `EROFS`, and the known scratch allocation was cleaned up. See `evidence/empty-filesystem-primitive-20261111/result.json`. This primitive result is not full physical-backend or Rust account-deletion qualification.

The complete physical-mode disposable run **passed** on Proxmox VE 9.2.26 using the final V3 source and all three portable Linux binaries. See the complete report and [quota qualification details](quota-real-qualification.md). Actual checks covered effect-present growth recovery after helper SIGKILL, configured-VM contraction and refusals, CPU-only editing, nonpurging VM lifecycle, read-only verified empty account deletion, root-marker/GUID drift refusal, schema-4 reopen, stopped catalog/provenance and exact inventories. The unmanaged VM, dataset and global configuration remained unchanged, original lab identity/marker stayed unchanged, all runtime artifact hashes matched and the owned QEMU process was stopped.

This qualifies the explicit backend inside a disposable Proxmox VM. Its blank nested VM supplied no guest OS boot claim; iroh was host-local over pinned lab SSH. No physical Hetzner host was contacted. The physical driver still requires exact matching qualified artifacts and its independent guest/migration gates before any physical host contact. Existing production-state adoption, physical power-loss behavior, generic blocked-operation reconciliation and concurrent external-admin coordination remain outside this bounded trial.

The standalone qualifier is `lab/physical_trial_suite.py --linux-bin-dir PATH --timeout 1800`, run in the Nix shell. It requires a sealed matching cached base and a frozen three-binary/source manifest; it never builds a base. Its direct Python entry point is `run(linux_bin_dir, timeout)` for the fixed-task lab coordinator. The standalone CLI holds the shared lab lock; coordinated calls rely on the coordinator's validated inherited lock ownership. Neither entry point accepts a physical SSH host. The fixture uses a fresh 16 GiB mirror with a 12 GiB trial parent, an unmanaged VM/dataset/global-config baseline, effect-present growth recovery on an empty user, then a small blank VM for in-use refusal, contraction, configuration preservation, lifecycle and empty account retirement. A successful report must include each required boolean plus exact hashes of all three executed portable binaries.
