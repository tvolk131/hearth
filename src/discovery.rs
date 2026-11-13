//! Read-only discovery and observation import.
//!
//! This module validates an observation snapshot produced by the embedded
//! read-only collector, proposes an *unconfirmed* ownership mapping, and imports
//! explicitly confirmed mappings into a dedicated observation-only redb file.
//!
//! Nothing in this module creates controller users, VMs or operations or writes
//! to Proxmox. The explicit `observation` flow executes only the fixed collector
//! over pinned SSH. Every observation record carries
//! `managed: false` and `authorizes_management: false`; those fields cannot be
//! deserialized as `true`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition, TableHandle};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub mod observation;

pub const SNAPSHOT_SCHEMA: &str = "pve-dev-discovery-snapshot/1";
pub const MAPPING_SCHEMA: &str = "pve-dev-discovery-mapping/1";
pub const IMPORT_SCHEMA: &str = "pve-dev-discovery-import/1";
pub const COLLECTOR_VERSION: &str = "pve-dev-discovery-collector/2";
pub const STORE_KIND: &str = "pve-dev-discovery-observations/1";
pub const DEFAULT_ZFS_ROOT: &str = "rpool/data";

pub const META_TABLE: &str = "discovery_meta_v1";
pub const HOSTS_TABLE: &str = "discovery_hosts_v1";
pub const SNAPSHOTS_TABLE: &str = "discovery_snapshots_v1";
pub const IMPORTS_TABLE: &str = "discovery_imports_v1";
pub const LOG_TABLE: &str = "discovery_log_v1";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new(META_TABLE);
const HOSTS: TableDefinition<&str, &[u8]> = TableDefinition::new(HOSTS_TABLE);
const SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new(SNAPSHOTS_TABLE);
const IMPORTS: TableDefinition<&str, &[u8]> = TableDefinition::new(IMPORTS_TABLE);
const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new(LOG_TABLE);

/// Collector template. `__ZFS_ROOT__` is replaced with a validated JSON string.
const COLLECTOR_TEMPLATE: &str = r#"import datetime
import hashlib
import json
import re
import subprocess
import sys

COLLECTOR = "pve-dev-discovery-collector/2"
ZFS_ROOT = __ZFS_ROOT__
MACHINE_ID_PATH = "/etc/machine-id"
HOST_KEY_PATH = "/etc/ssh/ssh_host_ed25519_key.pub"
STRUCTURAL = ("type", "guid", "quota", "reservation", "refreservation", "volsize", "origin")
DATASET_PROPS = STRUCTURAL + ("used", "available", "mountpoint", "canmount")
SNAPSHOT_PROPS = ("guid", "used", "createtxg")
VM_KEYS = re.compile(
    r"^(name|memory|balloon|cores|sockets|vcpus|cpu|cpuunits|cpulimit|machine|ostype|bios"
    r"|boot|scsihw|onboot|protection|tags|template|lock|digest|agent|numa|hotplug|kvm"
    r"|startup|vga|nameserver|searchdomain|ciuser|ciupgrade|citype|efidisk0|tpmstate0"
    r"|(ide|sata|scsi|virtio|unused|net|ipconfig|serial)[0-9]+)$"
)
CT_KEYS = re.compile(r"^(hostname|digest|lock|template|rootfs|(mp|unused)[0-9]+)$")
STORAGE_KEYS = ("type", "pool", "path", "content", "nodes", "sparse", "disable", "blocksize", "shared")
IFACE_KEYS = ("type", "cidr", "address", "netmask", "gateway", "bridge_ports", "active", "autostart")


def fail(message):
    sys.stderr.write("collector: " + message + "\n")
    sys.exit(2)


