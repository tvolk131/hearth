# Disposable real Proxmox integration lab

This is an **experimental integration adapter**, not permission to apply the production specification. It accepts no remote host, production SSH configuration, host block devices, or production credentials.

## Run

From the repository root:

```sh
nix develop ./nix --command bash scripts/check.sh
nix develop ./nix --command python3 lab/lab.py build
nix develop ./nix --command bash scripts/build-lab-binaries.sh
nix develop ./nix --command python3 lab/lab.py control-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 1800
nix develop ./nix --command python3 lab/lab.py guest-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 2400
```

`lab.py` accepts `build`, `control-test` and `guest-test`. The other scripts in
this directory are separate qualifiers and their unit tests, documented with
their features: [disk shrink](../docs/disk-shrink.md),
[migration spike](../docs/migration-spike.md) and
[handoff model](../docs/migration-handoff.md),
[physical-trial backend](../docs/physical-trial-backend.md) and
[bootable guests](../docs/bootable-guests.md).

The dedicated `nix/` flake keeps large images and sockets out of Nix's source snapshot. Its lockfile pins Nixpkgs, including QEMU. No Homebrew or permanent user-profile installation is needed.

The first build downloads a fixed Debian 13 amd64 cloud image, verifies SHA-512, and installs the real Proxmox kernel and packages. The signing key is downloaded with HTTPS and checked against its pinned SHA-256. Proxmox's documented HTTP apt repository still requires valid apt signatures. Apt repositories are currently rolling inputs: installed package versions are recorded, **not fully pinned**. A future trusted, checksum-pinned base artifact should remove this remaining source of variation.

An interrupted base build can resume after a corrected bootstrap script. An unchanged failed bootstrap is reported, not retried speculatively. A successful build is shut down and reused as a read-only backing image. Changed bootstrap/image inputs refuse reuse of a sealed base; preserve or explicitly remove `.lab/base` before rebuilding. Do not change a base while retained run overlays depend on it.

Each run gets a fresh root overlay, two new virtual data disks (4 GiB for `control-test`, 8 GiB for `guest-test`) and a fresh environment nonce. The data disks form a real ZFS mirror. The outer VM uses four emulated x86 CPUs and 4 GiB RAM, with TCG rather than nested KVM. Base-image builds use single-threaded TCG because a multithreaded run once panicked during a reboot. Controller/guest runs use multithreaded TCG to reduce nested boot delays and do not claim host reboot reliability. All modes fail promptly on a kernel panic instead of waiting for SSH or resetting automatically. The outer root is ext4, **not boot-from-ZFS**; this does not test production boot parity or mirror replacement.

After initial host provisioning, the harness disables cloud-init on the outer host before creating guest volumes. Otherwise, a later host boot can discover a guest's NoCloud seed on a ZFS zvol and mistakenly apply guest settings to the hypervisor. Guest cloud-init generation remains enabled; only the already-provisioned host's cloud-init service is disabled.

## Safety and storage

- Images, throwaway private lab keys, and state live under Git-ignored `.lab/`. Only a short-lived QMP socket uses a private temporary directory under `/tmp`.
- One randomly selected SSH port binds **127.0.0.1 only**. There is no public management listener, physical bridge, host-disk passthrough, or agent forwarding. Outbound NAT permits package downloads; this is not a hostile-code sandbox.
- SSH ignores user/global configuration, uses only the lab key, and pins a pre-generated host key. Mutations require the nonce and lab hostname. The lab helper accepts only VM IDs 9001–9099.
- The default free-space floor is 10 GiB, checked during installation and between cases. CI explicitly uses 3 GiB. Sparse images are not physical reservations on the Mac. The initial image download needs another ~420 MiB after preflight.
- Normal completion/errors shut QEMU down, with fallback termination of only the child process started by that invocation. Ctrl-C unwinds cleanup. SIGKILL/power loss cannot guarantee cleanup. No broad process killing or automatic disk deletion is used.
- Runs retain disks for debugging and share a local lab lock. After QEMU exits, remove old `.lab/run-*` directories explicitly when no longer needed. Never publicly upload private keys, seed ISOs, journals, or virtual disks.

