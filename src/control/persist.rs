//! Durable layout of the controller catalog and the helper journal.
//!
//! Format 1 (retired) stored each database as one JSON blob in a single redb
//! row, rewritten in full on every change. Format 2 keeps the small core state
//! as one blob but moves the collections that grow with history into their
//! own tables keyed by ID, and writes only the rows a change touched:
//!
//! | database   | core blob              | growing tables                                   |
//! | ---------- | ---------------------- | ------------------------------------------------ |
//! | controller | `controller_core_v2`   | `controller_operations_v2`, `controller_requests_v2`, `controller_audit_v2` |
//! | helper     | `host_journal_core_v2` | `host_jobs_v2`                                    |
//!
//! Both databases carry a `hearth_meta` table whose `format` row names the
//! layout. A build refuses any format it does not know, in particular a newer
//! one, rather than decoding it with serde (which silently ignores unknown
//! fields) and dropping that data on its next rewrite. As a second guard,
//! opening a format-2 database checks that every stored row survives a decode
//! and re-encode without losing a field, so a newer build that forgot to bump
//! the format is still refused instead of truncated.
//!
//! Hearth builds up to the release that dropped migration converted format 1
//! in place on first open. This build refuses a format-1 database and says
//! which earlier build to open it with first.
//!
//! Format 2 does not change what the in-memory [`Catalog`] or [`Journal`]
//! contains, with one exception: controller audit entries are append-only and
//! are not loaded for ordinary reads or changes. `Catalog::audit` starts empty
//! inside `Store::change`, and entries pushed there are appended with the next
//! audit sequence number. Full reads for backups load every entry.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use redb::{
    Database, Durability, ReadTransaction, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition, TableHandle, WriteTransaction,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use super::host::{HostJob, Journal};
use super::store::{Catalog, Record};

/// Format marker and counters shared by both databases.
pub const META: TableDefinition<&str, u64> = TableDefinition::new("hearth_meta");
const FORMAT_KEY: &str = "format";
const AUDIT_NEXT_KEY: &str = "audit_next";

/// Storage format this build reads and writes, for both databases.
pub const FORMAT_VERSION: u64 = 2;

pub const CATALOG_CORE: TableDefinition<u8, &[u8]> = TableDefinition::new("controller_core_v2");
pub const OPERATIONS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("controller_operations_v2");
pub const REQUESTS: TableDefinition<&str, &str> = TableDefinition::new("controller_requests_v2");
pub const AUDIT: TableDefinition<u64, &[u8]> = TableDefinition::new("controller_audit_v2");

pub const JOURNAL_CORE: TableDefinition<u8, &[u8]> = TableDefinition::new("host_journal_core_v2");
pub const JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("host_jobs_v2");

const CATALOG_SPLIT: [&str; 3] = ["operations", "requests", "audit"];
const JOURNAL_SPLIT: [&str; 1] = ["jobs"];

struct Kind {
    what: &'static str,
    /// The single table of retired format 1, named only to refuse it clearly.
    legacy: &'static str,
    owned: &'static [&'static str],
}

const CONTROLLER: Kind = Kind {
    what: "controller catalog",
    legacy: "controller_catalog_v1",
    owned: &[
        "hearth_meta",
        "controller_core_v2",
        "controller_operations_v2",
        "controller_requests_v2",
        "controller_audit_v2",
    ],
};

const HELPER: Kind = Kind {
    what: "helper journal",
    legacy: "host_journal_v1",
    owned: &["hearth_meta", "host_journal_core_v2", "host_jobs_v2"],
};

/// The last Hearth build that migrates a format-1 database on first open.
const LAST_MIGRATING_BUILD: &str = "e619ef0";

