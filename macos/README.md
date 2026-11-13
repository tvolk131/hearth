# PveDevApp: experimental macOS lab client

> **Experimental.** Lab and simulated controllers create VMs with **blank root disks**. Physical-trial controllers require an explicitly configured disposable-resource scope and advertise each enabled maintenance capability. A separate **Read-only Host** connection observes real Proxmox/ZFS machines over pinned SSH and offers no changes or enrollment. The controller's `environment` and `backend` values are shown on its Overview.

PveDevApp is a native SwiftUI frontend (macOS 14+) over the Rust `pve-devctl` CLI. It is deliberately thin:

- All networking, iroh, authentication, authorization policy, budgets and state stay in Rust.
- The app never opens the profile, private keys, or `redb`/state databases. It passes the profile **path** and saved public identity expectations to the CLI.
- Permission-aware visibility (admin panels, owner picker, per-VM controls) is for UX only. The server performs the real authorization.

## Layout

| Path | Purpose |
| --- | --- |
| `Package.swift` | SwiftPM manifest: `PveDevApp` executable, `PveDevCore` library, `PveDevCoreTests` |
| `Sources/PveDevCore/WireModels.swift` | Decodable `User`, `Budget`, `Vm`, `OperationRecord`, `DashboardResult`, `UpdatedResult` |
| `Sources/PveDevCore/WireRequests.swift` | `RpcRequest`, which encodes the tagged `action` wire format exactly |
| `Sources/PveDevCore/WireResponse.swift` | `{ok,result}` / `{ok,error}` envelope, `RpcFailure`, response interpretation |
| `Sources/PveDevCore/ProcessRunner.swift` | Bounded, cancellable child-process runner (no shell) |
| `Sources/PveDevCore/RpcClient.swift` | Builds `pve-devctl --profile <path> rpc` invocations |
| `Sources/PveDevCore/MutationLedger.swift` | Request-ID retention and on-disk pending-request ledger |
| `Sources/PveDevCore/AppSettings.swift`, `ServerProfile.swift` | Named servers, pinned public identities, path validation and settings migration |
| `Sources/PveDevCore/FormDrafts.swift`, `Permissions.swift`, `SafeText.swift` | Form validation, visibility rules, display sanitising |
| `Sources/PveDevApp/*` | SwiftUI app: sidebar, Overview, VMs, Operations, Pending Requests, Users, Devices, Settings |
| `Tests/PveDevCoreTests/*` | Swift Testing suites |

## Build, test, run

```sh
cd macos
swift build
swift build -c release
```

With full Xcode installed, `swift test` works directly. With only the Command Line Tools, the `Testing` framework is outside the default search path, so pass it explicitly:

```sh
F=/Library/Developer/CommandLineTools/Library/Developer/Frameworks
L=/Library/Developer/CommandLineTools/Library/Developer/usr/lib
swift test -Xswiftc -F -Xswiftc $F -Xlinker -F -Xlinker $F \
  -Xlinker -rpath -Xlinker $F -Xlinker -rpath -Xlinker $L
```

`swift run PveDevApp` launches the app as a bare executable. It is not a signed `.app` bundle.

For local review in this workspace, `.build/PveDevApp-MultiServer.app` contains the validated release executable with an ad hoc signature. Quit the already running PveDevApp first, then run `open .build/PveDevApp-MultiServer.app` from `macos/`. This separate bundle does not replace or restart the running app. It uses the same settings domain, so the legacy profile migration applies on its first launch. It is not a distributable, Developer ID signed or notarised release.

## Configuration

Open **Settings** (⌘,) and choose:

1. **pve-devctl executable**: an absolute path to an existing executable file.
2. Under **Servers**, add a name and choose an absolute **Profile JSON** path for each lab controller. Other extensions, such as `.redb` or key files, are refused.
3. Click **Save and Trust Profile**. The CLI reads the local profile and private key and returns only the public server ID, environment ID and device ID. This does not connect to a server. Every subsequent request verifies the saved IDs against the exact profile and key used for that request.
4. Choose the active server in the sidebar, then refresh. Each server retains its own dashboard, permissions, loading indicator, errors and refresh time. There is no aggregate view.
5. **Request timeout** is 5–600 s, default 60 s. **Auto-refresh interval** is 5–3600 s. Enable it separately for each server in **Lab → Auto-refresh Dashboard Every …s**. Polling on a server continues if you select another server; it starts off for every server at launch.

