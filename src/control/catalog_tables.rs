//! Durable layout of the controller [`Catalog`]: one redb table per entity.
//!
//! The in-memory `Catalog` is unchanged; this module maps it onto tables and
//! writes only the rows a change actually touched. Values are JSON so the
//! row format matches the API and backups. Every table carries a `_v2`
//! suffix; the retired single-row `controller_catalog_v1` layout is refused,
//! not converted.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use redb::{
    ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, TableHandle,
    WriteTransaction,
};
use serde::{Serialize, de::DeserializeOwned};

use super::store::Catalog;

pub const LEGACY_TABLE: &str = "controller_catalog_v1";

pub const META: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_meta_v2");
pub const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_users_v2");
pub const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_devices_v2");
pub const VMS: TableDefinition<u16, &[u8]> = TableDefinition::new("catalog_vms_v2");
pub const OPERATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_operations_v2");
pub const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_requests_v2");
pub const AUDIT: TableDefinition<u64, &[u8]> = TableDefinition::new("catalog_audit_v2");
pub const DISK_SHRINKS: TableDefinition<u16, &[u8]> =
    TableDefinition::new("catalog_disk_shrinks_v2");
pub const ENROLLMENT_PLANS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("catalog_enrollment_plans_v2");
pub const DISK_GROWTHS: TableDefinition<u16, &[u8]> =
    TableDefinition::new("catalog_disk_growths_v2");
pub const ENROLLMENTS: TableDefinition<u16, &[u8]> = TableDefinition::new("catalog_enrollments_v2");
pub const MIGRATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_migrations_v2");

/// Every table this layout owns, for foreign-table checks.
pub const TABLE_NAMES: [&str; 12] = [
    "catalog_meta_v2",
    "catalog_users_v2",
    "catalog_devices_v2",
    "catalog_vms_v2",
    "catalog_operations_v2",
    "catalog_requests_v2",
    "catalog_audit_v2",
    "catalog_disk_shrinks_v2",
    "catalog_enrollment_plans_v2",
    "catalog_disk_growths_v2",
    "catalog_enrollments_v2",
    "catalog_migrations_v2",
];

const META_KEYS: [&str; 4] = ["schema", "binding", "imported_vms", "revision"];

fn decode<T: DeserializeOwned>(
    table: &str,
    key: impl std::fmt::Display,
    bytes: &[u8],
) -> Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("decoding {table} row {key}"))
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

/// Reads every row of every table into a `Catalog`. Works on read and write
/// transactions alike, since both expose `open_table`.
macro_rules! load_catalog {
    ($tx:expr) => {{
        let tx = $tx;
        let meta = tx.open_table(META)?;
        let field = |key: &str| -> Result<Option<serde_json::Value>> {
            meta.get(key)?
                .map(|row| decode("catalog_meta_v2", key, row.value()))
                .transpose()
        };
        let schema =
            field("schema")?.context("controller catalog missing; refusing initialization")?;
        let mut object = serde_json::Map::new();
        object.insert("schema".into(), schema);
        for key in ["binding", "revision"] {
            object.insert(
                key.into(),
                field(key)?.with_context(|| format!("catalog meta {key} missing"))?,
            );
        }
        if let Some(imported) = field("imported_vms")? {
            object.insert("imported_vms".into(), imported);
        }
        macro_rules! rows {
            ($def:expr, $name:literal, $key:ty) => {{
                let table = tx.open_table($def)?;
                let mut rows = serde_json::Map::new();
                for row in table.iter()? {
                    let (key, value) = row?;
                    let key: $key = key.value();
                    rows.insert(key.to_string(), decode($name, &key, value.value())?);
                }
                serde_json::Value::Object(rows)
            }};
        }
        object.insert("users".into(), rows!(USERS, "catalog_users_v2", &str));
        object.insert("devices".into(), rows!(DEVICES, "catalog_devices_v2", &str));
        object.insert("vms".into(), rows!(VMS, "catalog_vms_v2", u16));
        object.insert(
            "operations".into(),
            rows!(OPERATIONS, "catalog_operations_v2", &str),
        );
        object.insert(
            "requests".into(),
            rows!(REQUESTS, "catalog_requests_v2", &str),
        );
        object.insert(
            "disk_shrinks".into(),
            rows!(DISK_SHRINKS, "catalog_disk_shrinks_v2", u16),
        );
        object.insert(
            "enrollment_plans".into(),
            rows!(ENROLLMENT_PLANS, "catalog_enrollment_plans_v2", &str),
        );
        object.insert(
            "disk_growths".into(),
            rows!(DISK_GROWTHS, "catalog_disk_growths_v2", u16),
        );
        object.insert(
            "enrollments".into(),
            rows!(ENROLLMENTS, "catalog_enrollments_v2", u16),
        );
        object.insert(
            "migrations".into(),
            rows!(MIGRATIONS, "catalog_migrations_v2", &str),
        );
        let audit = tx.open_table(AUDIT)?;
        let mut entries = Vec::new();
        for (expected, row) in (0u64..).zip(audit.iter()?) {
            let (key, value) = row?;
            ensure!(key.value() == expected, "audit log has a gap at {expected}");
            entries.push(decode::<serde_json::Value>(
                "catalog_audit_v2",
                expected,
                value.value(),
            )?);
        }
        object.insert("audit".into(), serde_json::Value::Array(entries));
        let catalog: Catalog = serde_json::from_value(serde_json::Value::Object(object))
            .context("assembling controller catalog")?;
        Ok::<Catalog, anyhow::Error>(catalog)
    }};
}