fn classify(kind: &Kind, names: &[String], format: Option<u64>) -> Result<()> {
    let has = |name: &str| names.iter().any(|n| n == name);
    if has(META.name()) {
        let version =
            format.with_context(|| format!("{} has no storage format marker", kind.what))?;
        ensure!(
            version <= FORMAT_VERSION,
            "{} was written in storage format {version} by a newer Hearth; this build understands \
             format {FORMAT_VERSION} and refuses to open it rather than drop data it cannot read",
            kind.what
        );
        ensure!(
            version == FORMAT_VERSION,
            "{} has unsupported storage format {version}",
            kind.what
        );
        for name in names {
            ensure!(
                kind.owned.contains(&name.as_str()),
                "{} contains table {name}, which this build does not own; it may have been written \
                 by a newer Hearth",
                kind.what
            );
        }
        return Ok(());
    }
    ensure!(
        !has(kind.legacy),
        "{} is still in retired storage format 1; open it once with Hearth \
         {LAST_MIGRATING_BUILD}, the last build that migrates it, then use this build",
        kind.what
    );
    ensure!(
        !kind.owned.iter().any(|name| has(name)),
        "{} has current-layout tables without a format marker; refusing to guess",
        kind.what
    );
    bail!("{} missing; refusing initialization", kind.what)
}

fn check_layout(tx: &ReadTransaction, kind: &Kind) -> Result<()> {
    let names: Vec<String> = tx.list_tables()?.map(|t| t.name().to_owned()).collect();
    let format = if names.iter().any(|n| n == META.name()) {
        tx.open_table(META)?.get(FORMAT_KEY)?.map(|v| v.value())
    } else {
        None
    };
    classify(kind, &names, format)
}

// ---------------------------------------------------------------------------
// Encoding helpers.

/// Returns the path of the first stored field that the re-encoded value lacks.
/// Explicit nulls count as absent, since optional fields are often skipped.
fn dropped(stored: &Value, roundtrip: &Value, path: &str) -> Option<String> {
    match (stored, roundtrip) {
        (Value::Object(stored), Value::Object(roundtrip)) => {
            for (key, value) in stored {
                if value.is_null() {
                    continue;
                }
                let child = format!("{path}/{key}");
                match roundtrip.get(key) {
                    None => return Some(child),
                    Some(next) => {
                        if let Some(found) = dropped(value, next, &child) {
                            return Some(found);
                        }
                    }
                }
            }
            None
        }
        (Value::Array(stored), Value::Array(roundtrip)) => {
            if stored.len() != roundtrip.len() {
                return Some(path.to_owned());
            }
            stored
                .iter()
                .zip(roundtrip)
                .enumerate()
                .find_map(|(index, (a, b))| dropped(a, b, &format!("{path}/{index}")))
        }
        _ => None,
    }
}

fn refuse_dropped(what: &str, stored: &Value, roundtrip: &Value) -> Result<()> {
    if let Some(path) = dropped(stored, roundtrip, "") {
        bail!(
            "{what} contains field {path}, which this build does not understand; it was probably \
             written by a newer Hearth. Refusing to open rather than silently drop it on the next write"
        );
    }
    Ok(())
}

fn decode<T: Serialize + DeserializeOwned>(what: &str, bytes: &[u8], check: bool) -> Result<T> {
    if !check {
        return serde_json::from_slice(bytes).with_context(|| format!("decoding {what}"));
    }
    let stored: Value =
        serde_json::from_slice(bytes).with_context(|| format!("decoding {what}"))?;
    let value: T =
        serde_json::from_value(stored.clone()).with_context(|| format!("decoding {what}"))?;
    refuse_dropped(what, &stored, &serde_json::to_value(&value)?)?;
    Ok(value)
}

fn strip(value: Value, keys: &[&str]) -> Result<Value> {
    let Value::Object(mut map) = value else {
        bail!("stored state is not a JSON object");
    };
    for key in keys {
        map.remove(*key);
    }
    Ok(Value::Object(map))
}

