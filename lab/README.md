# Disposable real Proxmox integration lab

This is an **experimental integration adapter**, not permission to apply the production specification. It accepts no remote host, production SSH configuration, host block devices, or production credentials.

## Run

From `pve-dev/`:

```sh
nix develop ./nix --command bash scripts/check.sh
nix develop ./nix --command python3 lab/lab.py build
nix develop ./nix --command python3 lab/lab.py test --smoke
nix develop ./nix --command python3 lab/lab.py test
```

The dedicated `nix/` flake keeps large images and sockets out of Nix's source snapshot even before this becomes a Git repository. Its lockfile pins Nixpkgs, including QEMU. No Homebrew or permanent user-profile installation is needed.

The first build downloads a fixed Debian 13 amd64 cloud image, verifies SHA-512, and installs the real Proxmox kernel and packages. The signing key is downloaded with HTTPS and checked against its pinned SHA-256. Proxmox's documented HTTP apt repository still requires valid apt signatures. Apt repositories are currently rolling inputs: installed package versions are recorded, **not fully pinned**. A future trusted, checksum-pinned base artifact should remove this remaining source of variation.

An interrupted base build can resume after a corrected bootstrap script. An unchanged failed bootstrap is reported, not retried speculatively. A successful build is shut down and reused as a read-only backing image. Changed bootstrap/image inputs refuse reuse of a sealed base; preserve or explicitly remove `.lab/base` before rebuilding. Do not change a base while retained run overlays depend on it.

Each legacy test gets a fresh root overlay, two new 4 GiB virtual data disks, environment nonce, and redb operation journal. The data disks form a real ZFS mirror. The outer VM uses four emulated x86 CPUs and 4 GiB RAM, with TCG rather than nested KVM. Base-image builds and the legacy reboot/crash matrix retain single-threaded TCG: a prior multithreaded run panicked during a reboot. Controller/guest runs instead use multithreaded TCG to reduce nested boot delays and do not claim host reboot reliability. All modes fail promptly on a kernel panic instead of waiting for SSH or resetting automatically. The outer root is ext4, **not boot-from-ZFS**; this does not test production boot parity or mirror replacement.

After initial host provisioning, the harness disables cloud-init on the outer host before creating guest volumes. Otherwise, a later host boot can discover a guest's NoCloud seed on a ZFS zvol and mistakenly apply guest settings to the hypervisor. Guest cloud-init generation remains enabled; only the already-provisioned host's cloud-init service is disabled.

## Safety and storage

- Images, throwaway private lab keys, and state live under Git-ignored `.lab/`. Only a short-lived QMP socket uses a private temporary directory under `/tmp`.
- One randomly selected SSH port binds **127.0.0.1 only**. There is no public management listener, physical bridge, host-disk passthrough, or agent forwarding. Outbound NAT permits package downloads; this is not a hostile-code sandbox.
- SSH ignores user/global configuration, uses only the lab key, and pins a pre-generated host key. Mutations require the nonce and lab hostname. The CLI accepts only direct `.lab/run-*` directories, VM IDs 9001–9099, and a worker matching its compiled SHA-256.
- The default free-space floor is 10 GiB, checked during installation and between cases. CI explicitly uses 3 GiB. Sparse images are not physical reservations on the Mac. The initial image download needs another ~420 MiB after preflight.
- Normal completion/errors shut QEMU down, with fallback termination of only the child process started by that invocation. Ctrl-C unwinds cleanup. SIGKILL/power loss cannot guarantee cleanup. No broad process killing or automatic disk deletion is used.
- Runs retain disks for debugging and share a local lab lock. After QEMU exits, remove old `.lab/run-*` directories explicitly when no longer needed. Never publicly upload private keys, seed ISOs, journals, or virtual disks.

## Actual coverage

Seven creation steps use real ZFS and Proxmox APIs:

1. A per-person dataset with a 128 MiB quota **and reservation**.
2. A non-sparse ZFS storage entry targeting that dataset.
3. A deterministic VM ID with ownership metadata included at creation.
4. A deterministic 32 MiB thick zvol with an atomic ownership property.
5. Attachment of that exact volume, with no implicit disk allocation.
6. A Proxmox-generated cloud-init drive.
7. Starting nested QEMU with hardware acceleration disabled.

The adapter invokes Proxmox's real API handlers through `pvesh` over pinned SSH, not an HTTP API client. HTTP token authorization and TLS transport are not covered. Repeated Perl CLI startup is deliberately simple but slow under ARM-to-x86 emulation; Linux CI timing needs its own measurement.

