//! Retention of finished history: deciding what may be pruned, and the JSONL
//! archive that every pruned record is exported to first.
//!
//! The offline `pve-dev-ops prune` command (see
//! [`super::maintenance::prune`]) drives this with both databases locked.
//! Nothing here deletes anything; [`plan`] only selects records.
//!
//! # What is never pruned
//!
//! * Operations that are not terminal (`queued`, `running`, `blocked`), and
//!   operations whose helper job is not terminal.
//! * Operations that changed within the retention window.
//! * Operations named anywhere in the controller's or the helper's core state
//!   (shrink and growth holds, migrations, account and machine records, ...),
//!   plus each migration's derived staging-create operation.
//! * Verified enrollments' proofs, and the deletion proof of every account
//!   that is no longer active (backups refuse a tombstone without it).
//! * Anything connected to a retained operation by reference, in either
//!   direction: a recovery and the operation it recovers, a shrink and its
//!   finish. The catalog validates these pairs together, so they are kept or
//!   pruned as a group.
//! * Request-ID records. `operation_id` is derived from the requesting user
//!   and request ID, and both the controller and the helper deduplicate by it,
//!   so dropping the mapping would let a reused request ID mint the ID of an
//!   archived operation and run it again. The mapping is tiny and is kept
//!   forever; a reuse of an archived request ID is refused.
//!
//! # Helper jobs
//!
//! The controller resubmits every non-terminal operation to the helper on
//! each pass, and the helper answers a resubmission from its journal by
//! operation ID instead of running it again. A helper job may therefore only
//! go once the controller can never submit that ID again: the controller
//! operation must already be pruned (or be pruned in the same run) while its
//! request-ID tombstone remains. Helper jobs are kept when non-terminal, when
//! their controller operation is retained, or when helper core state or a
//! retained job refers to them. The controller is always pruned first, and
//! the backup pair check accepts a finished helper job whose controller
//! operation survives only as a tombstone, so an interrupted run leaves a
//! valid pair and simply finishing it later converges.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::host::{HostJob, Journal};
use super::protocol::AccountState;
use super::store::{Catalog, Record};
use crate::control::JobKind;

/// Default age after which finished history may be pruned.
pub const DEFAULT_RETENTION_DAYS: u64 = 180;
/// Version of the JSONL archive layout written by [`write_archive`].
pub const ARCHIVE_FORMAT: u32 = 1;

/// Records selected for pruning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub operations: BTreeSet<String>,
    pub audit: BTreeSet<u64>,
    pub helper_jobs: BTreeSet<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty() && self.audit.is_empty() && self.helper_jobs.is_empty()
    }
}

fn collect_strings<'a>(value: &'a Value, out: &mut BTreeSet<&'a str>) {
    match value {
        Value::String(s) => {
            out.insert(s);
        }
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => {
            for (key, value) in map {
                out.insert(key);
                collect_strings(value, out);
            }
        }
        _ => {}
    }
}

fn core_value(value: impl Serialize, split: &[&str]) -> Result<Value> {
    let mut value = serde_json::to_value(value)?;
    let map = value.as_object_mut().context("state is not an object")?;
    for key in split {
        map.remove(*key);
    }
    Ok(value)
}

/// Latest time either side recorded activity for an operation.
fn last_activity(record: &Record, job: Option<&HostJob>) -> u64 {
    let mut at = record.public.created_at;
    if let Some(progress) = &record.public.progress {
        at = at.max(progress.updated_at);
        at = at.max(progress.finished_at.unwrap_or(0));
    }
    if let Some(job) = job {
        for value in [job.started_at, job.updated_at, job.finished_at]
            .into_iter()
            .flatten()
        {
            at = at.max(value);
        }
    }
    at
}

/// Undirected closure of `roots` over `edges`.
fn closure<'a>(
    roots: BTreeSet<&'a str>,
    edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
) -> BTreeSet<&'a str> {
    let mut seen = roots.clone();
    let mut queue: VecDeque<&str> = roots.into_iter().collect();
    while let Some(id) = queue.pop_front() {
        for next in edges.get(id).into_iter().flatten() {
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    seen
}

fn link<'a>(edges: &mut BTreeMap<&'a str, BTreeSet<&'a str>>, a: &'a str, b: &'a str) {
    if a != b {
        edges.entry(a).or_default().insert(b);
        edges.entry(b).or_default().insert(a);
    }
}