Settings use `UserDefaults` key `PveDevApp.settings.v2`. A previous `v1` single profile becomes **Default Server** without changing the legacy record. Inspect and trust it once before connecting. Existing pending requests have no trustworthy saved identities and remain available for review/discard; they cannot be retried. Removing a server keeps its pending requests. Changing a profile path clears its saved trust; replacing a profile or key in place causes requests to fail locally until you explicitly trust the new identity.

Those profiles target independent experimental lab controllers. To observe real physical machines, use **Add Read-only Host**, choose a reviewed observation profile, and **Save and Trust Profile**. The inventory executable defaults to `pve-dev-inventory` beside `pve-devctl`; it can be chosen separately. The app stores only its path, profile paths and public host bindings. Rust runs the fixed read-only collector over strictly pinned SSH on refresh. Host observations have their own overview/VM pages and cache, with capture and refresh times. No operations, admin, create or VM-action controls exist on these pages; model/RPC configuration guards also reject mutations/retries locally. See [read-only GUI setup](../docs/read-only-gui.md). Production management still requires future backend support; this UI does not install or adopt a host.

## How the CLI is invoked

For every request the app starts exactly one child process:

```
<executable> --profile <profile path> --expected-server-id <public ID> \
  --expected-environment <environment ID> --expected-device-id <public ID> rpc
```

- It uses a `Process` with an argument array. There is no shell, no `PATH` lookup and no string interpolation into a command line. The working directory is `/`.
- The environment is minimal: `HOME`, `USER`, `LOGNAME`, `TMPDIR`, `LANG`, `LC_ALL` and `LC_CTYPE` are inherited when set. `PATH` is fixed to `/usr/bin:/bin:/usr/sbin:/sbin`, plus `NO_COLOR=1` and `RUST_BACKTRACE=0`. No other parent variables are passed.
- The app writes one JSON request object, followed by a single `\n`, to stdin and then closes stdin. `serde_json::from_reader` accepts trailing whitespace.
- stdout and stderr are drained concurrently. stdout is capped at 8 MiB: on overflow the child is terminated and the call fails. stderr is capped at 64 KiB: any excess is dropped and the call continues.
- On timeout or cancellation, the app sends `SIGTERM` to **that child's PID only**, then `SIGKILL` after 2 s if it is still alive. Other in-flight requests are unaffected. Process groups are not signalled.
- Writing to a child that has exited cannot raise `SIGPIPE` in the app (`F_SETNOSIGPIPE`).
- Request bodies, stdout and stderr are never logged. A sanitised stderr tail of at most 1,500 characters appears only in in-app error details.

### Outcome classification

| Situation | Result |
| --- | --- |
| stdout `{"ok":true,...}` and exit 0 | success |
| stdout `{"ok":false,"error":{...}}`, any exit status | **server error** (`forbidden`, `invalid`, `conflict`, `unavailable`, `internal`; unknown codes are tolerated) |
| stdout `ok:true` but nonzero exit or a signal | process failure (not trusted as success) |
| unparseable stdout and nonzero exit or a signal | process failure, with the stderr tail |
| unparseable stdout and exit 0 | malformed response, reporting the field path only |
| launch error, timeout, cancellation, stdout overflow | each reported distinctly |

Server messages, operation errors and stderr are shown with control characters removed and long token-like runs (40 or more characters) replaced with `[redacted]`.

## Wire contract implemented

Requests use a top-level `action` tag in snake_case. `owner` is sent as `null` when it is absent. `request_id` is a lowercase UUID string.

```json
{"action":"dashboard"}
{"action":"create_vm","request_id":"<uuid>","name":"dev-frank","disk_mib":32,"memory_mib":128,"min_memory_mib":128,"cores":1,"owner":null}
{"action":"vm_action","request_id":"<uuid>","vm_id":9001,"kind":"start|stop|delete"}
{"action":"create_user","request_id":"<uuid>","user_id":"frank","budget_mib":128,"min_memory_budget_mib":256,"max_memory_budget_mib":1024,"max_cores":8,"max_vms":4}
{"action":"enroll_device","user_id":"frank","endpoint_id":"<64 hex>","label":"Frank laptop"}
{"action":"revoke_device","endpoint_id":"<64 hex>"}
```

Results by action:

- `dashboard` returns `identity`, `users`, `budget`, `vms`, `operations`, `environment` and `backend`.
- `create_vm`, `vm_action` and `create_user` return an operation object.
- `enroll_device` and `revoke_device` return `{"updated":bool}`.

Unknown fields are ignored at every level. Unknown enum strings for role, VM state, operation status, backend and error code decode without failing. An unrecognised role is treated as non-admin.

## Request-ID retention (no double allocation)

Each mutation attempt gets a request ID once. That ID is kept until the server acknowledges the request.

- The request is recorded in a pending ledger **before** it is sent. The ledger is written atomically with mode `0600` to `~/Library/Application Support/PveDevApp/pending-mutations.json` and holds no secrets.
- **Timeout, cancellation, launch or process failure, malformed output, or a server `unavailable`/`internal` error** leave the request *unacknowledged*. **Retry with Same Request ID** resends the byte-identical request. A new ID is never generated silently.
- Submitting an identical intent again, such as double-clicking Create, reuses the pending entry instead of minting a new ID.
- A server `forbidden`, `invalid` or `conflict` error is a definitive rejection. The entry can only be discarded. A later corrected submission gets a new ID.
- **Discard** needs confirmation and warns that the original may already have been applied.
- A retry is bound to the local server record, canonical profile path, public controller ID, environment ID, and public device ID. All must still match. Identical VMIDs and request UUIDs on different servers do not share busy state or ledger entries.
- After a relaunch, requests that were in flight are shown as *outcome unknown*. **Nothing is sent automatically at launch.** Every send and retry needs an explicit click.
- `enroll_device` and `revoke_device` carry no `request_id` per the contract. They are tracked locally the same way and rely on server-side idempotency.

## UI behaviour

- The status banner occupies one shared header with the same inset on every page. The toolbar orders page-specific controls before Refresh, which always remains rightmost. VM/operation filters retain their values while navigating within the window.
- **Overview**: identity, role, environment and backend, plus your own budget: disk allocated and remaining, maximum and minimum memory, cores and VM count against your limits. Admins see everyone's data, but the budget shown is their own. It also shows active and blocked operations and a VM summary.
- **Virtual Machines**: a sortable table with owner, resources and state, and start/stop/delete controls shown only to the owner or an admin, enabled by state.
  - **New VM** collects name, `disk_mib`, `memory_mib`, `min_memory_mib` and `cores`. Admins also get an owner picker.
  - **Stop** needs confirmation. **Delete** needs you to type the VM name.