pub fn load_read(tx: &ReadTransaction) -> Result<Catalog> {
    load_catalog!(tx)
}

pub fn load_write(tx: &WriteTransaction) -> Result<Catalog> {
    load_catalog!(tx)
}

/// Refuses databases in the retired layout or carrying tables this build does
/// not own, before anything reads them as a catalog.
pub fn check_layout(database: &redb::Database) -> Result<()> {
    let tx = database.begin_read()?;
    for table in tx.list_tables()? {
        let name = table.name();
        if name == LEGACY_TABLE {
            bail!(
                "controller database uses the retired single-record layout; \
                 convert it with the cutover procedure instead of opening it"
            );
        }
        ensure!(
            TABLE_NAMES.contains(&name),
            "controller database has foreign table {name}"
        );
    }
    Ok(())
}

/// Writes rows of `after` that differ from `before`, and deletes rows that
/// disappeared. With `before = None` every row is written. Returns whether
/// anything was written.
pub fn write_changes(
    tx: &WriteTransaction,
    before: Option<&Catalog>,
    after: &Catalog,
) -> Result<bool> {
    let mut changed = false;
    {
        let mut meta = tx.open_table(META)?;
        let old = before.map(meta_values).transpose()?;
        let new = meta_values(after)?;
        for key in META_KEYS {
            let value = &new[key];
            if old.as_ref().and_then(|old| old.get(key)) == Some(value) {
                continue;
            }
            changed = true;
            match value {
                Some(bytes) => {
                    meta.insert(key, bytes.as_slice())?;
                }
                None => {
                    meta.remove(key)?;
                }
            }
        }
    }
    macro_rules! row_key {
        (str, $key:expr) => {
            $key.as_str()
        };
        (u16, $key:expr) => {
            *$key
        };
    }
    macro_rules! sync {
        ($def:expr, $field:ident, $kind:ident) => {{
            let mut table = tx.open_table($def)?;
            let empty = BTreeMap::new();
            let old = before.map_or(&empty, |c| &c.$field);
            for (key, value) in &after.$field {
                let bytes = encode(value)?;
                if old.get(key).map(encode).transpose()?.as_deref() != Some(bytes.as_slice()) {
                    table.insert(row_key!($kind, key), bytes.as_slice())?;
                    changed = true;
                }
            }
            for key in old.keys().filter(|key| !after.$field.contains_key(*key)) {
                table.remove(row_key!($kind, key))?;
                changed = true;
            }
        }};
    }
    sync!(USERS, users, str);
    sync!(DEVICES, devices, str);
    sync!(VMS, vms, u16);
    sync!(OPERATIONS, operations, str);
    sync!(REQUESTS, requests, str);
    sync!(DISK_SHRINKS, disk_shrinks, u16);
    sync!(ENROLLMENT_PLANS, enrollment_plans, str);
    sync!(DISK_GROWTHS, disk_growths, u16);
    sync!(ENROLLMENTS, enrollments, u16);
    sync!(MIGRATIONS, migrations, str);
    {
        let mut audit = tx.open_table(AUDIT)?;
        let old: &[serde_json::Value] = before.map_or(&[], |c| c.audit.as_slice());
        for (index, value) in after.audit.iter().enumerate() {
            if old.get(index) != Some(value) {
                audit.insert(index as u64, encode(value)?.as_slice())?;
                changed = true;
            }
        }
        for index in after.audit.len()..old.len() {
            audit.remove(index as u64)?;
            changed = true;
        }
    }
    Ok(changed)
}

fn meta_values(catalog: &Catalog) -> Result<BTreeMap<&'static str, Option<Vec<u8>>>> {
    Ok(BTreeMap::from([
        ("schema", Some(encode(&catalog.schema)?)),
        ("binding", Some(encode(&catalog.binding)?)),
        (
            "imported_vms",
            (!catalog.imported_vms.is_empty())
                .then(|| encode(&catalog.imported_vms))
                .transpose()?,
        ),
        ("revision", Some(encode(&catalog.revision)?)),
    ]))
}

/// Creates every table so that read transactions can open them.
pub fn create_tables(tx: &WriteTransaction) -> Result<()> {
    tx.open_table(META)?;
    tx.open_table(USERS)?;
    tx.open_table(DEVICES)?;
    tx.open_table(VMS)?;
    tx.open_table(OPERATIONS)?;
    tx.open_table(REQUESTS)?;
    tx.open_table(AUDIT)?;
    tx.open_table(DISK_SHRINKS)?;
    tx.open_table(ENROLLMENT_PLANS)?;
    tx.open_table(DISK_GROWTHS)?;
    tx.open_table(ENROLLMENTS)?;
    tx.open_table(MIGRATIONS)?;
    Ok(())
}