These are deliberately tiny test allocations, not usable development machines. **The root disk is blank: a running QEMU process is not a booted guest.** Image import, guest boot/SSH, actual cloud-init disk contents, and production configuration parity are not established by this slice.

The full matrix creates 23 allocations: one normal case, seven steps × three client-process boundaries, and a server-reboot recovery case. Smoke mode creates five allocations. Each recovery uses a fresh CLI process and checks:

- redb intent committed with `Durability::Immediate` before submission; no transaction held across SSH/API calls.
- Exit before submission, after asynchronous service submission, or after observation before the local completion commit.
- Resume follows a deterministic server-side systemd service rather than resubmitting its effect. The helper records task IDs/outcomes independently and serializes managed effects with a remote lock.
- A no-op rerun preserves ZFS GUIDs/creation transaction IDs, VM identities, attachments, the running QEMU PID, and local attempt counts.
- A real oversized thick-zvol allocation fails the person's ZFS quota.
- Final VM IDs are exactly those expected. Each nested VM is stopped only after its no-op rerun passes.
- A completed operation recovers after a confirmed new server boot: persistent identities stay fixed and only the runtime PID/boot identity may change.
- A deliberately delayed cloud-init action records server-side intent, then the server reboots before the effect; resume verifies/reconciles the existing resources and continues under the new boot identity.
- Extra/orphan volumes, deliberate quota drift, and a changed VM ownership marker are detected. Drift is not overwritten.

The completed-operation reboot test stops its blank nested QEMU guest before rebooting the outer host; there is no guest OS to respond to an ACPI shutdown. The operation journal is deliberately left verified so the reconciler must distinguish an allowed new-boot runtime restart from forbidden recreation of persistent resources.

The wrapper is **not a generic exactly-once Proxmox API**. A confirmed new host boot proves old local service processes cannot still execute. Incomplete intents can then be reconciled against deterministic resource names and ownership. A known operation-owned stale VM-creation lock may be cleared only in that recovery path. An orphan cloud-init drive may be reused only with a durable preflight-absence witness, the expected owner, and exact size; its disposable contents are regenerated. These narrower recovery paths need more injection coverage, particularly mid-allocation, before production use.

Same-boot helper crashes, unknown locks, journal corruption, external writers, imports, cancellation, delete/resize, partitions, and hard power loss remain unsupported/partially tested. The server's fsynced JSON receipts are not yet an ACID/checksummed remote operation store. Unknown ownership or inability to prove safe continuation blocks rather than guessing. Persistent verified resources that vanish are not recreated. The explicit runtime exception only permits restarting the already-owned VM after a confirmed host reboot.

The legacy offline simulator remains on SQLite. The experimental real-lab journal uses redb 3.1.3. A full persistence migration and reusable environment/ownership catalog are separate work. Each lab operation is bound to its nonce, VM ID, and exact worker code hash.

## Evidence and future PRs

Runs write `.lab/run-*/evidence/summary.json`, per-case states, quota-refusal logs, versions, and diagnostics. Successful evidence is copied to `evidence/real-proxmox-*`. Only `complete: true` means the entire selected matrix passed.

### Verified local result: November 9, 2026

`test --smoke --timeout 1800` passed against Proxmox VE **9.2.26**, kernel **7.0.14-28-pve**, using single-threaded x86 TCG on this ARM Mac. The selected suite reported 4,521.1 seconds (about 75 minutes), reusing the already-built base. QEMU then shut down cleanly. Evidence is retained in `evidence/real-proxmox-20261109T123405-575f27/`.

- All five allocations passed identity-preserving reruns and actual quota-refusal checks.
- Fresh CLI processes recovered after submission of VM creation, after observation of zvol creation, and before submission of VM start.
- Both completed-operation host reboot recovery and reboot with a durably recorded pending cloud-init action passed.
- Final inventory contained exactly the expected VMs and zvols, with no orphan volumes.
- Deliberate quota drift was rejected without overwriting it; a changed VM owner was also rejected.
- The fast checks passed **23 Rust tests and 16 Python tests**, formatting, Clippy with warnings denied, syntax checks, and actionlint. These include the 96 simulated recovery scenarios and all 21 redb fresh-process client boundaries.

This is a verified **five-case real smoke run**, not a run of the full 23-allocation real matrix. That smoke run did not test nested guest boot or image import; those are covered by the separate extension below. The earlier failed disposable runs exposed the host cloud-init seed-discovery problem and a multi-threaded-emulation kernel panic; neither was treated as a successful run or retried against production.

