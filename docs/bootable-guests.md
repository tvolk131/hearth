# Bootable development guests (disposable lab only)

The existing `create` plan remains a blank-disk fixture. An optional `guest` specification selects a separate, versioned-by-kind `create_guest` plan. It imports a pinned Debian cloud image into the owner's already-reserved, thick root zvol before attaching that disk. Nothing here installs a controller on Hetzner or allows a production write backend.

## Client interface

The administrator must include each approved digest in the daemon's immutable environment binding at initialization (`pve-devd init ... --guest-image-sha256 SHA256`, repeatable), as well as in the helper allowlist below. `pve-devctl --profile lab.json images` lists the admitted digests. An unknown digest is rejected before a VMID or budget is allocated. Updating this binding is not an online image-management feature: use new disposable lab state until an audited migration path exists.

```sh
pve-devctl --profile lab.json create-user \
  --request-id labdev-user-1 --user-id labdev --budget-mib 5120 \
  --min-memory-budget-mib 512 --max-memory-budget-mib 1024 \
  --max-cores 4 --max-vms 1

pve-devctl --profile lab.json vm create \
  --request-id labdev-vm-1 --owner labdev --name dev-lab \
  --disk-mib 4096 --memory-mib 1024 --min-memory-mib 512 --cores 4 \
  --guest-image-sha256 APPROVED_IMAGE_SHA256 \
  --ssh-public-key-file owner.pub
```

Account storage must first reach `verified`; VM creation is asynchronous. Wait for its operation to verify before `vm start`. Reuse the exact request ID and payload when a reply is lost. The GUI's existing create form still creates blank-disk fixtures; guest creation currently uses the CLI or typed JSON RPC.

The RPC `create_vm` action takes an optional field:

```json
{"guest":{"image_sha256":"64 lowercase hex characters","ssh_public_key":"ssh-ed25519 BASE64_WITHOUT_COMMENT"}}
```

The server freezes both fields with the accepted VM/operation. A changed key or image with the same request ID conflicts. Only canonical Ed25519 public keys are accepted; private keys are never sent to the daemon or helper. The friendly CLI accepts a normal trailing newline in a `.pub` file and removes public-key comments before submission. Omitting `guest` preserves the old wire encoding and plan.

## Image trust and resource policy

The root helper's lab JSON has an optional root-admin-controlled allowlist:

```json
{"environment":"32 lowercase hex characters","pool_guid":"decimal ZFS GUID","images":[{"path":"/var/lib/pve-dev-images/SHA256.qcow2","sha256":"SHA256"}]}
```

Replace both occurrences of `SHA256` with the same complete digest. Images must be single-link regular files, read-only, at most 1 GiB on disk, stored at that exact content-addressed path, with root-owned, non-group/world-writable ancestry and no symlinks. The helper verifies SHA-256 and qcow2 metadata before creating a VM and again before import. It rejects external backing/data files, encryption/corruption flags, and virtual sizes larger than the admitted root disk. It never downloads an image itself. Root and its supplied image allowlist are trusted; this is not a defense against a hostile administrator modifying files during an operation.

The rerunnable harness starts from the same immutable, versioned Debian URL and pinned SHA-512 as the outer lab image (`lab/inputs.json`) and independently verifies that checksum. It then builds a **lab-only derivative**, not an unchanged upstream image, with `lab/prepare_guest_image.py`. Copying/importing a mutable `latest` image is not supported.

The preparation script first checks the disposable hostname, environment nonce and pool GUID. It copies the read-only, hash-checked parent to an exclusive staging file, refuses an occupied NBD device, and edits only the unmounted copy with `debugfs`. Recipe `pve-dev-lab-boot-policy/2` has two intended filesystem changes. `/etc/systemd/system.conf.d/90-pve-dev-lab.conf` extends boot time budgets for nested software emulation:

```ini
[Manager]
DefaultTimeoutStartSec=600s
DefaultDeviceTimeoutSec=600s
```

`/etc/cloud/cloud.cfg.d/90-pve-dev-lab.cfg` selects the expected SSH username through cloud-init's supported default-user mapping:

```yaml
system_info:
  default_user:
    name: dev
```

This does not skip EFI mounts, disable services, suppress warnings, allow degraded systemd/cloud-init, or bypass cloud-init. Filesystem and qcow2 checks, exact readback of both drop-ins, and a second parent hash check precede publication of the read-only, content-addressed result. Both helper and controller pin the **derived image's SHA-256**. `guest-image-build.json` records the parent SHA-512/SHA-256, derived SHA-256, recipe, default username and both drop-in hashes. Filesystem metadata can vary between builds, so this is a repeatable recipe with a per-run immutable pin, not a bit-reproducible-build claim. Nothing modifies the upstream parent or an existing guest.