/// Serializes the catalog without its split collections.
fn catalog_core(catalog: &mut Catalog) -> Result<Vec<u8>> {
    let operations = std::mem::take(&mut catalog.operations);
    let requests = std::mem::take(&mut catalog.requests);
    let audit = std::mem::take(&mut catalog.audit);
    let value = serde_json::to_value(&*catalog);
    catalog.operations = operations;
    catalog.requests = requests;
    catalog.audit = audit;
    Ok(serde_json::to_vec(&strip(value?, &CATALOG_SPLIT)?)?)
}

fn journal_core(journal: &mut Journal) -> Result<Vec<u8>> {
    let jobs = std::mem::take(&mut journal.jobs);
    let value = serde_json::to_value(&*journal);
    journal.jobs = jobs;
    Ok(serde_json::to_vec(&strip(value?, &JOURNAL_SPLIT)?)?)
}

fn decode_core<T: DeserializeOwned>(
    what: &str,
    bytes: &[u8],
    split: &[(&str, Value)],
) -> Result<(T, Value)> {
    let stored: Value =
        serde_json::from_slice(bytes).with_context(|| format!("decoding {what}"))?;
    let Value::Object(mut map) = stored.clone() else {
        bail!("{what} is not a JSON object");
    };
    for (key, empty) in split {
        ensure!(
            !map.contains_key(*key),
            "{what} unexpectedly embeds its {key} collection"
        );
        map.insert((*key).to_owned(), empty.clone());
    }
    let value =
        serde_json::from_value(Value::Object(map)).with_context(|| format!("decoding {what}"))?;
    Ok((value, stored))
}

fn decode_catalog_core(bytes: &[u8], check: bool) -> Result<Catalog> {
    let (mut catalog, stored): (Catalog, Value) = decode_core(
        "controller catalog core",
        bytes,
        &[
            ("operations", Value::Object(Map::new())),
            ("requests", Value::Object(Map::new())),
            ("audit", Value::Array(Vec::new())),
        ],
    )?;
    if check {
        let roundtrip: Value = serde_json::from_slice(&catalog_core(&mut catalog)?)?;
        refuse_dropped("controller catalog core", &stored, &roundtrip)?;
    }
    Ok(catalog)
}

fn decode_journal_core(bytes: &[u8], check: bool) -> Result<Journal> {
    let (mut journal, stored): (Journal, Value) = decode_core(
        "helper journal core",
        bytes,
        &[("jobs", Value::Object(Map::new()))],
    )?;
    if check {
        let roundtrip: Value = serde_json::from_slice(&journal_core(&mut journal)?)?;
        refuse_dropped("helper journal core", &stored, &roundtrip)?;
    }
    Ok(journal)
}

// ---------------------------------------------------------------------------
// Row access shared by read and write transactions.

fn load_core(table: &impl ReadableTable<u8, &'static [u8]>, what: &str) -> Result<Vec<u8>> {
    Ok(table
        .get(0)?
        .with_context(|| format!("{what} missing; refusing initialization"))?
        .value()
        .to_vec())
}

fn load_rows(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut rows = BTreeMap::new();
    for row in table.iter()? {
        let (key, value) = row?;
        rows.insert(key.value().to_owned(), value.value().to_vec());
    }
    Ok(rows)
}

fn load_requests(
    table: &impl ReadableTable<&'static str, &'static str>,
) -> Result<BTreeMap<String, String>> {
    let mut rows = BTreeMap::new();
    for row in table.iter()? {
        let (key, value) = row?;
        rows.insert(key.value().to_owned(), value.value().to_owned());
    }
    Ok(rows)
}

fn load_audit(table: &impl ReadableTable<u64, &'static [u8]>) -> Result<Vec<(u64, Value)>> {
    let mut rows = Vec::new();
    for row in table.iter()? {
        let (key, value) = row?;
        let seq = key.value();
        rows.push((
            seq,
            serde_json::from_slice(value.value())
                .with_context(|| format!("decoding controller audit entry {seq}"))?,
        ));
    }
    Ok(rows)
}