/// Selects the operations, audit entries and helper jobs that may be pruned
/// because they finished before `cutoff` (seconds since the epoch). See the
/// module documentation for what is always kept.
pub fn plan(
    catalog: &Catalog,
    audit: &[(u64, Value)],
    journal: &Journal,
    cutoff: u64,
) -> Result<Plan> {
    let ids: BTreeSet<&str> = catalog.operations.keys().map(String::as_str).collect();
    let mut roots = BTreeSet::new();
    for (id, record) in &catalog.operations {
        let job = journal.jobs.get(id);
        if !record.public.status.is_terminal()
            || job.is_some_and(|job| !job.status.is_terminal())
            || last_activity(record, job) >= cutoff
        {
            roots.insert(id.as_str());
        }
        let account_released = catalog
            .users
            .get(&record.public.owner)
            .is_none_or(|user| user.state != AccountState::Active);
        if (record.public.kind == JobKind::DeleteUser && account_released)
            || (record.public.kind == JobKind::EnrollVm
                && record
                    .public
                    .vm_id
                    .is_some_and(|vm| catalog.enrollments.contains_key(&vm)))
        {
            roots.insert(id.as_str());
        }
    }
    let controller_core = core_value(catalog, &["operations", "requests", "audit"])?;
    let helper_core = core_value(journal, &["jobs"])?;
    let mut referenced = BTreeSet::new();
    collect_strings(&controller_core, &mut referenced);
    collect_strings(&helper_core, &mut referenced);
    let staging: Vec<String> = catalog
        .migrations
        .keys()
        .chain(journal.migrations.keys())
        .map(|id| super::migration_transport::target_create_id(id))
        .collect();
    referenced.extend(staging.iter().map(String::as_str));
    roots.extend(ids.intersection(&referenced).copied());

    let records: Vec<(&str, Value)> = catalog
        .operations
        .iter()
        .map(|(id, record)| Ok((id.as_str(), serde_json::to_value(record)?)))
        .chain(
            journal
                .jobs
                .iter()
                .map(|(id, job)| Ok((id.as_str(), serde_json::to_value(job)?))),
        )
        .collect::<Result<_>>()?;
    let mut edges = BTreeMap::new();
    for (id, value) in &records {
        let mut strings = BTreeSet::new();
        collect_strings(value, &mut strings);
        for other in ids.intersection(&strings) {
            link(&mut edges, id, other);
        }
    }
    let kept = closure(roots, &edges);
    let operations: BTreeSet<String> = ids.difference(&kept).map(|id| (*id).to_owned()).collect();

    // Helper side: only jobs whose controller operation is gone (now or
    // earlier) while its request-ID tombstone remains.
    let tombstones: BTreeSet<&str> = catalog.requests.values().map(String::as_str).collect();
    let jobs: BTreeSet<&str> = journal.jobs.keys().map(String::as_str).collect();
    let mut helper_roots = BTreeSet::new();
    for (id, job) in &journal.jobs {
        if !job.status.is_terminal()
            || kept.contains(id.as_str())
            || !tombstones.contains(id.as_str())
        {
            helper_roots.insert(id.as_str());
        }
    }
    helper_roots.extend(jobs.intersection(&referenced).copied());
    let mut helper_edges = BTreeMap::new();
    for (id, value) in &records {
        if !jobs.contains(id) {
            continue;
        }
        let mut strings = BTreeSet::new();
        collect_strings(value, &mut strings);
        for other in jobs.intersection(&strings) {
            link(&mut helper_edges, id, other);
        }
    }
    let helper_kept = closure(helper_roots, &helper_edges);
    let helper_jobs = jobs
        .difference(&helper_kept)
        .map(|id| (*id).to_owned())
        .collect();

    let audit = audit
        .iter()
        .filter(|(_, entry)| entry["at"].as_u64().is_some_and(|at| at < cutoff))
        .map(|(seq, _)| *seq)
        .collect();
    Ok(Plan {
        operations,
        audit,
        helper_jobs,
    })
}

/// First line of an archive.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArchiveHeader {
    pub format: u32,
    pub environment: String,
    pub created_at: u64,
    /// Records that finished before this time were eligible.
    pub cutoff: u64,
    pub controller_revision: u64,
    pub helper_sequence: u64,
}

/// Contents of a verified archive.
#[derive(Debug, Default)]
pub struct Archive {
    pub header: Option<ArchiveHeader>,
    pub operations: Vec<Record>,
    pub helper_jobs: Vec<HostJob>,
    pub audit: Vec<(u64, Value)>,
    pub sha256: String,
}