`.github/workflows/checks.yml` assumes `pve-dev/` becomes the repository root. Every PR runs Rust checks, simulator tests, redb process-recovery/safety tests, then the real emulated matrix. Workflows pin action commits, grant read-only GitHub permissions, use ephemeral hosted runners and no production secrets, and allowlist uploaded evidence. They do not use `pull_request_target` or persistent self-hosted runners.

No repository, paid runner, or branch protection was created. GitHub execution and Linux runner timing/disk feasibility still require the first actual CI run. After validation, make all three workflow jobs required in branch protection. Review the real infrastructure spec, fixtures, and historical evidence before publishing; this workspace was not automatically initialized or pushed.

### Multi-user Rust daemon/helper integration

The separate `control-test` command exercises the new Rust daemon, redb catalog, iroh client and privileged Rust host helper inside the same disposable real-Proxmox environment. See `../docs/control-plane.md` for commands, architecture, the verified eight-check result, and remaining production gates. `evidence/real-controller-20261109T173920-5183e4/summary.json` reports a complete passing run on November 9, 2026. It covers three owner reservations, queued-budget admission, tenant isolation, actual VM lifecycle/deletion, daemon SIGKILL recovery, physical ZFS quota enforcement, live revocation, and exact orphan-free inventories. This is distinct from the older five-case Python-worker smoke run and does not claim that the legacy full 23-allocation matrix was executed locally.

The workflow now includes macOS Swift-to-Rust integration and the bootable-guest extension below, in addition to the legacy real matrix. The guest binaries are static musl builds so they do not require the CI runner's Nix dynamic loader inside the Proxmox guest. GitHub execution itself remains unverified until a repository exists.

### Pinned-image guest extension

`guest-test` extends the eight-check controller suite with real pinned-image import/comparison, nested OS boot and clean cloud-init, owner SSH plus wrong-key refusal, and verified guest deletion with no remaining disks. It uses two 8 GiB mirrored lab disks, limits outer ZFS ARC to 512 MiB, and provisions an isolated `vmbr77` bridge without NAT. It freezes copies of both the native CLI and Linux binaries into each private run directory, so a concurrent rebuild cannot change the code exercised by an active run.

The guest checks run after authenticated bootstrap and before the remaining baseline checks, retaining all eleven assertions and ending with exact inventories. The nested guest receives four virtual CPUs and 1 GiB RAM, and successful boot additionally requires healthy systemd. Passive, bounded serial evidence detects kernel/initramfs failures early without typing into or automatically rebooting the guest. The initial single-threaded guest attempt exposed late disk discovery and was retained as failed, manually diagnosed evidence; it is not boot success. A subsequent one-vCPU guest reached systemd but timed out before SSH under heavy emulation and likewise remains a failed run.

```sh
nix develop ./nix --command bash scripts/build-lab-binaries.sh
nix develop ./nix --command python3 lab/lab.py guest-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 2400
```

The nested guest uses an explicitly labeled lab-only derivative of the checksum-pinned Debian parent. `prepare_guest_image.py` edits an unmounted private copy to extend systemd start/device deadlines to 600 seconds and set cloud-init's supported default-user mapping to `dev`, verifies the parent stayed unchanged, and publishes a separate SHA-256 pin. The helper omits Proxmox's deprecated `ciuser` override. No services or warning reports are disabled, and degraded cloud-init/systemd still fails. `guest-image-build.json` records the recipe, both image identities and both drop-in hashes; filesystem metadata means independently prepared images need not be byte-identical.

See `../docs/bootable-guests.md` for the CLI, image trust policy, recovery rules and exact readiness assertions. A partial or timed-out run does not count as a boot success. The workflow invokes this extension on PRs, but hosted-runner execution and timing are not yet verified.

The November 10 extension run passed **all eleven combined controller/guest checks** against PVE 9.2.26 in 3,576.5 seconds (about 60 minutes), excluding final diagnostics/shutdown. Evidence is in `../evidence/real-controller-20261110T000212-44dcfa/`, with both `complete` and `nested_guest_boot_verified` true. Cloud-init completed without errors or recoverable errors; owner SSH/sudo, wrong-key refusal, healthy systemd, verified guest deletion and all baseline recovery/isolation/quota checks passed. No manual console input or extra diagnostic guest SSH was used in this run. The historical failed attempts remain incomplete evidence, not passing runs.