def run(*arguments):
    try:
        result = subprocess.run(
            arguments, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail("could not run %s: %s" % (arguments[0], type(error).__name__))
    if result.returncode != 0:
        fail("%s exited with status %d" % (arguments[0], result.returncode))
    return result.stdout


def api(path, *extra):
    return json.loads(run("pvesh", "get", path, "--output-format", "json", *extra))


def text(value):
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        return value
    fail("unexpected non-scalar value")


def pick(item, keys):
    picked = {key: text(item[key]) for key in keys if key in item and item[key] is not None}
    for key in ("content", "nodes"):
        if key in picked:
            picked[key] = ",".join(sorted(picked[key].split(",")))
    return picked


def zfs(kind, properties):
    output = run("zfs", "get", "-H", "-p", "-r", "-t", kind, "-o", "name,property,value",
                 ",".join(properties), ZFS_ROOT)
    result = {}
    for line in output.splitlines():
        fields = line.split("\t")
        if len(fields) != 3:
            fail("unexpected zfs get output")
        name, prop, value = fields
        result.setdefault(name, {})[prop] = None if value == "-" else value
    for name, props in result.items():
        if set(props) != set(properties):
            fail("incomplete zfs properties for a dataset")
    return result


def guests(node):
    machines = {}
    for item in api("/nodes/%s/qemu" % node):
        vmid = text(item["vmid"])
        config = api("/nodes/%s/qemu/%s/config" % (node, vmid), "--current", "1")
        pending = api("/nodes/%s/qemu/%s/pending" % (node, vmid))
        snapshots = api("/nodes/%s/qemu/%s/snapshot" % (node, vmid))
        machines[vmid] = {
            "status": text(item["status"]),
            "config": {k: text(v) for k, v in config.items() if VM_KEYS.match(k) and v is not None},
            "omitted_keys": sorted(k for k in config if not VM_KEYS.match(k)),
            "pending_keys": sorted(e["key"] for e in pending if "pending" in e or "delete" in e),
            "snapshots": sorted(s["name"] for s in snapshots if s["name"] != "current"),
        }
    containers = {}
    for item in api("/nodes/%s/lxc" % node):
        vmid = text(item["vmid"])
        config = api("/nodes/%s/lxc/%s/config" % (node, vmid), "--current", "1")
        containers[vmid] = {
            "status": text(item["status"]),
            "config": {k: text(v) for k, v in config.items() if CT_KEYS.match(k) and v is not None},
            "omitted_keys": sorted(k for k in config if not CT_KEYS.match(k)),
        }
    return machines, containers


def configuration(node):
    storages = {s["storage"]: pick(s, STORAGE_KEYS) for s in api("/storage")}
    pools = {}
    for pool in api("/pools"):
        detail = api("/pools/%s" % pool["poolid"])
        pools[pool["poolid"]] = {
            "comment": text(detail["comment"]) if detail.get("comment") is not None else None,
            "members": sorted(text(member["id"]) for member in detail.get("members", [])),
        }
    network = {i["iface"]: pick(i, IFACE_KEYS) for i in api("/nodes/%s/network" % node)}
    return storages, pools, network


def structure(datasets, snapshots):
    return (
        {name: {p: props[p] for p in STRUCTURAL} for name, props in datasets.items()},
        {name: props["guid"] for name, props in snapshots.items()},
    )


node = run("hostname", "-s").strip()
with open(MACHINE_ID_PATH) as handle:
    machine_id = handle.read().strip()
if not re.fullmatch(r"[0-9a-f]{32}", machine_id):
    fail("unexpected machine-id format")
fingerprint = run("ssh-keygen", "-l", "-E", "sha256", "-f", HOST_KEY_PATH).split()
if len(fingerprint) < 3 or not fingerprint[1].startswith("SHA256:") or fingerprint[-1] != "(ED25519)":
    fail("unexpected host key fingerprint output")
captured_at = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
pool_name = ZFS_ROOT.split("/")[0]
zpool_fields = run("zpool", "list", "-H", "-p", "-o", "name,guid,health", pool_name).strip().split("\t")
if len(zpool_fields) != 3:
    fail("unexpected zpool output")

datasets = zfs("filesystem,volume", DATASET_PROPS)
zfs_snapshots = zfs("snapshot", SNAPSHOT_PROPS)
machines, containers = guests(node)
storages, pools, network = configuration(node)
cluster_nodes = sorted(text(item["node"]) for item in api("/nodes"))
second_machines, second_containers = guests(node)
second_storages, second_pools, second_network = configuration(node)
second_datasets = zfs("filesystem,volume", DATASET_PROPS)
second_snapshots = zfs("snapshot", SNAPSHOT_PROPS)

report = {
    "schema": "pve-dev-discovery-snapshot/1",
    "collector": COLLECTOR,
    "captured_at": captured_at,
    "host": {
        "node": node,
        "host_key_sha256": fingerprint[1],
        "machine_id_sha256": hashlib.sha256(
            ("pve-dev-discovery-machine-id/1\n" + machine_id).encode()
        ).hexdigest(),
        "pve_version": run("pveversion").strip(),
        "cluster_nodes": cluster_nodes,
    },
    "zfs_root": ZFS_ROOT,
    "zpool": {"name": zpool_fields[0], "guid": zpool_fields[1], "health": zpool_fields[2]},
    "datasets": datasets,
    "zfs_snapshots": zfs_snapshots,
    "storages": storages,
    "pools": pools,
    "network": network,
    "vms": machines,
    "containers": containers,
    "consistency": {
        "zfs_stable": structure(datasets, zfs_snapshots) == structure(second_datasets, second_snapshots),
        "guests_stable": (machines, containers) == (second_machines, second_containers),
        "config_stable": (storages, pools, network) == (second_storages, second_pools, second_network),
    },
}
print(json.dumps(report, indent=1, sort_keys=True))
"#;

/// Serializes a JSON value with recursively sorted object keys and no whitespace.
pub fn canonical_json(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                out.push('{');
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort();
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key).expect("string key"));
                    out.push(':');
                    write(&map[key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Domain-separated SHA-256 over the canonical JSON form of `value`.
pub fn canonical_digest(domain: &str, value: &impl Serialize) -> Result<String> {
    let value = serde_json::to_value(value)?;
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\n");
    hasher.update(canonical_json(&value).as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

fn valid_dataset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
}

/// Returns the read-only collector Python source for the given ZFS root.
pub fn collector_source(zfs_root: &str) -> Result<String> {
    ensure!(
        valid_dataset_name(zfs_root) && !zfs_root.contains('@'),
        "invalid ZFS root dataset name"
    );
    Ok(COLLECTOR_TEMPLATE.replace("__ZFS_ROOT__", &serde_json::to_string(zfs_root)?))
}

pub fn collector_sha256(zfs_root: &str) -> Result<String> {
    Ok(sha256_hex(collector_source(zfs_root)?.as_bytes()))
}

fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./=@:+,".contains(&byte))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The exact SSH invocation (argument vector) an operator should use to run the
/// collector. It is never executed by this crate.
pub fn ssh_arguments(
    destination: &str,
    known_hosts: &Path,
    identity: Option<&Path>,
    port: u16,
) -> Result<Vec<String>> {
    ensure!(
        !destination.is_empty()
            && !destination.starts_with('-')
            && destination
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.@".contains(&byte)),
        "invalid SSH destination"
    );
    ensure!(
        known_hosts.is_absolute(),
        "known_hosts path must be absolute"
    );
    let mut arguments = vec![
        "ssh".to_owned(),
        "-F".into(),
        "none".into(),
        "-T".into(),
        "-p".into(),
        port.to_string(),
    ];
    for option in [
        "BatchMode=yes".to_owned(),
        "StrictHostKeyChecking=yes".into(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "GlobalKnownHostsFile=/dev/null".into(),
        "HostKeyAlgorithms=ssh-ed25519".into(),
        "UpdateHostKeys=no".into(),
        "ForwardAgent=no".into(),
        "ForwardX11=no".into(),
        "ClearAllForwardings=yes".into(),
        "PermitLocalCommand=no".into(),
        "ProxyCommand=none".into(),
        "RequestTTY=no".into(),
        "ConnectTimeout=15".into(),
        "ConnectionAttempts=1".into(),
        "ServerAliveInterval=15".into(),
        "ServerAliveCountMax=3".into(),
    ] {
        arguments.push("-o".into());
        arguments.push(option);
    }
    if let Some(identity) = identity {
        ensure!(identity.is_absolute(), "identity path must be absolute");
        arguments.extend([
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-i".into(),
            identity.display().to_string(),
        ]);
    }
    arguments.extend([destination.to_owned(), "python3 -I -".into()]);
    Ok(arguments)
}

pub fn ssh_command_line(arguments: &[String], collector: &Path, output: &Path) -> String {
    let mut line: Vec<String> = arguments.iter().map(|word| shell_quote(word)).collect();
    line.push(format!(
        "< {}",
        shell_quote(&collector.display().to_string())
    ));
    line.push(format!("> {}", shell_quote(&output.display().to_string())));
    line.join(" ")
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema: String,
    pub collector: String,
    pub captured_at: String,
    pub host: HostIdentity,
    pub zfs_root: String,
    pub zpool: Zpool,
    pub datasets: BTreeMap<String, DatasetProps>,
    pub zfs_snapshots: BTreeMap<String, SnapshotProps>,
    pub storages: BTreeMap<String, StorageProps>,
    pub pools: BTreeMap<String, PoolProps>,
    pub network: BTreeMap<String, InterfaceProps>,
    pub vms: BTreeMap<String, VmProps>,
    pub containers: BTreeMap<String, ContainerProps>,
    pub consistency: Consistency,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub node: String,
    pub host_key_sha256: String,
    pub machine_id_sha256: String,
    pub pve_version: String,
    pub cluster_nodes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Zpool {
    pub name: String,
    pub guid: String,
    pub health: String,
}

/// Deserializes an `Option` that must be present (possibly `null`). Without
/// this, serde would silently treat a missing key as `None`.
fn required<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

/// Raw `zfs get -p` values; `None` is ZFS's "-" (not applicable), never a default.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatasetProps {
    #[serde(deserialize_with = "required")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(deserialize_with = "required")]
    pub guid: Option<String>,
    #[serde(deserialize_with = "required")]
    pub quota: Option<String>,
    #[serde(deserialize_with = "required")]
    pub reservation: Option<String>,
    #[serde(deserialize_with = "required")]
    pub refreservation: Option<String>,
    #[serde(deserialize_with = "required")]
    pub volsize: Option<String>,
    #[serde(deserialize_with = "required")]
    pub origin: Option<String>,
    #[serde(deserialize_with = "required")]
    pub used: Option<String>,
    #[serde(deserialize_with = "required")]
    pub available: Option<String>,
    #[serde(deserialize_with = "required")]
    pub mountpoint: Option<String>,
    #[serde(deserialize_with = "required")]
    pub canmount: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotProps {
    #[serde(deserialize_with = "required")]
    pub guid: Option<String>,
    #[serde(deserialize_with = "required")]
    pub used: Option<String>,
    #[serde(deserialize_with = "required")]
    pub createtxg: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StorageProps {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocksize: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolProps {
    #[serde(deserialize_with = "required")]
    pub comment: Option<String>,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InterfaceProps {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netmask: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_ports: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VmProps {
    pub status: String,
    pub config: BTreeMap<String, String>,
    pub omitted_keys: Vec<String>,
    pub pending_keys: Vec<String>,
    pub snapshots: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerProps {
    pub status: String,
    pub config: BTreeMap<String, String>,
    pub omitted_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Consistency {
    pub zfs_stable: bool,
    pub guests_stable: bool,
    pub config_stable: bool,
}

/// Explicit host pin supplied by the operator, never derived from the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPin {
    pub node: String,
    pub host_key_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskAttachment {
    pub slot: String,
    pub volume: String,
    /// "zfs" (resolved to a dataset with a GUID), "media" (ISO/empty CD-ROM on
    /// non-ZFS storage), "passthrough" (host path), or "external" (non-ZFS disk).
    pub kind: String,
    pub storage: Option<String>,
    pub dataset: Option<String>,
    pub guid: Option<String>,
    pub cdrom: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NicObservation {
    pub slot: String,
    pub model: Option<String>,
    pub mac: Option<String>,
    pub bridge: Option<String>,
    pub firewall: bool,
    pub tag: Option<String>,
    pub ipconfig: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationReport {
    pub snapshot_digest: String,
    pub raw_sha256: String,
    pub node: String,
    pub host_key_sha256: String,
    pub captured_at: String,
    pub zfs_root: String,
    pub datasets: usize,
    pub volumes: usize,
    pub zfs_snapshots: usize,
    pub vms: usize,
    pub containers: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ValidatedSnapshot {
    pub snapshot: Snapshot,
    pub digest: String,
    pub raw_sha256: String,
    pub attachments: BTreeMap<u32, Vec<DiskAttachment>>,
    pub container_attachments: BTreeMap<u32, Vec<DiskAttachment>>,
    pub warnings: Vec<String>,
}

impl ValidatedSnapshot {
    pub fn report(&self) -> ValidationReport {
        let s = &self.snapshot;
        ValidationReport {
            snapshot_digest: self.digest.clone(),
            raw_sha256: self.raw_sha256.clone(),
            node: s.host.node.clone(),
            host_key_sha256: s.host.host_key_sha256.clone(),
            captured_at: s.captured_at.clone(),
            zfs_root: s.zfs_root.clone(),
            datasets: s.datasets.len(),
            volumes: s
                .datasets
                .values()
                .filter(|d| d.kind.as_deref() == Some("volume"))
                .count(),
            zfs_snapshots: s.zfs_snapshots.len(),
            vms: s.vms.len(),
            containers: s.containers.len(),
            warnings: self.warnings.clone(),
        }
    }

    pub fn host_id(&self) -> String {
        host_id(&self.snapshot.host.host_key_sha256)
    }
}

pub fn host_id(host_key_sha256: &str) -> String {
    sha256_hex(format!("pve-dev-discovery-host/1\n{host_key_sha256}").as_bytes())
}

const SECRET_KEYS: &[&str] = &[
    "sshkeys",
    "cipassword",
    "cicustom",
    "password",
    "token",
    "secret",
    "private_key",
    "description",
    "args",
    "hookscript",
    "keys",
    "authorized_keys",
];

fn scan_secrets(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                ensure!(
                    !SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()),
                    "snapshot contains secret-bearing field {path}/{key}; refusing"
                );
                scan_secrets(child, &format!("{path}/{key}"))?;
            }
        }
        Value::Array(items) => {
            for item in items {
                scan_secrets(item, path)?;
            }
        }
        Value::String(text) => {
            let upper = text.to_ascii_uppercase();
            ensure!(
                !upper.contains("PRIVATE KEY")
                    && !text.contains("ssh-ed25519 AAAA")
                    && !text.contains("ssh-rsa AAAA")
                    && !text.contains("#cloud-config"),
                "snapshot contains key or cloud-init material at {path}; refusing"
            );
        }
        _ => {}
    }
    Ok(())
}

fn parse_u64(value: &Option<String>, what: &str) -> Result<Option<u64>> {
    value
        .as_deref()
        .map(|text| {
            text.parse::<u64>()
                .with_context(|| format!("{what}: invalid integer"))
        })
        .transpose()
}

fn valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
}

fn valid_host_key(value: &str) -> bool {
    value.strip_prefix("SHA256:").is_some_and(|rest| {
        rest.len() == 43
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    })
}

fn valid_node(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_guid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_owner(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_vm_disk_key(key: &str) -> bool {
    if key == "efidisk0" || key == "tpmstate0" {
        return true;
    }
    ["ide", "sata", "scsi", "virtio", "unused"]
        .iter()
        .any(|prefix| {
            key.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
}

fn is_ct_disk_key(key: &str) -> bool {
    key == "rootfs"
        || ["mp", "unused"].iter().any(|prefix| {
            key.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
}

fn numbered(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

const VM_SCALAR_KEYS: &[&str] = &[
    "name",
    "memory",
    "balloon",
    "cores",
    "sockets",
    "vcpus",
    "cpu",
    "cpuunits",
    "cpulimit",
    "machine",
    "ostype",
    "bios",
    "boot",
    "scsihw",
    "onboot",
    "protection",
    "tags",
    "template",
    "lock",
    "digest",
    "agent",
    "numa",
    "hotplug",
    "kvm",
    "startup",
    "vga",
    "nameserver",
    "searchdomain",
    "ciuser",
    "ciupgrade",
    "citype",
];

fn allowed_vm_key(key: &str) -> bool {
    VM_SCALAR_KEYS.contains(&key)
        || is_vm_disk_key(key)
        || ["net", "ipconfig", "serial"]
            .iter()
            .any(|prefix| numbered(key, prefix))
}

fn allowed_ct_key(key: &str) -> bool {
    ["hostname", "digest", "lock", "template"].contains(&key) || is_ct_disk_key(key)
}

fn option_map(value: &str) -> (String, BTreeMap<String, String>) {
    let mut fields = value.split(',');
    let first = fields.next().unwrap_or_default();
    let volume = first
        .strip_prefix("file=")
        .or_else(|| first.strip_prefix("volume="))
        .unwrap_or(first)
        .to_owned();
    let mut options = BTreeMap::new();
    for field in fields {
        match field.split_once('=') {
            Some((key, value)) => options.insert(key.to_owned(), value.to_owned()),
            None => options.insert(field.to_owned(), "1".to_owned()),
        };
    }
    (volume, options)
}

fn resolve_disk(
    snapshot: &Snapshot,
    slot: &str,
    value: &str,
    want_kind: &str,
) -> Result<DiskAttachment> {
    let (volume, options) = option_map(value);
    let cdrom = options.get("media").is_some_and(|media| media == "cdrom");
    let mut attachment = DiskAttachment {
        slot: slot.to_owned(),
        volume: volume.clone(),
        kind: String::new(),
        storage: None,
        dataset: None,
        guid: None,
        cdrom,
    };
    if volume == "none" || volume == "cdrom" {
        ensure!(
            cdrom || volume == "cdrom",
            "{slot}: empty non-CD-ROM device"
        );
        attachment.kind = "media".into();
        return Ok(attachment);
    }
    if volume.starts_with('/') {
        attachment.kind = "passthrough".into();
        return Ok(attachment);
    }
    let (storage_id, volname) = volume
        .split_once(':')
        .with_context(|| format!("{slot}: unparseable volume reference"))?;
    let storage = snapshot.storages.get(storage_id).with_context(|| {
        format!("{slot}: storage {storage_id:?} is not in the snapshot (incomplete inventory)")
    })?;
    attachment.storage = Some(storage_id.to_owned());
    if storage.kind != "zfspool" {
        attachment.kind = if cdrom { "media" } else { "external" }.into();
        return Ok(attachment);
    }
    let pool = storage
        .pool
        .as_deref()
        .with_context(|| format!("storage {storage_id}: zfspool without pool"))?;
    let leaf = volname.rsplit('/').next().unwrap_or(volname);
    ensure!(
        !leaf.is_empty() && !leaf.contains('@'),
        "{slot}: invalid volume name"
    );
    let dataset_name = format!("{pool}/{leaf}");
    let dataset = snapshot.datasets.get(&dataset_name).with_context(|| {
        format!("{slot}: referenced dataset {dataset_name} is missing from the snapshot (incomplete inventory)")
    })?;
    ensure!(
        dataset.kind.as_deref() == Some(want_kind),
        "{slot}: {dataset_name} is not a ZFS {want_kind}"
    );
    attachment.kind = "zfs".into();
    attachment.dataset = Some(dataset_name);
    attachment.guid = dataset.guid.clone();
    Ok(attachment)
}

fn parent(name: &str) -> Option<&str> {
    name.rsplit_once('/').map(|(parent, _)| parent)
}

fn contained(child: &str, ancestor: &str) -> bool {
    child
        .strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Validates collector output against an explicit host pin. Fails closed on any
/// missing, unknown, inconsistent, unresolvable or secret-bearing content.
pub fn validate_snapshot(bytes: &[u8], pin: &HostPin) -> Result<ValidatedSnapshot> {
    let raw: Value = serde_json::from_slice(bytes).context("snapshot is not valid JSON")?;
    scan_secrets(&raw, "")?;
    let snapshot: Snapshot = serde_json::from_value(raw)
        .context("snapshot does not match the discovery schema (missing or unknown fields)")?;
    let mut warnings = Vec::new();
    let s = &snapshot;

    ensure!(s.schema == SNAPSHOT_SCHEMA, "unsupported snapshot schema");
    ensure!(
        s.collector == COLLECTOR_VERSION,
        "unsupported collector version"
    );
    ensure!(valid_timestamp(&s.captured_at), "invalid captured_at");
    ensure!(valid_node(&s.host.node), "invalid node name");
    ensure!(
        valid_host_key(&s.host.host_key_sha256),
        "invalid host key fingerprint"
    );
    ensure!(
        s.host.machine_id_sha256.len() == 64
            && s.host
                .machine_id_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid machine id digest"
    );
    ensure!(
        !s.host.pve_version.trim().is_empty(),
        "missing Proxmox version"
    );
    ensure!(
        valid_host_key(&pin.host_key_sha256) && valid_node(&pin.node),
        "invalid host pin"
    );
    ensure!(
        s.host.node == pin.node,
        "snapshot node {:?} does not match pinned node {:?}",
        s.host.node,
        pin.node
    );
    ensure!(
        s.host.host_key_sha256 == pin.host_key_sha256,
        "snapshot host key does not match the pinned host key"
    );
    ensure!(
        s.host.cluster_nodes == vec![s.host.node.clone()],
        "multi-node or inconsistent cluster membership is not supported (only the local node is collected)"
    );
    ensure!(
        s.consistency.zfs_stable && s.consistency.guests_stable && s.consistency.config_stable,
        "state changed during collection (inconsistent snapshot); collect again"
    );

    ensure!(valid_dataset_name(&s.zfs_root), "invalid ZFS root");
    ensure!(
        s.zpool.name == s.zfs_root.split('/').next().unwrap_or_default(),
        "zpool does not contain the ZFS root"
    );
    ensure!(valid_guid(&s.zpool.guid), "invalid zpool GUID");
    if s.zpool.health != "ONLINE" {
        warnings.push(format!(
            "zpool {} health is {}",
            s.zpool.name, s.zpool.health
        ));
    }
    let root = s
        .datasets
        .get(&s.zfs_root)
        .context("ZFS root dataset missing from snapshot")?;
    ensure!(
        root.kind.as_deref() == Some("filesystem"),
        "ZFS root is not a filesystem"
    );
    let mut guids = BTreeSet::new();
    for (name, props) in &s.datasets {
        ensure!(
            valid_dataset_name(name) && !name.contains('@'),
            "invalid dataset name {name:?}"
        );
        ensure!(
            name == &s.zfs_root || contained(name, &s.zfs_root),
            "dataset {name} is outside the collected root"
        );
        if name != &s.zfs_root {
            let parent = parent(name).context("dataset without parent")?;
            let parent_props = s
                .datasets
                .get(parent)
                .with_context(|| format!("parent of {name} missing (incomplete inventory)"))?;
            ensure!(
                parent_props.kind.as_deref() == Some("filesystem"),
                "parent of {name} is not a filesystem"
            );
        }
        let guid = props
            .guid
            .as_deref()
            .with_context(|| format!("{name}: missing GUID"))?;
        ensure!(valid_guid(guid), "{name}: invalid GUID");
        ensure!(guids.insert(guid.to_owned()), "duplicate ZFS GUID {guid}");
        match props.kind.as_deref() {
            Some("filesystem") => {
                ensure!(props.volsize.is_none(), "{name}: filesystem with volsize");
                parse_u64(&props.quota, name)?.with_context(|| format!("{name}: missing quota"))?;
                parse_u64(&props.reservation, name)?
                    .with_context(|| format!("{name}: missing reservation"))?;
            }
            Some("volume") => {
                ensure!(
                    parse_u64(&props.volsize, name)?.is_some_and(|size| size > 0),
                    "{name}: volume without volsize"
                );
                parse_u64(&props.refreservation, name)?
                    .with_context(|| format!("{name}: missing refreservation"))?;
            }
            _ => bail!("{name}: unsupported dataset type"),
        }
        parse_u64(&props.used, name)?.with_context(|| format!("{name}: missing used"))?;
    }
    for (name, props) in &s.zfs_snapshots {
        let (dataset, snap) = name
            .split_once('@')
            .with_context(|| format!("invalid snapshot name {name:?}"))?;
        ensure!(!snap.is_empty(), "invalid snapshot name {name:?}");
        ensure!(
            s.datasets.contains_key(dataset),
            "snapshot {name} references a dataset missing from the snapshot"
        );
        let guid = props
            .guid
            .as_deref()
            .with_context(|| format!("{name}: missing GUID"))?;
        ensure!(valid_guid(guid), "{name}: invalid GUID");
        ensure!(guids.insert(guid.to_owned()), "duplicate ZFS GUID {guid}");
        parse_u64(&props.used, name)?;
        parse_u64(&props.createtxg, name)?;
    }

    for (id, storage) in &s.storages {
        ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "invalid storage id {id:?}"
        );
        if storage.kind == "zfspool" {
            let pool = storage
                .pool
                .as_deref()
                .with_context(|| format!("storage {id}: zfspool without pool"))?;
            let disabled = storage.disable.as_deref().is_some_and(|v| v != "0");
            match s.datasets.get(pool) {
                Some(props) => ensure!(
                    props.kind.as_deref() == Some("filesystem"),
                    "storage {id}: pool {pool} is not a filesystem"
                ),
                None if disabled => warnings.push(format!(
                    "disabled storage {id} points outside the collected root"
                )),
                None => {
                    bail!("storage {id}: pool {pool} is not in the snapshot (incomplete inventory)")
                }
            }
        }
    }

    let mut attachments = BTreeMap::new();
    let mut attached_by: BTreeMap<String, String> = BTreeMap::new();
    for (key, vm) in &s.vms {
        let vmid: u32 = key.parse().context("invalid VM ID")?;
        ensure!(
            vmid >= 100 && key == &vmid.to_string(),
            "invalid VM ID {key}"
        );
        ensure!(!vm.status.is_empty(), "VM {vmid}: missing status");
        ensure!(
            vm.config.contains_key("digest"),
            "VM {vmid}: missing config digest (incomplete inventory)"
        );
        for option in vm.config.keys() {
            ensure!(
                allowed_vm_key(option),
                "VM {vmid}: unexpected config option {option:?} (not allow-listed)"
            );
        }
        for option in vm.omitted_keys.iter().chain(&vm.pending_keys) {
            ensure!(
                !option.is_empty()
                    && option
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                "VM {vmid}: invalid option name"
            );
        }
        for option in &vm.omitted_keys {
            ensure!(
                !allowed_vm_key(option),
                "VM {vmid}: allow-listed option {option} reported as omitted"
            );
        }
        let mut disks = Vec::new();
        for (slot, value) in &vm.config {
            if !is_vm_disk_key(slot) {
                continue;
            }
            let disk =
                resolve_disk(s, slot, value, "volume").with_context(|| format!("VM {vmid}"))?;
            if let Some(dataset) = &disk.dataset {
                if let Some(other) = attached_by.insert(dataset.clone(), format!("VM {vmid}"))
                    && other != format!("VM {vmid}")
                {
                    bail!("{dataset} is attached to both {other} and VM {vmid}");
                }
                let leaf = dataset.rsplit('/').next().unwrap_or_default();
                if !leaf.contains(&format!("-{vmid}-")) {
                    warnings.push(format!(
                        "VM {vmid} attaches {dataset}, named for another guest"
                    ));
                }
                for snapshot in &vm.snapshots {
                    if !s
                        .zfs_snapshots
                        .contains_key(&format!("{dataset}@{snapshot}"))
                    {
                        warnings.push(format!(
                            "VM {vmid} snapshot {snapshot} has no ZFS snapshot on {dataset}"
                        ));
                    }
                }
            }
            disks.push(disk);
        }
        for (slot, value) in &vm.config {
            if numbered(slot, "net") {
                let bridge = option_map(value).1.get("bridge").cloned();
                if let Some(bridge) = bridge
                    && !s.network.contains_key(&bridge)
                {
                    warnings.push(format!("VM {vmid} {slot} uses unknown bridge {bridge}"));
                }
            }
        }
        attachments.insert(vmid, disks);
    }
    let mut container_attachments = BTreeMap::new();
    for (key, ct) in &s.containers {
        let vmid: u32 = key.parse().context("invalid container ID")?;
        ensure!(
            vmid >= 100 && key == &vmid.to_string(),
            "invalid container ID {key}"
        );
        ensure!(
            !s.vms.contains_key(key),
            "ID {vmid} is both a VM and a container"
        );
        for option in ct.config.keys() {
            ensure!(
                allowed_ct_key(option),
                "container {vmid}: unexpected config option {option:?}"
            );
        }
        let mut disks = Vec::new();
        for (slot, value) in &ct.config {
            if !is_ct_disk_key(slot) {
                continue;
            }
            let disk = resolve_disk(s, slot, value, "filesystem")
                .with_context(|| format!("container {vmid}"))?;
            if let Some(dataset) = &disk.dataset
                && let Some(other) = attached_by.insert(dataset.clone(), format!("CT {vmid}"))
                && other != format!("CT {vmid}")
            {
                bail!("{dataset} is attached to both {other} and container {vmid}");
            }
            disks.push(disk);
        }
        container_attachments.insert(vmid, disks);
    }
    for (pool, props) in &s.pools {
        for member in &props.members {
            if let Some(id) = member.strip_prefix("qemu/") {
                ensure!(s.vms.contains_key(id), "pool {pool} lists unknown VM {id}");
            } else if let Some(id) = member.strip_prefix("lxc/") {
                ensure!(
                    s.containers.contains_key(id),
                    "pool {pool} lists unknown container {id}"
                );
            }
        }
    }
    for (name, props) in &s.datasets {
        if props.kind.as_deref() == Some("volume") && !attached_by.contains_key(name) {
            warnings.push(format!("volume {name} is not attached to any guest"));
        }
    }

    let digest = canonical_digest(SNAPSHOT_SCHEMA, &snapshot)?;
    Ok(ValidatedSnapshot {
        digest,
        raw_sha256: sha256_hex(bytes),
        attachments,
        container_attachments,
        warnings,
        snapshot,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub schema: String,
    pub snapshot_digest: String,
    pub node: String,
    pub host_key_sha256: String,
    pub allocations: Vec<AllocationMapping>,
    pub vms: Vec<VmMapping>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AllocationMapping {
    pub dataset: String,
    pub guid: String,
    /// Must be set explicitly; a proposal always leaves it null.
    pub owner: Option<String>,
    /// Informational only; never used for authorization and excluded from the mapping digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VmMapping {
    pub vmid: u32,
    pub config_digest: String,
    /// Exact ZFS attachments: slot -> dataset GUID.
    pub disks: BTreeMap<String, String>,
    /// "owned" or "unmanaged"; a proposal leaves it null.
    pub disposition: Option<String>,
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hint: Option<String>,
}

impl Mapping {
    pub fn digest(&self) -> Result<String> {
        let mut normalized = self.clone();
        for allocation in &mut normalized.allocations {
            allocation.name_hint = None;
        }
        for vm in &mut normalized.vms {
            vm.name_hint = None;
        }
        normalized
            .allocations
            .sort_by(|a, b| a.dataset.cmp(&b.dataset));
        normalized.vms.sort_by_key(|vm| vm.vmid);
        canonical_digest(MAPPING_SCHEMA, &normalized)
    }
}

/// Produces an unconfirmed mapping. Owners and dispositions are always null:
/// observation never implies ownership, and names are only hints.
pub fn propose_mapping(validated: &ValidatedSnapshot) -> Mapping {
    let s = &validated.snapshot;
    let mut storage_ids: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, storage) in &s.storages {
        if storage.kind == "zfspool"
            && let Some(pool) = storage.pool.as_deref()
            && pool != s.zfs_root
            && s.datasets.contains_key(pool)
        {
            storage_ids.entry(pool).or_default().push(id);
        }
    }
    let allocations = storage_ids
        .into_iter()
        .map(|(pool, ids)| AllocationMapping {
            dataset: pool.to_owned(),
            guid: s.datasets[pool].guid.clone().unwrap_or_default(),
            owner: None,
            name_hint: Some(format!("storage {}", ids.join(", "))),
        })
        .collect();
    let vms = validated
        .attachments
        .iter()
        .map(|(vmid, disks)| {
            let props = &s.vms[&vmid.to_string()];
            VmMapping {
                vmid: *vmid,
                config_digest: props.config.get("digest").cloned().unwrap_or_default(),
                disks: disks
                    .iter()
                    .filter_map(|disk| Some((disk.slot.clone(), disk.guid.clone()?)))
                    .collect(),
                disposition: None,
                owner: None,
                name_hint: props.config.get("name").cloned(),
            }
        })
        .collect();
    Mapping {
        schema: MAPPING_SCHEMA.into(),
        snapshot_digest: validated.digest.clone(),
        node: s.host.node.clone(),
        host_key_sha256: s.host.host_key_sha256.clone(),
        allocations,
        vms,
    }
}

/// A boolean that is always `false`: serializes as `false` and refuses to
/// deserialize `true`. Observation records can never claim management.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlwaysFalse;

impl AlwaysFalse {
    pub fn get(self) -> bool {
        false
    }
}

impl Serialize for AlwaysFalse {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_bool(false)
    }
}

impl<'de> Deserialize<'de> for AlwaysFalse {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Err(serde::de::Error::custom(
                "observation records can never be managed or authorize management",
            ))
        } else {
            Ok(AlwaysFalse)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedVolume {
    pub dataset: String,
    pub guid: String,
    pub volsize_bytes: u64,
    pub refreservation_bytes: u64,
    pub fully_reserved: bool,
    pub attached_to: Option<u32>,
    pub snapshots: Vec<ObservedZfsSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedZfsSnapshot {
    pub name: String,
    pub guid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedAllocation {
    pub dataset: String,
    pub guid: String,
    pub owner: String,
    pub quota_bytes: u64,
    pub reservation_bytes: u64,
    pub storage_ids: Vec<String>,
    pub volumes: Vec<ObservedVolume>,
    pub managed: AlwaysFalse,
    pub authorizes_management: AlwaysFalse,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedVm {
    pub vmid: u32,
    pub name: Option<String>,
    pub status: String,
    pub config_digest: String,
    pub disposition: String,
    pub owner: Option<String>,
    pub allocation: Option<String>,
    pub cores: Option<String>,
    pub sockets: Option<String>,
    pub memory: Option<String>,
    pub balloon: Option<String>,
    pub disks: Vec<DiskAttachment>,
    pub network: Vec<NicObservation>,
    pub proxmox_snapshots: Vec<String>,
    pub omitted_keys: Vec<String>,
    pub managed: AlwaysFalse,
    pub authorizes_management: AlwaysFalse,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub snapshot_schema: String,
    pub collector: String,
    /// SHA-256 of the collector source this importer embeds for `zfs_root`.
    /// The snapshot itself carries no cryptographic attestation of origin.
    pub expected_collector_sha256: String,
    pub raw_sha256: String,
    pub captured_at: String,
    pub node: String,
    pub host_key_sha256: String,
    pub machine_id_sha256: String,
    pub zpool_guid: String,
    pub pve_version: String,
    pub importer: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedState {
    pub allocations: Vec<ObservedAllocation>,
    pub vms: Vec<ObservedVm>,
    pub unowned_volumes: Vec<ObservedVolume>,
    pub containers: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportRecord {
    pub schema: String,
    pub host_id: String,
    pub snapshot_digest: String,
    pub mapping_digest: String,
    pub import_digest: String,
    pub provenance: Provenance,
    pub observed: ObservedState,
    pub warnings: Vec<String>,
    pub imported_at: u64,
    pub managed: AlwaysFalse,
    pub authorizes_management: AlwaysFalse,
    pub creates_controller_state: AlwaysFalse,
}

impl ImportRecord {
    fn content_digest(&self) -> Result<String> {
        canonical_digest(
            IMPORT_SCHEMA,
            &(
                &self.schema,
                &self.host_id,
                &self.snapshot_digest,
                &self.mapping_digest,
                &self.provenance,
                &self.observed,
                &self.warnings,
            ),
        )
    }
}

fn props_volume(
    s: &Snapshot,
    name: &str,
    props: &DatasetProps,
    attached_to: Option<u32>,
) -> Result<ObservedVolume> {
    let volsize = parse_u64(&props.volsize, name)?.context("volume without volsize")?;
    let refreservation = parse_u64(&props.refreservation, name)?.unwrap_or(0);
    Ok(ObservedVolume {
        dataset: name.to_owned(),
        guid: props.guid.clone().context("missing GUID")?,
        volsize_bytes: volsize,
        refreservation_bytes: refreservation,
        fully_reserved: refreservation >= volsize,
        attached_to,
        snapshots: s
            .zfs_snapshots
            .iter()
            .filter(|(snap, _)| snap.split_once('@').is_some_and(|(d, _)| d == name))
            .map(|(snap, p)| ObservedZfsSnapshot {
                name: snap.clone(),
                guid: p.guid.clone().unwrap_or_default(),
            })
            .collect(),
    })
}

/// Validates a mapping against the snapshot and builds the observed state.
/// Pure function; nothing is persisted.
pub fn check_mapping(
    validated: &ValidatedSnapshot,
    mapping: &Mapping,
    confirm_digest: &str,
) -> Result<ObservedState> {
    let s = &validated.snapshot;
    ensure!(
        mapping.schema == MAPPING_SCHEMA,
        "unsupported mapping schema"
    );
    ensure!(
        confirm_digest == validated.digest,
        "confirmation digest does not match the snapshot digest {}",
        validated.digest
    );
    ensure!(
        mapping.snapshot_digest == validated.digest,
        "mapping was prepared for a different snapshot"
    );
    ensure!(
        mapping.node == s.host.node && mapping.host_key_sha256 == s.host.host_key_sha256,
        "mapping host does not match the snapshot host"
    );

    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    for allocation in &mapping.allocations {
        let owner = allocation.owner.as_deref().with_context(|| {
            format!(
                "allocation {} has no explicit owner (unconfirmed proposal)",
                allocation.dataset
            )
        })?;
        ensure!(valid_owner(owner), "invalid owner {owner:?}");
        let props = s
            .datasets
            .get(&allocation.dataset)
            .with_context(|| format!("allocation dataset {} not observed", allocation.dataset))?;
        ensure!(
            props.guid.as_deref() == Some(allocation.guid.as_str()),
            "allocation {} GUID mismatch (dataset replaced or mapping stale)",
            allocation.dataset
        );
        ensure!(
            props.kind.as_deref() == Some("filesystem") && allocation.dataset != s.zfs_root,
            "allocation {} must be a filesystem below the ZFS root",
            allocation.dataset
        );
        ensure!(
            owners
                .insert(allocation.dataset.clone(), owner.to_owned())
                .is_none(),
            "allocation {} mapped twice",
            allocation.dataset
        );
    }
    for a in owners.keys() {
        for b in owners.keys() {
            ensure!(
                !contained(a, b),
                "nested allocations {a} and {b} are not allowed"
            );
        }
    }
    let allocation_of = |dataset: &str| -> Option<&String> {
        owners
            .keys()
            .find(|allocation| contained(dataset, allocation))
    };

    let mut seen = BTreeSet::new();
    let mut observed_vms = Vec::new();
    let mut attached: BTreeMap<String, u32> = BTreeMap::new();
    for entry in &mapping.vms {
        ensure!(seen.insert(entry.vmid), "VM {} mapped twice", entry.vmid);
        let props = s
            .vms
            .get(&entry.vmid.to_string())
            .with_context(|| format!("mapped VM {} was not observed", entry.vmid))?;
        let disks = &validated.attachments[&entry.vmid];
        ensure!(
            props.config.get("digest") == Some(&entry.config_digest),
            "VM {} config digest mismatch",
            entry.vmid
        );
        let actual: BTreeMap<String, String> = disks
            .iter()
            .filter_map(|disk| Some((disk.slot.clone(), disk.guid.clone()?)))
            .collect();
        ensure!(
            actual == entry.disks,
            "VM {} mapped disks do not match its attached disks",
            entry.vmid
        );
        let disposition = entry.disposition.as_deref().with_context(|| {
            format!(
                "VM {} has no explicit disposition (unconfirmed proposal)",
                entry.vmid
            )
        })?;
        let allocation = match disposition {
            "owned" => {
                let owner = entry
                    .owner
                    .as_deref()
                    .with_context(|| format!("owned VM {} has no explicit owner", entry.vmid))?;
                ensure!(valid_owner(owner), "invalid owner {owner:?}");
                ensure!(
                    !props.config.contains_key("lock"),
                    "VM {} is locked; refusing to map a VM with an operation in progress",
                    entry.vmid
                );
                ensure!(
                    props.pending_keys.is_empty(),
                    "VM {} has pending configuration changes",
                    entry.vmid
                );
                let mut allocations = BTreeSet::new();
                for disk in disks {
                    match disk.kind.as_str() {
                        "zfs" => {
                            let dataset = disk.dataset.as_deref().unwrap_or_default();
                            let allocation = allocation_of(dataset).with_context(|| {
                                format!(
                                    "VM {} disk {} ({dataset}) is outside every mapped allocation",
                                    entry.vmid, disk.slot
                                )
                            })?;
                            ensure!(
                                owners[allocation] == owner,
                                "VM {} disk {} lives in {allocation} owned by {}, not {owner}",
                                entry.vmid,
                                disk.slot,
                                owners[allocation]
                            );
                            allocations.insert(allocation.clone());
                        }
                        "media" if disk.cdrom => {}
                        other => bail!(
                            "VM {} disk {} is a {other} reference outside any allocation",
                            entry.vmid,
                            disk.slot
                        ),
                    }
                }
                ensure!(
                    allocations.len() <= 1,
                    "VM {} spans multiple allocations",
                    entry.vmid
                );
                ensure!(
                    !allocations.is_empty(),
                    "owned VM {} has no disk inside an allocation",
                    entry.vmid
                );
                allocations.into_iter().next()
            }
            "unmanaged" => {
                ensure!(
                    entry.owner.is_none(),
                    "unmanaged VM {} must not have an owner",
                    entry.vmid
                );
                for disk in disks {
                    if let Some(dataset) = &disk.dataset
                        && let Some(allocation) = allocation_of(dataset)
                    {
                        bail!(
                            "unmanaged VM {} has disk {} inside allocation {allocation} owned by {}",
                            entry.vmid,
                            disk.slot,
                            owners[allocation]
                        );
                    }
                }
                None
            }
            other => bail!("VM {}: unknown disposition {other:?}", entry.vmid),
        };
        for disk in disks {
            if let Some(dataset) = &disk.dataset {
                attached.insert(dataset.clone(), entry.vmid);
            }
        }
        let network = props
            .config
            .iter()
            .filter(|(key, _)| numbered(key, "net"))
            .map(|(slot, value)| {
                let options: BTreeMap<_, _> = value
                    .split(',')
                    .filter_map(|field| field.split_once('='))
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect();
                let model = ["virtio", "e1000", "e1000e", "rtl8139", "vmxnet3"]
                    .into_iter()
                    .find(|model| options.contains_key(*model));
                let index = &slot[3..];
                NicObservation {
                    slot: slot.clone(),
                    model: model.map(str::to_owned),
                    mac: model.and_then(|model| options.get(model).cloned()),
                    bridge: options.get("bridge").cloned(),
                    firewall: options.get("firewall").is_some_and(|v| v == "1"),
                    tag: options.get("tag").cloned(),
                    ipconfig: props.config.get(&format!("ipconfig{index}")).cloned(),
                }
            })
            .collect();
        observed_vms.push(ObservedVm {
            vmid: entry.vmid,
            name: props.config.get("name").cloned(),
            status: props.status.clone(),
            config_digest: entry.config_digest.clone(),
            disposition: disposition.to_owned(),
            owner: entry.owner.clone(),
            allocation,
            cores: props.config.get("cores").cloned(),
            sockets: props.config.get("sockets").cloned(),
            memory: props.config.get("memory").cloned(),
            balloon: props.config.get("balloon").cloned(),
            disks: disks.clone(),
            network,
            proxmox_snapshots: props.snapshots.clone(),
            omitted_keys: props.omitted_keys.clone(),
            managed: AlwaysFalse,
            authorizes_management: AlwaysFalse,
        });
    }
    for vmid in validated.attachments.keys() {
        ensure!(
            seen.contains(vmid),
            "observed VM {vmid} is missing from the mapping"
        );
    }
    for (vmid, disks) in &validated.container_attachments {
        for disk in disks {
            if let Some(dataset) = &disk.dataset
                && let Some(allocation) = allocation_of(dataset)
            {
                bail!(
                    "container {vmid} uses {dataset} inside owned allocation {allocation}; containers cannot be mapped"
                );
            }
        }
    }

    let mut allocations = Vec::new();
    for (dataset, owner) in &owners {
        let props = &s.datasets[dataset];
        let mut volumes = Vec::new();
        for (name, child) in &s.datasets {
            if contained(name, dataset) {
                ensure!(
                    child.kind.as_deref() == Some("volume"),
                    "allocation {dataset} contains nested filesystem {name}; unsupported"
                );
                volumes.push(props_volume(s, name, child, attached.get(name).copied())?);
            }
        }
        allocations.push(ObservedAllocation {
            dataset: dataset.clone(),
            guid: props.guid.clone().unwrap_or_default(),
            owner: owner.clone(),
            quota_bytes: parse_u64(&props.quota, dataset)?.unwrap_or(0),
            reservation_bytes: parse_u64(&props.reservation, dataset)?.unwrap_or(0),
            storage_ids: s
                .storages
                .iter()
                .filter(|(_, st)| st.kind == "zfspool" && st.pool.as_deref() == Some(dataset))
                .map(|(id, _)| id.clone())
                .collect(),
            volumes,
            managed: AlwaysFalse,
            authorizes_management: AlwaysFalse,
        });
    }
    let mut unowned_volumes = Vec::new();
    for (name, props) in &s.datasets {
        if props.kind.as_deref() == Some("volume") && allocation_of(name).is_none() {
            unowned_volumes.push(props_volume(s, name, props, attached.get(name).copied())?);
        }
    }
    observed_vms.sort_by_key(|vm| vm.vmid);
    Ok(ObservedState {
        allocations,
        vms: observed_vms,
        unowned_volumes,
        containers: validated.container_attachments.keys().copied().collect(),
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostRecord {
    pub host_id: String,
    pub node: String,
    pub host_key_sha256: String,
    pub machine_id_sha256: String,
    pub zpool_guid: String,
    pub latest_import_captured_at: Option<String>,
    /// "dataset:GUID" / "vm:VMID" -> owner ("unmanaged" for explicitly unmanaged VMs).
    pub ownership: BTreeMap<String, String>,
    pub managed: AlwaysFalse,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredSnapshot {
    pub host_id: String,
    pub digest: String,
    pub raw_sha256: String,
    pub recorded_at: u64,
    pub snapshot: Snapshot,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    pub seq: u64,
    pub previous: String,
    pub kind: String,
    pub key: String,
    pub record_sha256: String,
    pub entry_digest: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum Outcome {
    Recorded,
    Duplicate,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportOutcome {
    pub outcome: Outcome,
    pub record: ImportRecord,
}

#[derive(Clone, Debug, Serialize)]
pub struct StoreSummary {
    pub hosts: Vec<HostRecord>,
    pub snapshots: Vec<String>,
    pub imports: Vec<String>,
    pub log_entries: u64,
    pub head: String,
}

pub struct ObservationStore {
    database: Database,
}

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn record_key(host_id: &str, digest: &str) -> String {
    format!("{host_id}/{digest}")
}

fn entry_digest(entry: &LogEntry) -> Result<String> {
    canonical_digest(
        "pve-dev-discovery-log/1",
        &(
            entry.seq,
            &entry.previous,
            &entry.kind,
            &entry.key,
            &entry.record_sha256,
        ),
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn rebuild_owners(
    ownership: &mut BTreeMap<String, String>,
    observed: &ObservedState,
) -> Result<()> {
    let mut proposed = BTreeMap::new();
    for allocation in &observed.allocations {
        proposed.insert(
            format!("dataset:{}", allocation.guid),
            allocation.owner.clone(),
        );
    }
    for vm in &observed.vms {
        proposed.insert(
            format!("vm:{}", vm.vmid),
            vm.owner.clone().unwrap_or_else(|| "unmanaged".into()),
        );
    }
    for (key, owner) in &proposed {
        if let Some(previous) = ownership.get(key) {
            ensure!(
                previous == owner,
                "mapping changes recorded ownership of {key} from {previous} to {owner}; refusing (use a new observation database after review)"
            );
        }
    }
    ownership.extend(proposed);
    Ok(())
}

impl ObservationStore {
    /// Opens or creates a dedicated observation database. Refuses any redb
    /// file that is not a discovery store (for example a controller database)
    /// and verifies integrity of existing content.
    pub fn open(path: &Path) -> Result<Self> {
        let existed = path.exists();
        let database = if existed {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_file(),
                "observation database must be a regular file, not a symlink"
            );
            Database::open(path).with_context(|| format!("opening {}", path.display()))?
        } else {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("creating new observation file {}", path.display()))?;
            Database::builder()
                .create_file(file)
                .with_context(|| format!("creating {}", path.display()))?
        };
        let store = Self { database };
        if existed {
            let transaction = store.database.begin_read()?;
            let allowed = [
                META_TABLE,
                HOSTS_TABLE,
                SNAPSHOTS_TABLE,
                IMPORTS_TABLE,
                LOG_TABLE,
            ];
            for table in transaction.list_tables()? {
                ensure!(
                    allowed.contains(&table.name()),
                    "{} contains foreign table {:?}; refusing to use it as an observation store",
                    path.display(),
                    table.name()
                );
            }
            let meta = transaction
                .open_table(META)
                .context("not a pve-dev discovery observation store")?;
            let kind = meta.get("kind")?.context("not a discovery store")?;
            ensure!(
                kind.value() == STORE_KIND.as_bytes(),
                "unsupported observation store kind"
            );
            drop(meta);
            drop(transaction);
            store.verify()?;
        } else {
            let mut transaction = store.database.begin_write()?;
            transaction.set_durability(Durability::Immediate)?;
            {
                let mut meta = transaction.open_table(META)?;
                meta.insert("kind", STORE_KIND.as_bytes())?;
                transaction.open_table(HOSTS)?;
                transaction.open_table(SNAPSHOTS)?;
                transaction.open_table(IMPORTS)?;
                transaction.open_table(LOG)?;
            }
            transaction.commit()?;
            fs::File::open(
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new(".")),
            )?
            .sync_all()?;
        }
        Ok(store)
    }

    fn append_log(
        log: &mut redb::Table<u64, &[u8]>,
        kind: &str,
        key: &str,
        record: &[u8],
    ) -> Result<()> {
        let (seq, previous) = match log.last()? {
            Some((seq, value)) => {
                let entry: LogEntry = serde_json::from_slice(value.value())?;
                (seq.value() + 1, entry.entry_digest)
            }
            None => (0, GENESIS.to_owned()),
        };
        let mut entry = LogEntry {
            seq,
            previous,
            kind: kind.into(),
            key: key.into(),
            record_sha256: sha256_hex(record),
            entry_digest: String::new(),
        };
        entry.entry_digest = entry_digest(&entry)?;
        log.insert(seq, serde_json::to_vec(&entry)?.as_slice())?;
        Ok(())
    }

    fn check_host(
        hosts: &redb::Table<&str, &[u8]>,
        validated: &ValidatedSnapshot,
    ) -> Result<HostRecord> {
        let s = &validated.snapshot;
        let id = validated.host_id();
        let mut existing = None;
        for item in hosts.iter()? {
            let (key, value) = item?;
            let host: HostRecord = serde_json::from_slice(value.value())?;
            if key.value() == id {
                ensure!(
                    host.node == s.host.node
                        && host.machine_id_sha256 == s.host.machine_id_sha256
                        && host.zpool_guid == s.zpool.guid,
                    "host identity changed for pinned host key (node, machine id or zpool GUID differ); refusing"
                );
                existing = Some(host);
            } else {
                ensure!(
                    host.node != s.host.node
                        && host.machine_id_sha256 != s.host.machine_id_sha256
                        && host.zpool_guid != s.zpool.guid,
                    "snapshot host key differs from the key previously recorded for this machine; refusing"
                );
            }
        }
        if let Some(host) = existing {
            return Ok(host);
        }
        Ok(HostRecord {
            host_id: id,
            node: s.host.node.clone(),
            host_key_sha256: s.host.host_key_sha256.clone(),
            machine_id_sha256: s.host.machine_id_sha256.clone(),
            zpool_guid: s.zpool.guid.clone(),
            latest_import_captured_at: None,
            ownership: BTreeMap::new(),
            managed: AlwaysFalse,
        })
    }

    fn put_snapshot(
        hosts: &mut redb::Table<&str, &[u8]>,
        snapshots: &mut redb::Table<&str, &[u8]>,
        log: &mut redb::Table<u64, &[u8]>,
        validated: &ValidatedSnapshot,
    ) -> Result<(HostRecord, Outcome)> {
        let host = Self::check_host(hosts, validated)?;
        let key = record_key(&host.host_id, &validated.digest);
        if let Some(existing) = snapshots.get(key.as_str())? {
            let existing: StoredSnapshot = serde_json::from_slice(existing.value())?;
            ensure!(
                existing.snapshot == validated.snapshot,
                "stored snapshot differs from its digest"
            );
            return Ok((host, Outcome::Duplicate));
        }
        if hosts.get(host.host_id.as_str())?.is_none() {
            let bytes = serde_json::to_vec(&host)?;
            hosts.insert(host.host_id.as_str(), bytes.as_slice())?;
        }
        let bytes = serde_json::to_vec(&StoredSnapshot {
            host_id: host.host_id.clone(),
            digest: validated.digest.clone(),
            raw_sha256: validated.raw_sha256.clone(),
            recorded_at: now(),
            snapshot: validated.snapshot.clone(),
        })?;
        snapshots.insert(key.as_str(), bytes.as_slice())?;
        Self::append_log(log, "snapshot", &key, &bytes)?;
        Ok((host, Outcome::Recorded))
    }

    /// Stores a validated snapshot under its host (no mapping, no ownership).
    pub fn record_snapshot(&self, validated: &ValidatedSnapshot) -> Result<Outcome> {
        let mut transaction = self.database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        let outcome = {
            let mut hosts = transaction.open_table(HOSTS)?;
            let mut snapshots = transaction.open_table(SNAPSHOTS)?;
            let mut log = transaction.open_table(LOG)?;
            Self::put_snapshot(&mut hosts, &mut snapshots, &mut log, validated)?.1
        };
        transaction.commit()?;
        Ok(outcome)
    }

    /// Imports observed state for an explicitly confirmed mapping. Idempotent
    /// for an identical (snapshot, mapping); refuses a different mapping for an
    /// already-imported snapshot, ownership changes, and stale snapshots.
    pub fn import(
        &self,
        validated: &ValidatedSnapshot,
        mapping: &Mapping,
        confirm_digest: &str,
    ) -> Result<ImportOutcome> {
        let observed = check_mapping(validated, mapping, confirm_digest)?;
        let mapping_digest = mapping.digest()?;
        let s = &validated.snapshot;
        let mut transaction = self.database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        let result = {
            let mut hosts = transaction.open_table(HOSTS)?;
            let mut snapshots = transaction.open_table(SNAPSHOTS)?;
            let mut imports = transaction.open_table(IMPORTS)?;
            let mut log = transaction.open_table(LOG)?;
            let (mut host, _) =
                Self::put_snapshot(&mut hosts, &mut snapshots, &mut log, validated)?;
            let key = record_key(&host.host_id, &validated.digest);
            if let Some(existing) = imports.get(key.as_str())? {
                let existing: ImportRecord = serde_json::from_slice(existing.value())?;
                ensure!(
                    existing.mapping_digest == mapping_digest,
                    "snapshot {} was already imported with a different mapping; refusing mapping change",
                    validated.digest
                );
                ImportOutcome {
                    outcome: Outcome::Duplicate,
                    record: existing,
                }
            } else {
                if let Some(latest) = &host.latest_import_captured_at {
                    ensure!(
                        s.captured_at.as_str() > latest.as_str(),
                        "snapshot captured at {} is not newer than the latest import ({latest}); refusing stale import",
                        s.captured_at
                    );
                }
                rebuild_owners(&mut host.ownership, &observed)?;
                host.latest_import_captured_at = Some(s.captured_at.clone());
                let mut record = ImportRecord {
                    schema: IMPORT_SCHEMA.into(),
                    host_id: host.host_id.clone(),
                    snapshot_digest: validated.digest.clone(),
                    mapping_digest,
                    import_digest: String::new(),
                    provenance: Provenance {
                        snapshot_schema: s.schema.clone(),
                        collector: s.collector.clone(),
                        expected_collector_sha256: collector_sha256(&s.zfs_root)?,
                        raw_sha256: validated.raw_sha256.clone(),
                        captured_at: s.captured_at.clone(),
                        node: s.host.node.clone(),
                        host_key_sha256: s.host.host_key_sha256.clone(),
                        machine_id_sha256: s.host.machine_id_sha256.clone(),
                        zpool_guid: s.zpool.guid.clone(),
                        pve_version: s.host.pve_version.clone(),
                        importer: format!("pve-dev-inventory/{}", env!("CARGO_PKG_VERSION")),
                    },
                    observed,
                    warnings: validated.warnings.clone(),
                    imported_at: now(),
                    managed: AlwaysFalse,
                    authorizes_management: AlwaysFalse,
                    creates_controller_state: AlwaysFalse,
                };
                record.import_digest = record.content_digest()?;
                let bytes = serde_json::to_vec(&record)?;
                imports.insert(key.as_str(), bytes.as_slice())?;
                Self::append_log(&mut log, "import", &key, &bytes)?;
                let host_bytes = serde_json::to_vec(&host)?;
                hosts.insert(host.host_id.as_str(), host_bytes.as_slice())?;
                ImportOutcome {
                    outcome: Outcome::Recorded,
                    record,
                }
            }
        };
        if result.outcome == Outcome::Duplicate {
            transaction.abort()?;
        } else {
            transaction.commit()?;
        }
        Ok(result)
    }

    /// Recomputes the hash chain, every record digest and the derived host
    /// ownership. Detects partial edits; it is not a defence against an
    /// attacker who can rewrite the whole file consistently (nothing is signed).
    pub fn verify(&self) -> Result<StoreSummary> {
        let transaction = self.database.begin_read()?;
        let hosts = transaction.open_table(HOSTS)?;
        let snapshots = transaction.open_table(SNAPSHOTS)?;
        let imports = transaction.open_table(IMPORTS)?;
        let log = transaction.open_table(LOG)?;
        let mut previous = GENESIS.to_owned();
        let mut expected_seq = 0;
        let mut logged = BTreeSet::new();
        let mut derived: BTreeMap<String, HostRecord> = BTreeMap::new();
        for item in log.iter()? {
            let (seq, value) = item?;
            let entry: LogEntry =
                serde_json::from_slice(value.value()).context("corrupt log entry")?;
            ensure!(
                seq.value() == expected_seq && entry.seq == expected_seq,
                "log sequence gap at {expected_seq}"
            );
            ensure!(
                entry.previous == previous,
                "log chain broken at {expected_seq}"
            );
            ensure!(
                entry_digest(&entry)? == entry.entry_digest,
                "log entry {expected_seq} digest mismatch"
            );
            let table = match entry.kind.as_str() {
                "snapshot" => &snapshots,
                "import" => &imports,
                other => bail!("unknown log entry kind {other}"),
            };
            let bytes = table
                .get(entry.key.as_str())?
                .with_context(|| format!("logged {} {} is missing", entry.kind, entry.key))?;
            ensure!(
                sha256_hex(bytes.value()) == entry.record_sha256,
                "{} record {} was modified after it was logged",
                entry.kind,
                entry.key
            );
            let (host_id, digest) = entry.key.split_once('/').context("invalid record key")?;
            if entry.kind == "snapshot" {
                let stored: StoredSnapshot = serde_json::from_slice(bytes.value())?;
                ensure!(
                    stored.host_id == host_id
                        && stored.digest == digest
                        && canonical_digest(SNAPSHOT_SCHEMA, &stored.snapshot)? == digest
                        && host_id == self::host_id(&stored.snapshot.host.host_key_sha256),
                    "snapshot record {} does not match its digest",
                    entry.key
                );
                derived
                    .entry(host_id.to_owned())
                    .or_insert_with(|| HostRecord {
                        host_id: host_id.to_owned(),
                        node: stored.snapshot.host.node.clone(),
                        host_key_sha256: stored.snapshot.host.host_key_sha256.clone(),
                        machine_id_sha256: stored.snapshot.host.machine_id_sha256.clone(),
                        zpool_guid: stored.snapshot.zpool.guid.clone(),
                        latest_import_captured_at: None,
                        ownership: BTreeMap::new(),
                        managed: AlwaysFalse,
                    });
            } else {
                let record: ImportRecord = serde_json::from_slice(bytes.value())?;
                ensure!(
                    record.host_id == host_id
                        && record.snapshot_digest == digest
                        && record.content_digest()? == record.import_digest,
                    "import record {} does not match its digest",
                    entry.key
                );
                ensure!(
                    logged.contains(&format!("snapshot:{}", entry.key)),
                    "import without snapshot"
                );
                let host = derived
                    .get_mut(host_id)
                    .context("import for unknown host")?;
                rebuild_owners(&mut host.ownership, &record.observed)?;
                host.latest_import_captured_at = Some(record.provenance.captured_at.clone());
            }
            logged.insert(format!("{}:{}", entry.kind, entry.key));
            previous = entry.entry_digest;
            expected_seq += 1;
        }
        let snapshot_keys: Vec<String> = snapshots
            .iter()?
            .map(|item| item.map(|(key, _)| key.value().to_owned()))
            .collect::<std::result::Result<_, _>>()?;
        let import_keys: Vec<String> = imports
            .iter()?
            .map(|item| item.map(|(key, _)| key.value().to_owned()))
            .collect::<std::result::Result<_, _>>()?;
        for key in &snapshot_keys {
            ensure!(
                logged.contains(&format!("snapshot:{key}")),
                "unlogged snapshot {key}"
            );
        }
        for key in &import_keys {
            ensure!(
                logged.contains(&format!("import:{key}")),
                "unlogged import {key}"
            );
        }
        let mut stored_hosts = BTreeMap::new();
        for item in hosts.iter()? {
            let (key, value) = item?;
            let host: HostRecord =
                serde_json::from_slice(value.value()).context("corrupt host record")?;
            stored_hosts.insert(key.value().to_owned(), host);
        }
        ensure!(
            stored_hosts == derived,
            "host records do not match the logged history (tampered or inconsistent)"
        );
        Ok(StoreSummary {
            hosts: stored_hosts.into_values().collect(),
            snapshots: snapshot_keys,
            imports: import_keys,
            log_entries: expected_seq,
            head: previous,
        })
    }

    pub fn import_record(
        &self,
        host_id: &str,
        snapshot_digest: &str,
    ) -> Result<Option<ImportRecord>> {
        let transaction = self.database.begin_read()?;
        let imports = transaction.open_table(IMPORTS)?;
        imports
            .get(record_key(host_id, snapshot_digest).as_str())?
            .map(|value| serde_json::from_slice(value.value()).map_err(Into::into))
            .transpose()
    }

    pub fn snapshot_record(&self, host_id: &str, digest: &str) -> Result<Option<StoredSnapshot>> {
        let transaction = self.database.begin_read()?;
        let snapshots = transaction.open_table(SNAPSHOTS)?;
        snapshots
            .get(record_key(host_id, digest).as_str())?
            .map(|value| serde_json::from_slice(value.value()).map_err(Into::into))
            .transpose()
    }
}