All mutations still require the lab hostname, environment nonce, pool GUID and marker, and VMIDs 9001–9099. To fit a real OS, lab host disk admission can be configured up to 16 GiB and owner budgets up to 8 GiB. Blank fixtures retain their 512 MiB disk/RAM limits. Image guests allow root disks up to 8 GiB and RAM up to 2 GiB; this remains deliberately incompatible with production-sized allocations. The whole thick root disk, cloud-init allocation and overhead allowance count against the same owner budget, including pending/blocked work.

## Durable creation and recovery

```text
preflight → VM → thick volume → image → attach → cloud-init → verify
```

- Preflight checks owner storage, VMID/volume collisions and the trusted source image without touching a VM.
- The `image` step durably records intent before running `qemu-img convert -n` into the exact journal-owned root zvol. The target must be stopped, unattached, and match its previously recorded GUID/creation transaction.
- Before conversion the helper settles udev and waits a bounded time for the exact zvol block device. After conversion succeeds, it flushes block writes and runs `qemu-img compare` against the pinned source image and raw target. Only then does it write `org.pve-dev:image=SHA256` on that zvol and sync the pool before recording completion in redb.
- An interrupted same-boot import without a completion marker blocks for investigation. A matching marker permits reconciliation without re-import. A confirmed new host boot can retry a missing-marker import only while the disk is still journal-owned, stopped and unattached. It cannot overwrite a disk already attached to a guest.
- Later verification checks the disk identity and **image provenance marker**, not a hash of mutable guest filesystem contents. An OS is expected to change its own disk after boot.
- Guest deletion uses the existing verified stop/unprotect/destroy/release sequence, retains the VMID tombstone, and preserves the owner's dataset/reservation.

The image marker is helper-controlled evidence of a completed import; it is not attestation of an existing arbitrary VM or permission to adopt it.

## Cloud-init and network

Guests use a deliberately isolated, harness-created bridge `vmbr77`, gateway `10.77.0.1/24`, and a deterministic address `10.77.0.(VMID - 9000 + 10)`. VMID tombstones prevent controller address reuse. The helper does not create host bridges, NAT rules, public IPs or production network configuration. External administrators must not concurrently provision conflicting addresses in this subnet.

Proxmox generates the persistent NoCloud drive from `citype=nocloud`, the owner's URL-encoded public SSH key, static `ipconfig0`, and the VM name. Approved images must provide `dev` as their cloud-init default user. The helper deliberately leaves `ciuser` unset: PVE 9.2.26 otherwise emits the deprecated string form `user: dev`, which cloud-init 25.1.4 reports as `degraded done`. The prepared image's supported default-user mapping avoids that deprecated input without suppressing the diagnostic or relaxing readiness. Adding even `ciuser=dev` later is unpinned drift and blocks a start.

`ciupgrade=0` explicitly disables Proxmox's default first-boot package upgrade, keeping this offline test independent of live package mirrors. DNS is pinned to `10.77.0.1` and the search domain to `lab.invalid`; the isolated lab does not supply internet access. Guest VMs also have a deterministic virtio MAC and serial console. The helper verifies these settings on later actions and rejects additional unpinned `ci*`, `ipconfig*`, DNS or search-domain options. Changed owner keys, bridge settings or disk provenance block rather than get silently repaired. No password is provisioned. The owner account can sudo inside its own VM; this does not confer Proxmox access.

Creation leaves the guest **stopped**. A verified `start` means Proxmox reported the QEMU VM running, not that cloud-init or SSH is ready. OS readiness is currently an explicit lab test, not a production readiness service. The base OS does not automatically install Rust, T3, Codex or coworker-specific software.

## Rerunnable real test

```sh
nix develop ./nix --command bash scripts/build-lab-binaries.sh
nix develop ./nix --command python3 lab/lab.py guest-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 2400
```

After authenticated bootstrap, this imports the pinned image into a 4 GiB thick zvol inside an 8 GiB mirrored virtual pool, independently compares the source image to the raw zvol before start, starts a real nested guest with four vCPUs and 1 GiB RAM, waits for clean cloud-init completion and owner SSH, refuses a different SSH key, checks owner sudo access and healthy systemd, then stops/deletes the VM and checks for leftover disks. It subsequently runs the remaining baseline controller checks, including the final exact catalog/VM/zvol inventory assertion. Thus the complete run still requires all eight baseline checks and all three guest checks; only their order changed to expose boot failures earlier.

A passive serial collector starts before the guest, never sends console input, limits captured output to 2 MiB and is terminated with its own SSH child after readiness or failure. A kernel panic, initramfs emergency shell, collector failure, cloud-init error or degraded result fails rather than being retried until timeout. The outer hypervisor, pool, bridge, keys, services and nested guest are all disposable local lab resources.

The combined suite reserves minimum-memory budgets for three 256 MiB baseline owners and a 512 MiB guest owner. The harness explicitly initializes its admission capacity to 1536 MiB; the blank-only suite retains 1024 MiB. A regression test checks that the selected suite's owner budgets fit both disk and minimum-memory capacity. The first combined attempt passed all eight baseline checks but correctly refused the guest owner under the old 1024 MiB limit; it remains failed evidence, not a guest-boot success.

