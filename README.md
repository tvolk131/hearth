# Hearth

Hearth lets a small group share one or more [Proxmox VE](https://www.proxmox.com/en/proxmox-virtual-environment) hosts. Each person gets a bounded budget of disk, memory and CPU, and manages their own VMs from a native macOS app or a CLI. Administrators create accounts, set budgets and handle maintenance such as disk resizing and migration between hosts.

The core design goal is that **nothing happens twice and nothing is silently lost**. Every change is written as a durable intent before it runs, then verified against the real host before it's marked done. Any step whose outcome is uncertain blocks for an administrator instead of guessing.

> **Status: experimental.** Hearth runs a real two-host deployment, but its interfaces, storage format and binary names (`pve-dev*`) will change. Don't point it at a host whose existing VMs you can't afford to have inspected or adopted carefully.

## Components

| Binary | Runs on | Role |
| --- | --- | --- |
| `pve-devd` | each Proxmox host, unprivileged | Controller: accounts, budgets, request admission, authenticated [iroh](https://iroh.computer) RPC. State in [redb](https://www.redb.org). |
| `pve-dev-host` | each Proxmox host, root | Privileged helper: performs Proxmox/ZFS changes from a separate recovery journal, accepting only scoped, fingerprinted jobs. |
| `pve-devctl` | client | CLI for users and admins. JSON on stdout; exit 0 success, 2 rejected by server, 1 local or transport error. |
| `pve-dev-ops` | host, root | Offline maintenance: paired backups, quarantined restores, service staging. |
| `pve-dev-inventory` | client | Read-only discovery of existing hosts into a separate observation store. |
| `macos/` | client | SwiftUI app. Drives `pve-devctl` and never reads keys or databases itself. |

There is no shared database or consensus between hosts: each host's controller is independent, and the app talks to each one directly.

## Quick start (development)

```sh
cargo build --locked --bins
cargo test --locked

export PATH="$PWD/target/debug:$PATH"
pve-devctl identity new --path ~/.config/hearth/device.key
pve-devctl --help
```

Run the local end-to-end check of the real daemon, helper and CLI against a simulated Proxmox backend:

```sh
python3 scripts/test-control-local.py --quota --user-deletion
```

Build and test the macOS app (needs Xcode or the Command Line Tools):

```sh
bash scripts/check-swift.sh
```

`nix develop ./nix#checks` provides the same toolchain CI uses; `bash scripts/check.sh` runs every fast check.

## Testing tiers

1. **Simulated** (`cargo test`, every push): the controller and helper run against an in-memory Proxmox/ZFS world with crash injection at each step boundary.
2. **Local processes** (`scripts/test-control-local.py`, every push): real binaries talking over real sockets and iroh.
3. **Disposable Proxmox** (`lab/lab.py guest-test`, nightly): boots Proxmox VE under QEMU and runs real VM lifecycles, quota enforcement and recovery. See `lab/README.md`.

## Documentation

- [`docs/control-plane.md`](docs/control-plane.md): architecture, authority and commands
- [`docs/host.md`](docs/host.md): the privileged helper's wire contract and execution model
- [`docs/state-machine-audit.md`](docs/state-machine-audit.md): adversarial recovery review and its fixes
- [`docs/persistent-management.md`](docs/persistent-management.md): the managed backend for real hosts
- [`docs/deployment-recovery.md`](docs/deployment-recovery.md): backups and restores
- Feature notes: [disk shrink](docs/disk-shrink.md), [quota editing](docs/quota-editing.md), [user deletion](docs/user-deletion.md), [stopped migration](docs/stopped-migration.md), [discovery](docs/discovery.md)

Some docs still read like development logs and are being rewritten.

## License

Not yet licensed. All rights reserved until a license is added.