struct Hashing<W> {
    inner: W,
    digest: Sha256,
}

impl<W: Write> Hashing<W> {
    fn line(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.digest.update(&bytes);
        self.inner.write_all(&bytes)?;
        Ok(())
    }
}

/// Writes every record in `plan` as JSON lines: a header, one line per
/// operation, helper job and audit entry, and a footer carrying the counts
/// and the SHA-256 of all preceding bytes, so truncation is detectable.
/// Returns that digest. The caller creates, syncs and names the file.
pub fn write_archive(
    out: impl Write,
    header: &ArchiveHeader,
    catalog: &Catalog,
    audit: &[(u64, Value)],
    journal: &Journal,
    plan: &Plan,
) -> Result<String> {
    let mut out = Hashing {
        inner: out,
        digest: Sha256::new(),
    };
    let mut header_value = serde_json::to_value(header)?;
    header_value["kind"] = json!("header");
    out.line(&header_value)?;
    for id in &plan.operations {
        let record = catalog
            .operations
            .get(id)
            .with_context(|| format!("operation {id} missing while archiving"))?;
        out.line(&json!({"kind": "operation", "id": id, "record": record}))?;
    }
    for id in &plan.helper_jobs {
        let job = journal
            .jobs
            .get(id)
            .with_context(|| format!("helper job {id} missing while archiving"))?;
        out.line(&json!({"kind": "helper_job", "id": id, "job": job}))?;
    }
    let entries: BTreeMap<u64, &Value> = audit.iter().map(|(seq, entry)| (*seq, entry)).collect();
    for seq in &plan.audit {
        let entry = entries
            .get(seq)
            .with_context(|| format!("audit entry {seq} missing while archiving"))?;
        out.line(&json!({"kind": "audit", "seq": seq, "entry": entry}))?;
    }
    let sha256 = format!("{:x}", out.digest.clone().finalize());
    let footer = json!({
        "kind": "footer",
        "operations": plan.operations.len(),
        "helper_jobs": plan.helper_jobs.len(),
        "audit": plan.audit.len(),
        "sha256": sha256,
    });
    let mut bytes = serde_json::to_vec(&footer)?;
    bytes.push(b'\n');
    out.inner.write_all(&bytes)?;
    out.inner.flush()?;
    Ok(sha256)
}

/// Reads an archive and checks its footer digest and counts.
pub fn read_archive(input: impl Read) -> Result<Archive> {
    let mut archive = Archive::default();
    let mut digest = Sha256::new();
    let mut footer = None;
    for line in BufReader::new(input).split(b'\n') {
        let line = line?;
        ensure!(footer.is_none(), "archive has data after its footer");
        let value: Value = serde_json::from_slice(&line).context("malformed archive line")?;
        let kind = value["kind"].as_str().unwrap_or_default().to_owned();
        if kind != "footer" {
            digest.update(&line);
            digest.update(b"\n");
        }
        match kind.as_str() {
            "header" => {
                ensure!(
                    archive.header.is_none() && archive.operations.is_empty(),
                    "archive header is not first"
                );
                let header: ArchiveHeader = serde_json::from_value(value)?;
                ensure!(
                    header.format == ARCHIVE_FORMAT,
                    "unsupported archive format {}",
                    header.format
                );
                archive.header = Some(header);
            }
            "operation" => archive
                .operations
                .push(serde_json::from_value(value["record"].clone())?),
            "helper_job" => archive
                .helper_jobs
                .push(serde_json::from_value(value["job"].clone())?),
            "audit" => archive.audit.push((
                value["seq"].as_u64().context("audit line lacks seq")?,
                value["entry"].clone(),
            )),
            "footer" => footer = Some(value),
            other => bail!("unknown archive line kind {other:?}"),
        }
    }
    let footer = footer.context("archive has no footer; it may be truncated")?;
    ensure!(archive.header.is_some(), "archive has no header");
    archive.sha256 = format!("{:x}", digest.finalize());
    ensure!(
        footer["sha256"] == json!(archive.sha256)
            && footer["operations"] == json!(archive.operations.len())
            && footer["helper_jobs"] == json!(archive.helper_jobs.len())
            && footer["audit"] == json!(archive.audit.len()),
        "archive footer does not match its contents"
    );
    Ok(archive)
}
