# Storage layout, paging and retention

The controller catalog and the helper journal used to be stored as one JSON
row each, rewritten whole on every change and loaded whole on every request.
History only grows, so this document covers three changes: storing it in
separate tables, paging the lists that grow without limit, and an offline
command that archives and prunes old history.

## Storage format 2

Each database records an explicit **storage format** in the `hearth_meta`
table (`format = 2`). This number describes the physical layout and is
separate from the logical `schema` field inside the catalog and journal.

| Database | Table | Key → value |
| --- | --- | --- |
| controller | `controller_core_v2` | `0` → catalog without operations, requests or audit |
| controller | `controller_operations_v2` | operation ID → operation record |
| controller | `controller_requests_v2` | request key → operation ID |
| controller | `controller_audit_v2` | sequence → audit entry |
| helper | `host_journal_core_v2` | `0` → journal without jobs |
| helper | `host_jobs_v2` | operation ID → helper job |

`Store::change(|catalog| ...)` keeps its shape. Each change loads the core
and the operation and request rows inside one write transaction. It rewrites
only the rows that changed and appends any new audit entries. The audit log
is not loaded for ordinary requests. `hearth_meta.audit_next` hands out audit
sequence numbers, so a number is never reused after pruning. The helper's
`change` works the same way.

### Refusing newer state

serde silently ignores fields it does not know, so an older binary could
otherwise load newer state and drop those fields the next time it wrote. Two
guards prevent this:

* **Format version.** A database whose `format` is higher than this build
  understands is refused with "written in storage format N by a newer
  Hearth". So are unknown formats, a split layout without its marker, a
  database holding both layouts, and tables this build did not create.
* **Lossless open.** On open, every stored row is decoded and re-encoded. If
  any stored non-null field does not survive, the database is refused and the
  error names that field. This catches a newer build that added a field
  without bumping the format.

Binaries from before this change only know `controller_catalog_v1` and
`host_journal_v1`. Migration deletes those tables, so an old binary fails to
find its catalog instead of starting with partial state.

### Migration from format 1

`Store::open` and `Host::open` first validate the legacy row: scope, binding
and helper identity checks. Then they migrate once, in a single redb write
transaction. The transaction writes the split tables and the format marker,
deletes the legacy table, then commits. redb commits are atomic, so a crash
leaves either the untouched format-1 database or a complete format-2 one, and
the next open simply retries. Tests cover:

* migration from a committed format-1 fixture
  (`tests/fixtures/format1-controller-catalog.json`) and from a format-1
  helper journal;
* a hook that fails, a hook that panics, and a child process that exits
  without unwinding just before the commit. Each leaves the format-1 database
  intact, and a later open migrates it.

Format-1 rows were only ever written by this build or older ones, and before
this change unknown fields were dropped on every write anyway. So migration
decodes the legacy row the same way the old code did; the lossless check
applies from format 2 on. The managed schema-repair tool (`schema_repair.rs`)
still works only on format-1 journals, because the downgrade it repairs can
only have been written by an older binary.

### Backups

Backup bundles keep their logical, layout-independent JSON payload. `backup`
reads both databases through the same validated path as the services.
`restore` writes a fresh format-2 database. Bundles are now written and
hashed as a stream, and the read cap has risen from 64 MiB to 1 GiB
(`maintenance::BUNDLE_LIMIT`). Bundles from before this change still verify
and restore.

## Paging

`list_operations` and the new admin-only `list_audit` accept an optional
`cursor` and `limit`:

* A `list_operations` request with neither field returns the bare array older
  clients expect. With either field it returns
  `{"operations": [...], "next_cursor": "<operation id>" | null}`, in
  operation-ID order.
* `list_audit` returns `{"entries": [{"seq", "entry"}], "next_cursor": <seq> | null}`
  in sequence order.
* `limit` defaults to 100 and must be between 1 and 1000. A page also stops
  before its encoded size exceeds 1 MiB, so a page always fits in a protocol
  frame.
* `dashboard` returns every unfinished operation plus the 200 most recent
  finished ones, newest first. `omitted_operations` counts the rest, which
  are available through `list_operations`.

`pve-devctl operations` still sends the unpaged request. If the server refuses
it as too large, the CLI follows the pages itself. `--limit`/`--cursor` print
one page, and `--all` fetches every page. `pve-devctl audit` pages the audit
log the same way. The Swift client did not need changes: its dashboard decoder
ignores the new optional field.

Migrations (`migration list`) are not paged. They are limited to one record
per VM ID (at most 65,536) and are removed when finished.

## Retention: `pve-dev-ops prune`

```sh
pve-dev-ops prune --controller-dir /var/lib/pve-dev/controller \
  --helper-dir /var/lib/pve-dev-host --archive /secure/archive-2026-11.jsonl \
  [--older-than-days 180] --expected-environment ENV \
  --expected-server SERVER_ID --expected-pool-guid GUID
```

Use `--dry-run` instead of `--archive` to print counts without writing
anything. Like `backup`, prune runs **offline**:

* both services must be stopped;
* it takes the exclusive locks on both databases;
* it checks the same identity pins;
* both sides must already be in format 2 (start the upgraded services once
  first);
* it validates both sides on their own and as a pair before changing
  anything.

It then:

1. **Exports first.** It writes the archive to a new file (mode 0600; an
   existing file is never overwritten) and fsyncs it. The archive is JSONL: a
   header (environment, cutoff, revisions), then one line per pruned
   operation, helper job and audit entry, then a footer with counts and a
   SHA-256 of the preceding bytes. The footer catches truncation;
   `retention::read_archive` checks it. If nothing qualifies, no archive is
   written.
2. **Prunes the controller** in one transaction and re-validates the result.
3. **Prunes the helper** in one transaction and re-validates the pair.

The receipt reports counts and the archive digest. There is no automatic
schedule. The archive is the only remaining copy of the pruned history, so
storing it is up to the operator, and pruning needs both databases offline
together.

### What is never pruned

An operation is kept if any of these hold:

* it is not terminal (`queued`, `running`, `blocked`), or its helper job is
  not terminal;
* it was created or progressed within the retention window;
* it is named anywhere in the controller's or helper's core state: shrink and
  growth holds, migrations and their staging-create IDs, account bootstrap and
  deletion proofs, machine create/delete operations, imports;
* it is the proof for a verified enrollment, or the deletion proof of an
  account that is no longer active;
* it is linked by an exact ID reference, in either direction, to any kept
  operation or job. A recovery and the blocked operation it recovers, or a
  shrink and its finish, are kept or pruned together, so the pair checks in
  `validate_scope` still hold.

**Request-ID records are never pruned.** Operation IDs are derived from the
requesting user and request ID, and both sides deduplicate by them. Dropping
the mapping would let a reused request ID recreate the ID of an archived
operation and run it again. The mapping is small. Reusing an archived request
ID is refused with a `conflict` telling the client to choose a new request ID.

Audit entries older than the cutoff are pruned (they are archived first).

### Why pruning helper jobs is safe

The controller resubmits every unfinished operation on each pass. The helper
answers a resubmission from its journal by operation ID. So a helper job may
only be removed once the controller can never submit that ID again. That
means all of the following:

* the job is terminal;
* its controller operation is already gone, or goes in the same run;
* the request-ID tombstone for that operation remains;
* no helper core state or retained job refers to it.

The controller is always pruned before the helper. The backup and prune pair
check accepts a finished helper job whose controller operation survives only
as a tombstone. So if prune is interrupted between the two databases, the
result is still a valid pair that backs up and starts. Running prune again
finishes the helper side. A test covers this case.