## Removed legacy matrix

The first lab adapter was a Python-worker crash/reboot matrix (`lab.py test`,
with a five-allocation `--smoke` mode). It has been removed from the repository,
along with the legacy simulator. Its coverage and its November 9, 2026 smoke run
are summarized in [CHANGELOG.md](../CHANGELOG.md#2026-11-09). The Rust
daemon/helper suites below replace it.

## Evidence and CI

Runs write `.lab/run-controller-*/evidence/summary.json` and diagnostics. Successful evidence is copied to `evidence/real-controller-*/`. Both `.lab/` and `evidence/` are Git-ignored and are not part of the repository. Only `complete: true` means the entire selected suite passed.

`.github/workflows/checks.yml` runs the Rust checks (including the simulated-backend crash tests) and the local process suite on every pull request and push to `main`, and the macOS Swift-to-Rust integration alongside them. The slow real-Proxmox job (`lab/lab.py guest-test`) runs nightly and on manual dispatch. Workflows pin action commits, grant read-only GitHub permissions, use ephemeral hosted runners and no production secrets, and allowlist uploaded evidence. They do not use `pull_request_target` or persistent self-hosted runners.

The dated history of lab runs, including the first five-case smoke run, is in [CHANGELOG.md](../CHANGELOG.md).

## Multi-user Rust daemon/helper integration

The separate `control-test` command exercises the new Rust daemon, redb catalog, iroh client and privileged Rust host helper inside the same disposable real-Proxmox environment. See [`../docs/control-plane.md`](../docs/control-plane.md) for commands, architecture and remaining production gates. The suite covers three owner reservations, queued-budget admission, tenant isolation, actual VM lifecycle/deletion, daemon SIGKILL recovery, physical ZFS quota enforcement, live revocation, and exact orphan-free inventories.

The guest binaries are static musl builds so they do not require the CI runner's Nix dynamic loader inside the Proxmox guest.

## Pinned-image guest extension

`guest-test` extends the eight-check controller suite with real pinned-image import/comparison, nested OS boot and clean cloud-init, owner SSH plus wrong-key refusal, and verified guest deletion with no remaining disks. It uses two 8 GiB mirrored lab disks, limits outer ZFS ARC to 512 MiB, and provisions an isolated `vmbr77` bridge without NAT. It freezes copies of both the native CLI and Linux binaries into each private run directory, so a concurrent rebuild cannot change the code exercised by an active run.

The guest checks run after authenticated bootstrap and before the remaining baseline checks, retaining all eleven assertions and ending with exact inventories. The nested guest receives four virtual CPUs and 1 GiB RAM, and successful boot additionally requires healthy systemd. Passive, bounded serial evidence detects kernel/initramfs failures early without typing into or automatically rebooting the guest.

```sh
nix develop ./nix --command bash scripts/build-lab-binaries.sh
nix develop ./nix --command python3 lab/lab.py guest-test \
  --linux-bin-dir target/x86_64-unknown-linux-musl/release --timeout 2400
```

The nested guest uses an explicitly labeled lab-only derivative of the checksum-pinned Debian parent. `prepare_guest_image.py` edits an unmounted private copy to extend systemd start/device deadlines to 600 seconds and set cloud-init's supported default-user mapping to `dev`, verifies the parent stayed unchanged, and publishes a separate SHA-256 pin. The helper omits Proxmox's deprecated `ciuser` override. No services or warning reports are disabled, and degraded cloud-init/systemd still fails. `guest-image-build.json` records the recipe, both image identities and both drop-in hashes; filesystem metadata means independently prepared images need not be byte-identical.

See `../docs/bootable-guests.md` for the CLI, image trust policy, recovery rules and exact readiness assertions. A partial or timed-out run does not count as a boot success. The nightly real-Proxmox workflow job runs this extension.