The next single-threaded attempt passed the baseline and image comparison but reached an initramfs emergency shell. Its read-only diagnostics found the correct root PARTUUID, with disk discovery only around kernel timestamp 696 seconds under nested software emulation. Diagnostic console input was explicitly recorded, and this attempt is not unattended-boot evidence. Controller/guest runs now use multithreaded outer TCG with four emulated CPUs; base-image builds and the legacy matrix retain single-threaded TCG. An earlier multithreaded host reboot panicked, so host reboot reliability is not inferred from these no-reboot controller tests, and kernel panics still fail without resets/retries.

A one-vCPU nested guest under multithreaded outer emulation subsequently reached systemd unattended, but critical services repeatedly timed out and SSH did not become reachable within the 2400-second readiness deadline. A four-vCPU attempt then mounted root but missed the early EFI-device deadline and entered systemd emergency mode without console intervention. Both remain failed evidence. The fixture now grants its guest four virtual CPUs and uses the explicitly identified derivative above. The post-boot check additionally requires `systemctl is-system-running --wait` to succeed; cloud-init/SSH alone must not disguise a degraded OS. All such reruns use fresh lab identities and disks, not retries of uncertain mutable production operations.

The first timeout-only derivative booted unattended, mounted EFI, and completed cloud-init. A longer read-only SSH diagnostic confirmed owner authentication and healthy systemd, but cloud-init reported the Proxmox-generated `user` deprecation as `degraded done`. That attempt was interrupted and remains incomplete, not a passing guest suite. Its diagnostic evidence is retained under `.lab/run-controller-20261109T231711-9a1c4a/evidence/`. This led to the supported default-user mapping in recipe v2. SSH command probes now have a 180-second bound (also bounded by the remaining overall readiness deadline) because the successful diagnostic took longer than the previous 60-second probe; a slow read-only query must not conceal its eventual error status. No diagnostic sent guest-console input or changed guest configuration.

Inner SSH uses lab-only trust-on-first-use through an independently pinned outer SSH connection and rejects subsequent host-key changes. This is not a production SSH-host-key enrollment design. Readiness polls only read guest state; a failed mutable operation is not speculatively retried. Neither a running QEMU process nor a test timeout counts as a boot success. Evidence includes `nested_guest_boot_verified`, cloud-init/SSH details and ZFS backing properties, never private keys or disk images.

The GitHub real-Proxmox job invokes this test on PRs; repository publication and hosted-runner timing/capacity remain separate validation steps. The separate legacy full crash matrix remains configured as well. This feature does not establish real-host mid-copy kill recovery, production network isolation, toolchain provisioning, backup/restore, schema migrations or production write readiness. Image metadata rejection has unit coverage, but real-host negative tests for every image hash/ownership/symlink/permission failure remain to be added; a passing import exercises those guards only on their success path.

## Verified November 10 result

The fresh run `real-controller-20261110T000212-44dcfa` passed **all eleven combined checks** against Proxmox VE **9.2.26**, with `complete: true` and `nested_guest_boot_verified: true`. Its measured suite time was **3,576.5 seconds** (about 60 minutes) on this ARM Mac under multithreaded x86 TCG, excluding final diagnostics/shutdown. Evidence is in `evidence/real-controller-20261110T000212-44dcfa/`. The driver exited successfully, no cleanup error was recorded, and a final local process check found no lab QEMU, demo daemon/helper or guest SSH probes remaining.

- `guest-image-build.json` identifies recipe v2, the unchanged checksum-pinned parent, both drop-ins and the independently pinned derivative. `guest-image-comparison.txt` confirms the pre-boot source/zvol comparison; its size-mismatch warning is expected because the 3 GiB source is zero-extended into a 4 GiB target.
- `guest-readiness.json` records `status: done`, `extended_status: done`, empty errors and recoverable errors in every cloud-init stage, successful owner SSH, foreign-key refusal and healthy systemd. `guest-health.txt` independently records the `dev` account, mounted ext4 root, expanded filesystem, passwordless owner sudo and systemd `running`.
- The passive serial log was captured without console input. No extra manual SSH probes, guest configuration changes or emergency-mode continuation were used in this passing run.
- Verified stop/delete removed both guest zvols. The remaining baseline checks also passed: shared owner budgets including pending claims, cross-tenant refusals, capacity release, daemon SIGKILL recovery, real ZFS quota refusal and live device revocation. The final physical VM/zvol inventory exactly matched the catalog, with every retained VM stopped.

The failed/incomplete attempts above remain failures; none is substituted for this clean run. A passing lab-only derivative is not evidence that an unchanged arbitrary cloud image, a production network, or the Hetzner guests were tested or modified.