- **Operations**: status, step, error, owner, operation ID and request ID, with progress shown for queued and running operations. When the selected filter has no results, the empty-state icon and text are centered in the available page area below the shared header.
- **Pending Requests**: unacknowledged and rejected requests, with Retry (same ID), Cancel (terminates only that request's child process) and Discard.
- **Users** (admin only): the user list, create-user form and **Edit…** budget dialog. Edits retain the origin server and expected policy revision through confirmation. Positive reductions must still cover committed allocations; no guest disk/configuration is resized. Pending/blocked owners cannot start another edit. See `../docs/quota-editing.md`.
- **Devices** (admin only): enroll a public iroh endpoint ID for a user, and revoke an endpoint ID after confirmation.
- Refresh is manual (toolbar button or ⌘R). The toolbar keeps one fixed-size button: refreshes longer than 300 ms replace its arrow with a spinner in the same space, and clicking during a refresh cancels it. Faster refreshes show no spinner. Optional auto-refresh lives in the Lab menu, starts off at launch, and only reloads the dashboard without sending or retrying changes. A completed mutation triggers one read-only refresh of its originating server, even if another server is selected. Replacing or removing that originating profile prevents the automatic refresh.

## Validation and known gaps

- The core transport has been verified against the real Rust `rpc` subcommand and live loopback daemon using `python3 scripts/test-control-local.py --swift` from the project root. This tests dashboard decoding, mutation submission and request-ID reuse against the simulated host backend.
- Mutation sends fail closed if the pending ledger cannot be durably written. An unreadable existing ledger is preserved and blocks new mutations pending investigation.
- Identity-bound requests reject a changed server ID, environment ID or device ID before opening an iroh endpoint. Profile routing/address changes are allowed only when the authenticated public server identity stays the same. Validation and connection use the same loaded profile and key, avoiding a metadata preflight race.
- Automated multi-server tests cover settings migration, matching VMID and request-ID isolation, stale refresh results, host-specific errors/permissions/polling, switching during mutation, explicit profile trust, and retries after identity changes. CLI tests cover each mismatched identity, replaced profiles/keys, schema/private-file guards, and existing profile generation compatibility. No real Hetzner host is used by these tests.
- Multi-server UI layout has compiled in debug/release; this change has not been visually exercised in the user's running app.
- A local UI smoke check on macOS 26.6.2 (2026-11-10) reproduced and fixed the VM page expanding the split layout beyond the window. The page now bounds its native table to the available detail area. Checks covered dashboard loading, all sidebar destinations, returning to VMs, column sorting, the deleted-VM filter, window zoom, and opening/cancelling the New VM sheet. No mutations were submitted. A temporary local app wrapper enabled UI automation; distributable packaging remains unfinished. Debug/release builds and the Swift suite passed; the opt-in Rust CLI integration test was skipped in this rerun.
- The contract has no device listing, so revocation takes a typed endpoint ID.
- Client-side form checks are conveniences and may be stricter than the server:
  - VM names must be DNS labels.
  - User IDs must be lowercase and at most 32 characters.
  - Endpoint IDs must be 64 hex characters.
- Admin budget views show only the admin's own budget. There is no per-user budget selector yet.
- No distributable app packaging, Developer ID signing or notarisation; local ad hoc review wrappers are temporary artifacts.
- A child that forks its own grandchildren is not tracked. Only the direct child is signalled.

## Reviewed VM maintenance and transfers

New maintenance actions appear only when the selected controller advertises
support and the signed-in account is an administrator. Physical-trial controllers
must advertise each enabled capability; they do not inherit the legacy lab
fallback. Read-only host profiles still expose no mutation or enrollment path.

- **More Actions → Expand Disk… / Shrink Disk…** obtains a server capacity plan
  before submission and binds the request to the expected current size. An
  optional GPT root partition UUID supports explicit selection in layouts with
  more than one ext4 partition. The filesystem is checked during server-side
  preparation; a displayed capacity plan is not a filesystem-fit certificate.
- **Enroll VM…** reviews the observed VM, owner, resources and root/cloud-init
  disk identities. Only an eligible server plan can be submitted, with its exact
  plan ID and explicit ownership/budget confirmation.
- **Operations → Inspect and Recover…** displays fresh server evidence for a
  blocked operation. It offers only recognized actions enumerated by that
  inspection; its inspection ID is retained in the pending request. A verified
  abort is shown as **Cancelled**, not completed. No generic force action exists.
- **More Actions → Move VM…** loads a separately trusted destination profile and
  requires administrator access there. The source and destination independently
  authorize the same immutable transfer plan. The client checks both pinned
  identities, VM/profile/owner mapping and exact grant-plan agreement, then
  starts the reviewed migration. This is a stopped transfer. Source recovery
  removal requires separate guest-verification and deletion acknowledgments.
- **Transfers** lists durable server-side migrations so an accepted copy can be
  inspected after the sheet closes or the app restarts. Committed or unknown
  transfer states cannot expose a pre-commit abort action. Pre-commit renewal
  retains the same plan and copy offsets, requires a fresh source authorization
  followed by independent destination consent, and keeps each renewal request
  ID for exact retry. Aborting starts at the source; destination staging cleanup
  forwards the signed source abort proof. Source recovery removal is offered
  only after the server reports destination acceptance.
- Operation rows show server-reported stage counts and timestamps. Stage counts
  describe verified work, not an estimated fraction of elapsed transfer time.

Multi-server authorization uses the same durable pending-request ledger as
ordinary mutations. Target-side calls keep their own profile/device/server
binding and do not change the selected server. A stale query or acknowledgement
cannot advance a workflow after a route/identity change. Only public descriptors
and scoped signed grants pass through the app; private profile files and key
files remain exclusively handled by the Rust CLI. A lost acknowledgement keeps
its exact request for retry in **Pending Requests**.

These are client workflows. Server eligibility, scope, capacity, signed peer
proofs, persistent fencing and recovery receipts remain authoritative. The new
physical test environments are restricted to disposable resources; no existing
Hetzner guest is enrolled by these client changes. GUI compilation and automated
routing tests do not substitute for the separate backend/physical qualification.