fn decode_operations(
    rows: &BTreeMap<String, Vec<u8>>,
    check: bool,
) -> Result<BTreeMap<String, Record>> {
    rows.iter()
        .map(|(id, bytes)| {
            let record: Record = decode(&format!("controller operation {id}"), bytes, check)?;
            ensure!(
                record.public.operation_id == *id,
                "controller operation row {id} is keyed under the wrong ID"
            );
            Ok((id.clone(), record))
        })
        .collect()
}

fn decode_jobs(rows: &BTreeMap<String, Vec<u8>>, check: bool) -> Result<BTreeMap<String, HostJob>> {
    rows.iter()
        .map(|(id, bytes)| {
            let job: HostJob = decode(&format!("helper job {id}"), bytes, check)?;
            ensure!(
                job.job.operation_id == *id,
                "helper job row {id} is keyed under the wrong ID"
            );
            Ok((id.clone(), job))
        })
        .collect()
}

fn ensure_format(tx: &WriteTransaction, what: &str) -> Result<()> {
    let format = tx.open_table(META)?.get(FORMAT_KEY)?.map(|v| v.value());
    ensure!(
        format == Some(FORMAT_VERSION),
        "{what} is not in storage format {FORMAT_VERSION}; open it through its service first"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Controller catalog.

/// Writes a complete catalog into a transaction whose split tables are empty.
fn write_split_catalog(tx: &WriteTransaction, mut catalog: Catalog) -> Result<()> {
    let core = catalog_core(&mut catalog)?;
    {
        let mut table = tx.open_table(CATALOG_CORE)?;
        ensure!(table.is_empty()?, "controller catalog core already exists");
        table.insert(0, core.as_slice())?;
    }
    {
        let mut table = tx.open_table(OPERATIONS)?;
        ensure!(table.is_empty()?, "controller operations already exist");
        for (id, record) in &catalog.operations {
            ensure!(
                record.public.operation_id == *id,
                "controller operation {id} is keyed under the wrong ID"
            );
            table.insert(id.as_str(), serde_json::to_vec(record)?.as_slice())?;
        }
    }
    {
        let mut table = tx.open_table(REQUESTS)?;
        ensure!(table.is_empty()?, "controller requests already exist");
        for (key, id) in &catalog.requests {
            table.insert(key.as_str(), id.as_str())?;
        }
    }
    {
        let mut table = tx.open_table(AUDIT)?;
        ensure!(table.is_empty()?, "controller audit already exists");
        for (seq, entry) in (0u64..).zip(&catalog.audit) {
            table.insert(seq, serde_json::to_vec(entry)?.as_slice())?;
        }
    }
    let mut meta = tx.open_table(META)?;
    meta.insert(FORMAT_KEY, FORMAT_VERSION)?;
    meta.insert(AUDIT_NEXT_KEY, catalog.audit.len() as u64)?;
    Ok(())
}

/// Initializes an empty database with a complete catalog in the current format.
pub fn write_new_catalog(db: &Database, catalog: &Catalog) -> Result<()> {
    let mut tx = db.begin_write()?;
    tx.set_durability(Durability::Immediate)?;
    ensure!(
        tx.list_tables()?.next().is_none(),
        "refusing to initialize a controller database that already has tables"
    );
    write_split_catalog(&tx, catalog.clone())?;
    tx.commit()?;
    Ok(())
}

fn load_split_catalog(tx: &ReadTransaction, audit: bool, check: bool) -> Result<Catalog> {
    let core = load_core(&tx.open_table(CATALOG_CORE)?, "controller catalog")?;
    let mut catalog = decode_catalog_core(&core, check)?;
    catalog.operations = decode_operations(&load_rows(&tx.open_table(OPERATIONS)?)?, check)?;
    catalog.requests = load_requests(&tx.open_table(REQUESTS)?)?;
    if audit {
        catalog.audit = load_audit(&tx.open_table(AUDIT)?)?
            .into_iter()
            .map(|(_, entry)| entry)
            .collect();
    }
    Ok(catalog)
}

/// Reads the catalog without writing. With `audit` false the append-only
/// audit log is left empty.
pub fn read_catalog(db: &Database, audit: bool) -> Result<Catalog> {
    let tx = db.begin_read()?;
    check_layout(&tx, &CONTROLLER)?;
    load_split_catalog(&tx, audit, false)
}

/// Decodes every row of the catalog and refuses any field that this build
/// would drop on rewrite.
pub fn verify_catalog(db: &Database) -> Result<()> {
    let tx = db.begin_read()?;
    check_layout(&tx, &CONTROLLER)?;
    load_split_catalog(&tx, false, true)?;
    let next = tx.open_table(META)?.get(AUDIT_NEXT_KEY)?.map(|v| v.value());
    let last = tx.open_table(AUDIT)?.last()?.map(|(k, _)| k.value());
    ensure!(
        next.is_some_and(|next| last.is_none_or(|last| last < next)),
        "controller audit sequence counter is missing or behind its log"
    );
    Ok(())
}

/// Audit entries with their durable sequence numbers.
pub fn read_audit(db: &Database) -> Result<Vec<(u64, Value)>> {
    let tx = db.begin_read()?;
    check_layout(&tx, &CONTROLLER)?;
    load_audit(&tx.open_table(AUDIT)?)
}

/// A page of `(seq, entry)` audit rows and the cursor for the next page.
pub type AuditRows = (Vec<(u64, Value)>, Option<u64>);

/// One page of the audit log in sequence order, starting after `after`.
/// Stops at `limit` entries or once the encoded page would exceed `budget`
/// bytes; the returned cursor is set when more entries remain.
pub fn audit_page(
    db: &Database,
    after: Option<u64>,
    limit: usize,
    budget: usize,
) -> Result<AuditRows> {
    let tx = db.begin_read()?;
    let table = tx.open_table(AUDIT)?;
    let mut rows = match after {
        Some(after) => match after.checked_add(1) {
            Some(start) => table.range(start..)?,
            None => return Ok((Vec::new(), None)),
        },
        None => table.range(0..)?,
    };
    let mut page = Vec::new();
    let mut size = 0usize;
    for row in rows.by_ref() {
        let (key, value) = row?;
        let bytes = value.value();
        if page.len() == limit || (!page.is_empty() && size + bytes.len() > budget) {
            let last = page.last().map(|(seq, _): &(u64, Value)| *seq);
            return Ok((page, last));
        }
        size += bytes.len() + 32;
        page.push((key.value(), serde_json::from_slice(bytes)?));
    }
    Ok((page, None))
}

/// An open write transaction on a current-format catalog, holding the stored
/// bytes so that only changed rows are rewritten.
pub(crate) struct CatalogWrite {
    tx: WriteTransaction,
    core: Vec<u8>,
    operations: BTreeMap<String, Vec<u8>>,
    requests: BTreeMap<String, String>,
}

impl CatalogWrite {
    /// Begins a durable write and loads the catalog without its audit log.
    pub(crate) fn begin(db: &Database) -> Result<(Self, Catalog)> {
        let mut tx = db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        ensure_format(&tx, "controller catalog")?;
        let core = load_core(&tx.open_table(CATALOG_CORE)?, "controller catalog")?;
        let operations = load_rows(&tx.open_table(OPERATIONS)?)?;
        let requests = load_requests(&tx.open_table(REQUESTS)?)?;
        let mut catalog = decode_catalog_core(&core, false)?;
        catalog.operations = decode_operations(&operations, false)?;
        catalog.requests = requests.clone();
        Ok((
            Self {
                tx,
                core,
                operations,
                requests,
            },
            catalog,
        ))
    }

    /// Commits the rows that differ from what was loaded, bumping the
    /// revision, or aborts when nothing changed. Audit entries pushed onto the
    /// catalog are appended. Returns whether anything was committed.
    pub(crate) fn finish(self, catalog: &mut Catalog) -> Result<bool> {
        let audit = std::mem::take(&mut catalog.audit);
        let result = self.write(catalog, &audit);
        catalog.audit = audit;
        result
    }

    fn write(self, catalog: &mut Catalog, audit: &[Value]) -> Result<bool> {
        let mut operations = Vec::new();
        for (id, record) in &catalog.operations {
            let bytes = serde_json::to_vec(record)?;
            if self.operations.get(id) != Some(&bytes) {
                ensure!(
                    record.public.operation_id == *id,
                    "controller operation {id} is keyed under the wrong ID"
                );
                operations.push((id.clone(), bytes));
            }
        }
        let removed: Vec<&str> = self
            .operations
            .keys()
            .filter(|id| !catalog.operations.contains_key(*id))
            .map(String::as_str)
            .collect();
        let requests: Vec<(String, String)> = catalog
            .requests
            .iter()
            .filter(|(key, id)| self.requests.get(*key) != Some(*id))
            .map(|(key, id)| (key.clone(), id.clone()))
            .collect();
        let forgotten: Vec<&str> = self
            .requests
            .keys()
            .filter(|key| !catalog.requests.contains_key(*key))
            .map(String::as_str)
            .collect();
        let core_changed = catalog_core(catalog)? != self.core;
        if !core_changed
            && operations.is_empty()
            && removed.is_empty()
            && requests.is_empty()
            && forgotten.is_empty()
            && audit.is_empty()
        {
            self.tx.abort()?;
            return Ok(false);
        }
        catalog.revision = catalog
            .revision
            .checked_add(1)
            .context("catalog revision exhausted")?;
        let core = catalog_core(catalog)?;
        {
            self.tx
                .open_table(CATALOG_CORE)?
                .insert(0, core.as_slice())?;
            let mut table = self.tx.open_table(OPERATIONS)?;
            for (id, bytes) in &operations {
                table.insert(id.as_str(), bytes.as_slice())?;
            }
            for id in removed {
                table.remove(id)?;
            }
            let mut table = self.tx.open_table(REQUESTS)?;
            for (key, id) in &requests {
                table.insert(key.as_str(), id.as_str())?;
            }
            for key in forgotten {
                table.remove(key)?;
            }
            if !audit.is_empty() {
                let mut meta = self.tx.open_table(META)?;
                let mut next = meta
                    .get(AUDIT_NEXT_KEY)?
                    .context("controller audit sequence counter missing")?
                    .value();
                let mut table = self.tx.open_table(AUDIT)?;
                for entry in audit {
                    table.insert(next, serde_json::to_vec(entry)?.as_slice())?;
                    next = next.checked_add(1).context("audit sequence exhausted")?;
                }
                meta.insert(AUDIT_NEXT_KEY, next)?;
            }
        }
        self.tx.commit()?;
        Ok(true)
    }
}

/// Removes the given operations and audit entries in one durable transaction.
/// `validate` sees the resulting catalog (without audit) and can veto the
/// commit. Request tombstones are deliberately kept.
pub fn prune_catalog(
    db: &Database,
    operations: &BTreeSet<String>,
    audit: &BTreeSet<u64>,
    validate: impl FnOnce(&Catalog) -> Result<()>,
) -> Result<()> {
    let mut tx = db.begin_write()?;
    tx.set_durability(Durability::Immediate)?;
    ensure_format(&tx, "controller catalog")?;
    {
        let mut table = tx.open_table(OPERATIONS)?;
        for id in operations {
            ensure!(
                table.remove(id.as_str())?.is_some(),
                "operation {id} vanished before pruning"
            );
        }
        let mut table = tx.open_table(AUDIT)?;
        for seq in audit {
            ensure!(
                table.remove(*seq)?.is_some(),
                "audit entry {seq} vanished before pruning"
            );
        }
    }
    let catalog = {
        let core = load_core(&tx.open_table(CATALOG_CORE)?, "controller catalog")?;
        let mut catalog = decode_catalog_core(&core, false)?;
        catalog.operations = decode_operations(&load_rows(&tx.open_table(OPERATIONS)?)?, false)?;
        catalog.requests = load_requests(&tx.open_table(REQUESTS)?)?;
        catalog
    };
    validate(&catalog)?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper journal.

fn write_split_journal(tx: &WriteTransaction, mut journal: Journal) -> Result<()> {
    let core = journal_core(&mut journal)?;
    tx.open_table(JOURNAL_CORE)?.insert(0, core.as_slice())?;
    {
        let mut table = tx.open_table(JOBS)?;
        for (id, job) in &journal.jobs {
            ensure!(
                job.job.operation_id == *id,
                "helper job {id} is keyed under the wrong ID"
            );
            table.insert(id.as_str(), serde_json::to_vec(job)?.as_slice())?;
        }
    }
    tx.open_table(META)?.insert(FORMAT_KEY, FORMAT_VERSION)?;
    Ok(())
}

fn load_split_journal(tx: &ReadTransaction, check: bool) -> Result<Journal> {
    let core = load_core(&tx.open_table(JOURNAL_CORE)?, "helper journal")?;
    let mut journal = decode_journal_core(&core, check)?;
    journal.jobs = decode_jobs(&load_rows(&tx.open_table(JOBS)?)?, check)?;
    Ok(journal)
}

/// Reads the whole journal without writing.
pub fn read_journal(db: &Database) -> Result<Journal> {
    let tx = db.begin_read()?;
    check_layout(&tx, &HELPER)?;
    load_split_journal(&tx, false)
}

/// Decodes every row of the journal and refuses any field that this build
/// would drop on rewrite.
pub fn verify_journal(db: &Database) -> Result<()> {
    let tx = db.begin_read()?;
    check_layout(&tx, &HELPER)?;
    load_split_journal(&tx, true)?;
    Ok(())
}

/// Replaces the whole journal with `journal` in the current format. Used to
/// initialize and by offline tooling.
pub fn write_journal(db: &Database, journal: &Journal) -> Result<()> {
    let mut tx = db.begin_write()?;
    tx.set_durability(Durability::Immediate)?;
    for name in tx
        .list_tables()?
        .map(|t| t.name().to_owned())
        .collect::<Vec<_>>()
    {
        ensure!(
            HELPER.owned.contains(&name.as_str()),
            "helper journal contains foreign table {name}"
        );
    }
    tx.delete_table(JOURNAL_CORE)?;
    tx.delete_table(JOBS)?;
    tx.delete_table(META)?;
    write_split_journal(&tx, journal.clone())?;
    tx.commit()?;
    Ok(())
}

/// An open write transaction on a current-format journal.
pub(crate) struct JournalWrite {
    tx: WriteTransaction,
    core: Vec<u8>,
    jobs: BTreeMap<String, Vec<u8>>,
}

impl JournalWrite {
    pub(crate) fn begin(db: &Database) -> Result<(Self, Journal)> {
        let mut tx = db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        ensure_format(&tx, "helper journal")?;
        let core = load_core(&tx.open_table(JOURNAL_CORE)?, "helper journal")?;
        let jobs = load_rows(&tx.open_table(JOBS)?)?;
        let mut journal = decode_journal_core(&core, false)?;
        journal.jobs = decode_jobs(&jobs, false)?;
        Ok((Self { tx, core, jobs }, journal))
    }

    /// Commits the rows that differ from what was loaded; aborts if none do.
    pub(crate) fn finish(self, journal: &mut Journal) -> Result<()> {
        let core = journal_core(journal)?;
        let mut jobs = Vec::new();
        for (id, job) in &journal.jobs {
            let bytes = serde_json::to_vec(job)?;
            if self.jobs.get(id) != Some(&bytes) {
                ensure!(
                    job.job.operation_id == *id,
                    "helper job {id} is keyed under the wrong ID"
                );
                jobs.push((id.as_str(), bytes));
            }
        }
        let removed: Vec<&str> = self
            .jobs
            .keys()
            .filter(|id| !journal.jobs.contains_key(*id))
            .map(String::as_str)
            .collect();
        if core == self.core && jobs.is_empty() && removed.is_empty() {
            self.tx.abort()?;
            return Ok(());
        }
        {
            if core != self.core {
                self.tx
                    .open_table(JOURNAL_CORE)?
                    .insert(0, core.as_slice())?;
            }
            let mut table = self.tx.open_table(JOBS)?;
            for (id, bytes) in &jobs {
                table.insert(*id, bytes.as_slice())?;
            }
            for id in removed {
                table.remove(id)?;
            }
        }
        self.tx.commit()?;
        Ok(())
    }
}

/// Removes the given finished jobs in one durable transaction; `validate`
/// sees the resulting journal and can veto the commit.
pub fn prune_journal(
    db: &Database,
    jobs: &BTreeSet<String>,
    validate: impl FnOnce(&Journal) -> Result<()>,
) -> Result<()> {
    let mut tx = db.begin_write()?;
    tx.set_durability(Durability::Immediate)?;
    ensure_format(&tx, "helper journal")?;
    {
        let mut table = tx.open_table(JOBS)?;
        for id in jobs {
            ensure!(
                table.remove(id.as_str())?.is_some(),
                "helper job {id} vanished before pruning"
            );
        }
    }
    let journal = {
        let core = load_core(&tx.open_table(JOURNAL_CORE)?, "helper journal")?;
        let mut journal = decode_journal_core(&core, false)?;
        journal.jobs = decode_jobs(&load_rows(&tx.open_table(JOBS)?)?, false)?;
        journal
    };
    validate(&journal)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dropped_field_detection_ignores_nulls_and_added_defaults() {
        let stored = json!({"a": 1, "b": null, "c": {"d": [1, {"e": 2}]}});
        assert_eq!(
            dropped(&stored, &json!({"a": 1, "c": {"d": [1, {"e": 2}]}}), ""),
            None
        );
        assert_eq!(
            dropped(
                &stored,
                &json!({"a": 1, "c": {"d": [1, {"e": 2, "f": 3}]}, "g": 4}),
                ""
            ),
            None
        );
        assert_eq!(
            dropped(&stored, &json!({"a": 1, "c": {"d": [1, {}]}}), "").as_deref(),
            Some("/c/d/1/e")
        );
        assert_eq!(
            dropped(&stored, &json!({"c": {"d": [1, {"e": 2}]}}), "").as_deref(),
            Some("/a")
        );
    }

    #[test]
    fn layouts_are_classified_strictly() {
        let names = |n: &[&str]| n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let retired = classify(&CONTROLLER, &names(&["controller_catalog_v1"]), None)
            .unwrap_err()
            .to_string();
        assert!(
            retired.contains("format 1") && retired.contains(LAST_MIGRATING_BUILD),
            "{retired}"
        );
        classify(&CONTROLLER, &names(CONTROLLER.owned), Some(FORMAT_VERSION)).unwrap();
        let newer = classify(
            &CONTROLLER,
            &names(CONTROLLER.owned),
            Some(FORMAT_VERSION + 1),
        )
        .unwrap_err()
        .to_string();
        assert!(newer.contains("newer Hearth"), "{newer}");
        assert!(classify(&CONTROLLER, &names(&[]), None).is_err());
        assert!(classify(&CONTROLLER, &names(&["controller_core_v2"]), None).is_err());
        let mut mixed = names(CONTROLLER.owned);
        mixed.push("controller_catalog_v1".into());
        assert!(classify(&CONTROLLER, &mixed, Some(FORMAT_VERSION)).is_err());
        let mut foreign = names(HELPER.owned);
        foreign.push("host_widgets_v3".into());
        assert!(classify(&HELPER, &foreign, Some(FORMAT_VERSION)).is_err());
    }
}
