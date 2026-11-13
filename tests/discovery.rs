#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use pve_dev::discovery::{
    self, AlwaysFalse, COLLECTOR_VERSION, HostPin, IMPORTS_TABLE, Mapping, ObservationStore,
    ObservedVm, Outcome, ValidatedSnapshot, check_mapping, collector_source, propose_mapping,
    validate_snapshot,
};
use redb::{Database, TableDefinition};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const HOST_KEY: &str = "SHA256:abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const OTHER_KEY: &str = "SHA256:QPONMLKJIHGFEDCBAzyxwvutsrqponmlkjihgfedcba";
const MACHINE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const PEOPLE: [(&str, u32); 3] = [("erin", 101), ("frank", 102), ("heidi", 103)];

fn observation_profile(directory: &TempDir) -> pve_dev::discovery::observation::ObservationProfile {
    use base64::{
        Engine,
        engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    };
    use sha2::{Digest, Sha256};
    let mut key = Vec::new();
    key.extend_from_slice(&11_u32.to_be_bytes());
    key.extend_from_slice(b"ssh-ed25519");
    key.extend_from_slice(&32_u32.to_be_bytes());
    key.extend_from_slice(&[17_u8; 32]);
    let known_hosts = format!("127.0.0.1 ssh-ed25519 {}\n", STANDARD.encode(&key));
    let path = directory.path().join("known_hosts");
    fs::write(&path, &known_hosts).unwrap();
    serde_json::from_value(json!({
        "schema":"pve-dev-observation-profile/1", "destination":"root@127.0.0.1", "port":22,
        "known_hosts":path, "known_hosts_sha256":format!("{:x}", Sha256::digest(known_hosts.as_bytes())),
        "node":"pve1", "host_key_sha256":format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&key))),
        "machine_id_sha256":MACHINE, "zpool_guid":"424242", "zfs_root":"rpool/data",
        "vm_notes":{"103":"Recovery copy · original HDD"}
    })).unwrap()
}

#[test]
fn observation_pins_known_hosts_and_refuses_replacement_before_collection() {
    let directory = TempDir::new().unwrap();
    let profile = observation_profile(&directory);
    profile.validate().unwrap();
    profile.checked_known_hosts().unwrap();
    let binding = profile.public_identity().binding;
    let mut replaced = profile.clone();
    replaced.known_hosts_sha256 = "9".repeat(64);
    assert_ne!(replaced.public_identity().binding, binding);
    assert!(replaced.checked_known_hosts().is_err());
    fs::write(&profile.known_hosts, "127.0.0.1 ssh-ed25519 AAAA\n").unwrap();
    assert!(profile.checked_known_hosts().is_err());
}

#[test]
fn observation_ssh_route_and_identity_pins_fail_closed() {
    let directory = TempDir::new().unwrap();
    let profile = observation_profile(&directory);
    for destination in ["-oProxyCommand=bad", "root@host;whoami", "root@host\nother"] {
        let mut bad = profile.clone();
        bad.destination = destination.into();
        assert!(bad.validate().is_err());
    }
    let mut value = snapshot();
    value["host"]["host_key_sha256"] = json!(profile.host_key_sha256);
    profile
        .validate_capture(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    value["host"]["machine_id_sha256"] = json!("9".repeat(64));
    assert!(
        profile
            .validate_capture(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    value["host"]["machine_id_sha256"] = json!(MACHINE);
    value["zpool"]["guid"] = json!("9999");
    assert!(
        profile
            .validate_capture(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
}

#[test]
fn observation_summary_is_host_scoped_and_never_grants_authority() {
    use pve_dev::discovery::observation::{ObservationResult, summarize};
    let directory = TempDir::new().unwrap();
    let profile = observation_profile(&directory);
    let mut value = snapshot();
    value["host"]["host_key_sha256"] = json!(profile.host_key_sha256);
    value["vms"]["103"]["status"] = json!("stopped");
    let validated = profile
        .validate_capture(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    let summary = summarize(&profile, &validated).unwrap();
    let mut other_profile = profile.clone();
    other_profile.zpool_guid = "9999".into();
    assert!(summarize(&other_profile, &validated).is_err());
    let vm = summary.vms.iter().find(|vm| vm.vmid == 103).unwrap();
    assert_eq!(vm.status, "stopped");
    assert_eq!(vm.note.as_deref(), Some("Recovery copy · original HDD"));
    assert_eq!(vm.id, format!("{}/103", profile.public_identity().host_id));
    assert_eq!(vm.disk_bytes, 1_374_389_534_720);
    let mut output = serde_json::to_value(&summary).unwrap();
    assert_never_managed(&output);
    for field in [
        "managed",
        "authorizes_management",
        "creates_controller_state",
    ] {
        output[field] = json!(true);
        assert!(serde_json::from_value::<ObservationResult>(output.clone()).is_err());
        output[field] = json!(false);
    }
}

#[test]
fn observation_cli_trust_is_local_and_wrong_binding_cannot_connect() {
    let directory = TempDir::new().unwrap();
    let profile = observation_profile(&directory);
    let path = directory.path().join("host.json");
    fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let output = cli(&["observe-profile-info", "--profile", path.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["binding"],
        json!(profile.public_identity().binding)
    );
    let output = cli(&[
        "observe",
        "--profile",
        path.to_str().unwrap(),
        "--expected-binding",
        &"0".repeat(64),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("profile identity changed"));
    assert!(!directory.path().join("state.redb").exists());
}

fn pin() -> HostPin {
    HostPin {
        node: "pve1".into(),
        host_key_sha256: HOST_KEY.into(),
    }
}

fn filesystem(guid: u64, quota: u64) -> Value {
    json!({"type":"filesystem","guid":guid.to_string(),"quota":quota.to_string(),"reservation":quota.to_string(),
        "refreservation":"0","volsize":null,"origin":null,"used":"4096","available":"1000000","mountpoint":"none","canmount":"off"})
}

fn volume(guid: u64, size: u64) -> Value {
    json!({"type":"volume","guid":guid.to_string(),"quota":null,"reservation":"0","refreservation":(size + size / 50).to_string(),
        "volsize":size.to_string(),"origin":null,"used":size.to_string(),"available":"1000000","mountpoint":null,"canmount":null})
}

/// Synthetic snapshot modelled on the three-person topology. No real keys,
/// snippets, or secrets are included.
fn snapshot() -> Value {
    let mut datasets = Map::new();
    datasets.insert("rpool/data".into(), filesystem(1000, 0));
    let mut storages = Map::new();
    storages.insert(
        "local".into(),
        json!({"type":"dir","path":"/var/lib/vz","content":"iso,snippets"}),
    );
    storages.insert(
        "local-zfs".into(),
        json!({"type":"zfspool","pool":"rpool/data","disable":"1","sparse":"1"}),
    );
    let mut vms = Map::new();
    let mut pools = Map::new();
    let mut snapshots = Map::new();
    for (index, (person, vmid)) in PEOPLE.into_iter().enumerate() {
        let base = 1001 + index as u64;
        let allocation = format!("rpool/data/dev-{person}");
        datasets.insert(allocation.clone(), filesystem(base, 1_649_267_441_664));
        datasets.insert(
            format!("{allocation}/vm-{vmid}-disk-0"),
            volume(base * 10, 1_374_389_534_720),
        );
        datasets.insert(
            format!("{allocation}/vm-{vmid}-cloudinit"),
            volume(base * 10 + 1, 4_194_304),
        );
        storages.insert(format!("dev-{person}"), json!({"type":"zfspool","pool":allocation,"content":"images","nodes":"pve1","sparse":"0","blocksize":"16k"}));
        vms.insert(vmid.to_string(), json!({
            "status": "running",
            "config": {
                "digest": format!("{vmid}a7852019c9d83fe14cd39d047a395bd5ea8a93"),
                "name": format!("dev-{person}"), "cores": "8", "sockets": "1", "memory": "current=32768", "balloon": "12288",
                "scsi0": format!("dev-{person}:vm-{vmid}-disk-0,cache=none,discard=on,iothread=1,size=1280G"),
                "ide2": format!("dev-{person}:vm-{vmid}-cloudinit,media=cdrom"),
                "net0": format!("virtio=BC:24:11:00:00:{vmid:02X},bridge=vmbr1,firewall=1"),
                "ipconfig0": format!("ip=10.50.0.{}/16,gw=10.50.0.1", index + 11),
                "boot": "order=scsi0"
            },
            "omitted_keys": ["cicustom", "description", "smbios1", "sshkeys", "vmgenid"],
            "pending_keys": [],
            "snapshots": []
        }));
        pools.insert(format!("dev-{person}"), json!({"comment": person, "members": [format!("qemu/{vmid}"), format!("storage/pve1/dev-{person}")]}));
    }
    snapshots.insert(
        "rpool/data/dev-erin/vm-101-disk-0@pre".into(),
        json!({"guid":"9101","used":"0","createtxg":"77"}),
    );
    vms.get_mut("101").unwrap()["snapshots"] = json!(["pre"]);
    json!({
        "schema": "pve-dev-discovery-snapshot/1",
        "collector": COLLECTOR_VERSION,
        "captured_at": "2026-11-09T12:00:00Z",
        "host": {"node":"pve1","host_key_sha256":HOST_KEY,"machine_id_sha256":MACHINE,"pve_version":"pve-manager/9.0.11/abc","cluster_nodes":["pve1"]},
        "zfs_root": "rpool/data",
        "zpool": {"name":"rpool","guid":"424242","health":"ONLINE"},
        "datasets": datasets,
        "zfs_snapshots": snapshots,
        "storages": storages,
        "pools": pools,
        "network": {"vmbr0":{"type":"bridge","cidr":"203.0.113.10/26"},"vmbr1":{"type":"bridge","cidr":"10.50.0.1/16"}},
        "vms": vms,
        "containers": {},
        "consistency": {"zfs_stable":true,"guests_stable":true,"config_stable":true}
    })
}

fn validated(value: &Value) -> ValidatedSnapshot {
    validate_snapshot(&serde_json::to_vec(value).unwrap(), &pin()).unwrap()
}

fn refuse(value: &Value, needle: &str) {
    let error = validate_snapshot(&serde_json::to_vec(value).unwrap(), &pin())
        .err()
        .unwrap_or_else(|| panic!("snapshot accepted; expected refusal containing {needle:?}"));
    let text = format!("{error:#}");
    assert!(
        text.contains(needle),
        "unexpected error {text:?}, wanted {needle:?}"
    );
}

fn confirmed(validated: &ValidatedSnapshot) -> Mapping {
    let mut mapping = propose_mapping(validated);
    for allocation in &mut mapping.allocations {
        allocation.owner = Some(
            allocation
                .dataset
                .trim_start_matches("rpool/data/dev-")
                .into(),
        );
    }
    for vm in &mut mapping.vms {
        let Some((owner, _)) = PEOPLE.iter().find(|(_, id)| *id == vm.vmid) else {
            continue;
        };
        vm.disposition = Some("owned".into());
        vm.owner = Some((*owner).into());
    }
    mapping
}

fn mapping_error(validated: &ValidatedSnapshot, mapping: &Mapping) -> String {
    format!(
        "{:#}",
        check_mapping(validated, mapping, &validated.digest).expect_err("mapping accepted")
    )
}

fn assert_never_managed(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if [
                    "managed",
                    "authorizes_management",
                    "creates_controller_state",
                ]
                .contains(&key.as_str())
                {
                    assert_eq!(child, &Value::Bool(false), "{key} must be false");
                }
                assert_never_managed(child);
            }
        }
        Value::Array(items) => items.iter().for_each(assert_never_managed),
        _ => {}
    }
}

fn count_flags(value: &Value) -> usize {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, child)| usize::from(key == "managed") + count_flags(child))
            .sum(),
        Value::Array(items) => items.iter().map(count_flags).sum(),
        _ => 0,
    }
}

#[test]
fn valid_snapshot_has_stable_digest_independent_of_formatting() {
    let value = snapshot();
    let first = crate::validated(&value);
    let pretty = serde_json::to_string_pretty(&value).unwrap();
    let second = validate_snapshot(pretty.as_bytes(), &pin()).unwrap();
    assert_eq!(first.digest, second.digest);
    assert_ne!(
        first.raw_sha256, second.raw_sha256,
        "raw bytes differ, digest must not"
    );
    assert_eq!(first.digest.len(), 64);
    assert_eq!(first.attachments.len(), 3);
    let report = first.report();
    assert_eq!(
        (report.vms, report.volumes, report.zfs_snapshots),
        (3, 6, 1)
    );
    assert!(
        report.warnings.iter().any(|w| w
            .contains("snapshot pre has no ZFS snapshot on rpool/data/dev-erin/vm-101-cloudinit")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn any_content_change_changes_the_digest() {
    let base = crate::validated(&snapshot()).digest;
    let mut changed = snapshot();
    changed["datasets"]["rpool/data/dev-frank"]["quota"] = json!("1");
    assert_ne!(validated(&changed).digest, base);
    let mut changed = snapshot();
    changed["vms"]["102"]["status"] = json!("stopped");
    assert_ne!(validated(&changed).digest, base);
}

#[test]
fn host_pin_mismatch_is_refused() {
    let bytes = serde_json::to_vec(&snapshot()).unwrap();
    let wrong_key = HostPin {
        node: "pve1".into(),
        host_key_sha256: OTHER_KEY.into(),
    };
    assert!(
        format!("{:#}", validate_snapshot(&bytes, &wrong_key).unwrap_err())
            .contains("pinned host key")
    );
    let wrong_node = HostPin {
        node: "pve2".into(),
        host_key_sha256: HOST_KEY.into(),
    };
    assert!(
        format!("{:#}", validate_snapshot(&bytes, &wrong_node).unwrap_err())
            .contains("pinned node")
    );
}

#[test]
fn incomplete_or_invalid_snapshots_are_refused() {
    let mut value = snapshot();
    value.as_object_mut().unwrap().remove("consistency");
    refuse(&value, "missing or unknown fields");

    let mut value = snapshot();
    value["unexpected"] = json!(1);
    refuse(&value, "missing or unknown fields");

    let mut value = snapshot();
    value["datasets"]["rpool/data/dev-erin"]
        .as_object_mut()
        .unwrap()
        .remove("guid");
    refuse(&value, "missing or unknown fields");

    let mut value = snapshot();
    value["datasets"]["rpool/data/dev-erin"]["guid"] = Value::Null;
    refuse(&value, "missing GUID");

    let mut value = snapshot();
    value["consistency"]["zfs_stable"] = json!(false);
    refuse(&value, "changed during collection");

    let mut value = snapshot();
    value["datasets"]
        .as_object_mut()
        .unwrap()
        .remove("rpool/data/dev-frank/vm-102-disk-0");
    refuse(&value, "missing from the snapshot (incomplete inventory)");

    let mut value = snapshot();
    value["datasets"]
        .as_object_mut()
        .unwrap()
        .remove("rpool/data/dev-heidi");
    refuse(&value, "missing (incomplete inventory)");

    let mut value = snapshot();
    value["storages"]
        .as_object_mut()
        .unwrap()
        .remove("dev-erin");
    refuse(&value, "is not in the snapshot (incomplete inventory)");

    let mut value = snapshot();
    value["datasets"]["rpool/data/dev-frank"]["guid"] = json!("1001");
    refuse(&value, "duplicate ZFS GUID");

    let mut value = snapshot();
    value["zfs_snapshots"]["rpool/data/dev-gone/vm-1@x"] =
        json!({"guid":"5","used":"0","createtxg":"1"});
    refuse(&value, "missing from the snapshot");

    let mut value = snapshot();
    value["host"]["cluster_nodes"] = json!(["pve1", "pve2"]);
    refuse(&value, "multi-node");

    let mut value = snapshot();
    value["vms"]["101"]["config"]
        .as_object_mut()
        .unwrap()
        .remove("digest");
    refuse(&value, "missing config digest");

    let mut value = snapshot();
    value["vms"]["101"]["config"]["scsi1"] = json!("dev-frank:vm-102-disk-0,size=1G");
    refuse(&value, "attached to both");

    let mut value = snapshot();
    value["collector"] = json!("pve-dev-discovery-collector/0");
    refuse(&value, "collector version");

    let mut value = snapshot();
    value["captured_at"] = json!("yesterday");
    refuse(&value, "captured_at");

    refuse(&json!([]), "missing or unknown fields");
    assert!(validate_snapshot(b"{not json", &pin()).is_err());
}

#[test]
fn secret_material_is_refused() {
    let mut value = snapshot();
    value["vms"]["101"]["config"]["sshkeys"] = json!("ssh-ed25519%20AAAA");
    refuse(&value, "secret-bearing field");

    let mut value = snapshot();
    value["vms"]["101"]["config"]["cicustom"] = json!("user=local:snippets/x.yaml");
    refuse(&value, "secret-bearing field");

    let mut value = snapshot();
    value["vms"]["101"]["config"]["hookscript"] = json!("local:snippets/hook.pl");
    refuse(&value, "secret-bearing field");

    let mut value = snapshot();
    value["pools"]["dev-erin"]["comment"] = json!("-----BEGIN OPENSSH PRIVATE KEY-----");
    refuse(&value, "key or cloud-init material");

    let mut value = snapshot();
    value["vms"]["101"]["config"]["smbios1"] = json!("uuid=x");
    refuse(&value, "not allow-listed");
}

#[test]
fn proposal_never_assigns_owners_and_cannot_be_imported_unconfirmed() {
    let validated = crate::validated(&snapshot());
    let proposal = propose_mapping(&validated);
    assert_eq!(
        proposal.allocations.len(),
        3,
        "disabled root storage is not an allocation"
    );
    assert!(proposal.allocations.iter().all(|a| a.owner.is_none()));
    assert!(
        proposal
            .vms
            .iter()
            .all(|vm| vm.owner.is_none() && vm.disposition.is_none())
    );
    assert_eq!(proposal.vms.len(), 3);
    assert_eq!(proposal.vms[0].disks.len(), 2);
    assert!(mapping_error(&validated, &proposal).contains("no explicit owner"));

    let mut only_allocations = proposal.clone();
    for allocation in &mut only_allocations.allocations {
        allocation.owner = Some("erin".into());
    }
    let error = mapping_error(&validated, &only_allocations);
    assert!(error.contains("no explicit disposition"), "{error}");
}

#[test]
fn confirmed_mapping_produces_observed_state_only() {
    let validated = crate::validated(&snapshot());
    let mapping = confirmed(&validated);
    let observed = check_mapping(&validated, &mapping, &validated.digest).unwrap();
    assert_eq!(observed.allocations.len(), 3);
    let frank = observed
        .allocations
        .iter()
        .find(|a| a.owner == "frank")
        .unwrap();
    assert_eq!(frank.storage_ids, ["dev-frank"]);
    assert_eq!(frank.quota_bytes, 1_649_267_441_664);
    assert_eq!(frank.volumes.len(), 2);
    assert!(
        frank
            .volumes
            .iter()
            .all(|v| v.attached_to == Some(102) && v.fully_reserved)
    );
    let erin_vm = observed.vms.iter().find(|vm| vm.vmid == 101).unwrap();
    assert_eq!(erin_vm.allocation.as_deref(), Some("rpool/data/dev-erin"));
    assert_eq!(erin_vm.network[0].model.as_deref(), Some("virtio"));
    assert_eq!(erin_vm.network[0].mac.as_deref(), Some("BC:24:11:00:00:65"));
    assert_eq!(erin_vm.network[0].bridge.as_deref(), Some("vmbr1"));
    assert!(erin_vm.network[0].firewall);
    assert_eq!(erin_vm.proxmox_snapshots, ["pre"]);
    let value = serde_json::to_value(&observed).unwrap();
    assert_never_managed(&value);
    assert_eq!(
        count_flags(&value),
        6,
        "every allocation and VM carries managed=false"
    );
}

#[test]
fn nic_model_and_mac_are_preserved_independent_of_option_order() {
    for nic in [
        "e1000=BC:24:11:AA:BB:CC,bridge=vmbr1,firewall=1",
        "bridge=vmbr1,firewall=1,e1000=BC:24:11:AA:BB:CC",
    ] {
        let mut raw = snapshot();
        raw["vms"]["101"]["config"]["net0"] = json!(nic);
        let validated = crate::validated(&raw);
        let mapping = confirmed(&validated);
        let observed = check_mapping(&validated, &mapping, &validated.digest).unwrap();
        let vm = observed.vms.iter().find(|vm| vm.vmid == 101).unwrap();
        assert_eq!(vm.network[0].model.as_deref(), Some("e1000"));
        assert_eq!(vm.network[0].mac.as_deref(), Some("BC:24:11:AA:BB:CC"));
        assert_eq!(vm.network[0].bridge.as_deref(), Some("vmbr1"));
        assert!(vm.network[0].firewall);
    }
}

#[test]
fn unsafe_mappings_are_refused() {
    let validated = crate::validated(&snapshot());
    let good = confirmed(&validated);

    let mut mapping = good.clone();
    mapping.vms[1].owner = Some("erin".into());
    assert!(mapping_error(&validated, &mapping).contains("owned by frank, not erin"));

    let mut mapping = good.clone();
    mapping.allocations[0].guid = "1003".into();
    assert!(mapping_error(&validated, &mapping).contains("GUID mismatch"));

    let mut mapping = good.clone();
    mapping.vms[0].disks.insert("scsi0".into(), "10030".into());
    assert!(mapping_error(&validated, &mapping).contains("do not match its attached disks"));

    let mut mapping = good.clone();
    mapping.vms[0].disks.remove("ide2");
    assert!(mapping_error(&validated, &mapping).contains("do not match its attached disks"));

    let mut mapping = good.clone();
    mapping.vms.pop();
    assert!(mapping_error(&validated, &mapping).contains("missing from the mapping"));
    let mut mapping = good.clone();
    let mut extra = mapping.vms[0].clone();
    extra.vmid = 999;
    mapping.vms.push(extra);
    assert!(mapping_error(&validated, &mapping).contains("was not observed"));
    let mut mapping = good.clone();
    mapping.vms.push(mapping.vms[0].clone());
    assert!(mapping_error(&validated, &mapping).contains("mapped twice"));

    let mut mapping = good.clone();
    mapping
        .allocations
        .retain(|a| a.dataset != "rpool/data/dev-heidi");
    assert!(mapping_error(&validated, &mapping).contains("outside every mapped allocation"));

    let mut mapping = good.clone();
    mapping.vms[2].disposition = Some("unmanaged".into());
    mapping.vms[2].owner = None;
    assert!(mapping_error(&validated, &mapping).contains("inside allocation"));

    let mut mapping = good.clone();
    mapping.allocations.push(discovery::AllocationMapping {
        dataset: "rpool/data".into(),
        guid: "1000".into(),
        owner: Some("erin".into()),
        name_hint: None,
    });
    assert!(mapping_error(&validated, &mapping).contains("below the ZFS root"));

    let mut mapping = good.clone();
    mapping.allocations[0].owner = Some("Frank Smith".into());
    assert!(mapping_error(&validated, &mapping).contains("invalid owner"));
    let error = check_mapping(&validated, &good, &"0".repeat(64)).unwrap_err();
    assert!(format!("{error:#}").contains("confirmation digest"));
    let mut mapping = good.clone();
    mapping.snapshot_digest = "0".repeat(64);
    assert!(mapping_error(&validated, &mapping).contains("different snapshot"));
    let mut mapping = good.clone();
    mapping.host_key_sha256 = OTHER_KEY.into();
    assert!(mapping_error(&validated, &mapping).contains("host does not match"));

    let mut mapping = good.clone();
    mapping.vms[0].config_digest = "stale".into();
    assert!(mapping_error(&validated, &mapping).contains("config digest mismatch"));

    let mut mapping = good.clone();
    mapping.vms[0].disposition = Some("adopt".into());
    assert!(mapping_error(&validated, &mapping).contains("unknown disposition"));
}

#[test]
fn unsafe_vm_states_are_refused_for_owned_mapping() {
    for (field, value, needle) in [
        ("lock", json!("backup"), "locked"),
        ("scsi1", json!("/dev/sdb,size=1G"), "passthrough"),
        (
            "scsi2",
            json!("local:101/vm-101-disk-9.qcow2,size=1G"),
            "external",
        ),
    ] {
        let mut raw = snapshot();
        raw["vms"]["101"]["config"][field] = value;
        let validated = crate::validated(&raw);
        let error = mapping_error(&validated, &confirmed(&validated));
        assert!(error.contains(needle), "{field}: {error}");
    }
    let mut raw = snapshot();
    raw["vms"]["101"]["pending_keys"] = json!(["memory"]);
    let validated_pending = crate::validated(&raw);
    assert!(mapping_error(&validated_pending, &confirmed(&validated_pending)).contains("pending"));

    let mut raw = snapshot();
    raw["datasets"]["rpool/data/dev-erin2"] = filesystem(1500, 1);
    raw["datasets"]["rpool/data/dev-erin2/vm-101-disk-1"] = volume(1501, 10);
    raw["storages"]["dev-erin2"] = json!({"type":"zfspool","pool":"rpool/data/dev-erin2"});
    raw["vms"]["101"]["config"]["scsi1"] = json!("dev-erin2:vm-101-disk-1,size=10");
    let validated = crate::validated(&raw);
    let mut mapping = confirmed(&validated);
    for allocation in &mut mapping.allocations {
        if allocation.dataset.ends_with("erin2") {
            allocation.owner = Some("erin".into());
        }
    }
    assert!(mapping_error(&validated, &mapping).contains("spans multiple allocations"));

    let mut raw = snapshot();
    raw["datasets"]["rpool/data/dev-erin/subvol-200-disk-0"] = filesystem(1600, 0);
    raw["containers"]["200"] = json!({"status":"running","config":{"rootfs":"dev-erin:subvol-200-disk-0,size=8G","digest":"x"},"omitted_keys":[]});
    let validated = crate::validated(&raw);
    assert!(
        mapping_error(&validated, &confirmed(&validated)).contains("containers cannot be mapped")
    );
}

#[test]
fn unmanaged_vm_outside_allocations_is_observed_without_owner() {
    let mut raw = snapshot();
    raw["datasets"]["rpool/data/vm-150-disk-0"] = volume(1700, 10);
    raw["vms"]["150"] = json!({"status":"stopped","config":{"digest":"d150","name":"scratch","scsi0":"local-zfs:vm-150-disk-0,size=10"},
        "omitted_keys":[],"pending_keys":[],"snapshots":[]});
    let validated = crate::validated(&raw);
    let mut mapping = confirmed(&validated);
    assert!(mapping_error(&validated, &mapping).contains("no explicit disposition"));
    let scratch = mapping.vms.iter_mut().find(|vm| vm.vmid == 150).unwrap();
    scratch.disposition = Some("unmanaged".into());
    let observed = check_mapping(&validated, &mapping, &validated.digest).unwrap();
    let vm = observed.vms.iter().find(|vm| vm.vmid == 150).unwrap();
    assert_eq!(
        (vm.owner.as_deref(), vm.allocation.as_deref()),
        (None, None)
    );
    assert_eq!(observed.unowned_volumes.len(), 1);
    assert_never_managed(&serde_json::to_value(&observed).unwrap());
}

#[test]
fn managed_flags_cannot_be_deserialized_true() {
    assert!(serde_json::from_str::<AlwaysFalse>("true").is_err());
    assert!(!serde_json::from_str::<AlwaysFalse>("false").unwrap().get());
    let validated = crate::validated(&snapshot());
    let observed = check_mapping(&validated, &confirmed(&validated), &validated.digest).unwrap();
    let mut vm = serde_json::to_value(&observed.vms[0]).unwrap();
    vm["managed"] = json!(true);
    assert!(serde_json::from_value::<ObservedVm>(vm).is_err());
}

fn store_path(directory: &TempDir) -> PathBuf {
    directory.path().join("observations.redb")
}

#[test]
fn observation_files_are_private_and_symlinks_are_refused() {
    let directory = TempDir::new().unwrap();
    let path = store_path(&directory);
    drop(ObservationStore::open(&path).unwrap());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let alias = directory.path().join("alias.redb");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert!(ObservationStore::open(&alias).is_err());
    ObservationStore::open(&path).unwrap().verify().unwrap();
}

#[test]
fn import_is_durable_deduplicated_and_never_managed() {
    let directory = TempDir::new().unwrap();
    let path = store_path(&directory);
    let validated = crate::validated(&snapshot());
    let mapping = confirmed(&validated);
    let record = {
        let store = ObservationStore::open(&path).unwrap();
        let first = store
            .import(&validated, &mapping, &validated.digest)
            .unwrap();
        assert_eq!(first.outcome, Outcome::Recorded);
        let again = store
            .import(&validated, &mapping, &validated.digest)
            .unwrap();
        assert_eq!(again.outcome, Outcome::Duplicate);
        assert_eq!(again.record, first.record);
        assert_eq!(
            store.verify().unwrap().log_entries,
            2,
            "one snapshot + one import"
        );
        first.record
    };
    let value = serde_json::to_value(&record).unwrap();
    assert_never_managed(&value);
    assert_eq!(record.provenance.collector, COLLECTOR_VERSION);
    assert_eq!(record.provenance.host_key_sha256, HOST_KEY);
    assert_eq!(record.provenance.raw_sha256, validated.raw_sha256);
    assert_eq!(
        record.provenance.expected_collector_sha256,
        discovery::collector_sha256("rpool/data").unwrap()
    );

    let store = ObservationStore::open(&path).unwrap();
    let summary = store.verify().unwrap();
    assert_eq!(
        (
            summary.snapshots.len(),
            summary.imports.len(),
            summary.log_entries
        ),
        (1, 1, 2)
    );
    assert_eq!(summary.hosts[0].ownership["vm:102"], "frank");
    assert_eq!(summary.hosts[0].ownership["dataset:1003"], "heidi");
    let mut hinted = mapping.clone();
    hinted.vms[0].name_hint = Some("renamed".into());
    hinted.vms.reverse();
    let again = store
        .import(&validated, &hinted, &validated.digest)
        .unwrap();
    assert_eq!(again.outcome, Outcome::Duplicate);
    assert_eq!(
        store
            .import_record(&validated.host_id(), &validated.digest)
            .unwrap()
            .unwrap(),
        record
    );
    assert!(
        store
            .snapshot_record(&validated.host_id(), &validated.digest)
            .unwrap()
            .is_some()
    );
    assert_eq!(store.verify().unwrap().log_entries, 2);
    drop(store);

    let database = Database::open(&path).unwrap();
    let transaction = redb::ReadableDatabase::begin_read(&database).unwrap();
    let tables: Vec<String> = transaction
        .list_tables()
        .unwrap()
        .map(|table| redb::TableHandle::name(&table).to_owned())
        .collect();
    assert!(
        tables.iter().all(|name| name.starts_with("discovery_")),
        "{tables:?}"
    );
}

#[test]
fn recording_snapshots_without_mapping_assigns_no_ownership() {
    let directory = TempDir::new().unwrap();
    let store = ObservationStore::open(&store_path(&directory)).unwrap();
    let validated = crate::validated(&snapshot());
    assert_eq!(
        store.record_snapshot(&validated).unwrap(),
        Outcome::Recorded
    );
    assert_eq!(
        store.record_snapshot(&validated).unwrap(),
        Outcome::Duplicate
    );
    let summary = store.verify().unwrap();
    assert!(summary.hosts[0].ownership.is_empty());
    assert!(summary.imports.is_empty());
    assert!(
        store
            .import_record(&validated.host_id(), &validated.digest)
            .unwrap()
            .is_none()
    );
}

#[test]
fn mapping_changes_and_stale_imports_are_refused() {
    let directory = TempDir::new().unwrap();
    let path = store_path(&directory);
    let store = ObservationStore::open(&path).unwrap();
    let first = crate::validated(&snapshot());
    store
        .import(&first, &confirmed(&first), &first.digest)
        .unwrap();

    let mut swapped = confirmed(&first);
    for allocation in &mut swapped.allocations {
        if allocation.owner.as_deref() == Some("heidi") {
            allocation.owner = Some("dan".into());
        }
    }
    for vm in &mut swapped.vms {
        if vm.owner.as_deref() == Some("heidi") {
            vm.owner = Some("dan".into());
        }
    }
    check_mapping(&first, &swapped, &first.digest).unwrap();
    let error = store.import(&first, &swapped, &first.digest).unwrap_err();
    assert!(
        format!("{error:#}").contains("different mapping"),
        "{error:#}"
    );

    let mut later = snapshot();
    later["captured_at"] = json!("2026-11-10T12:00:00Z");
    let later = crate::validated(&later);
    let mut remapped = confirmed(&later);
    for allocation in &mut remapped.allocations {
        if allocation.owner.as_deref() == Some("heidi") {
            allocation.owner = Some("dan".into());
        }
    }
    for vm in &mut remapped.vms {
        if vm.owner.as_deref() == Some("heidi") {
            vm.owner = Some("dan".into());
        }
    }
    let error = store.import(&later, &remapped, &later.digest).unwrap_err();
    assert!(
        format!("{error:#}").contains("changes recorded ownership"),
        "{error:#}"
    );
    assert_eq!(
        store.verify().unwrap().imports.len(),
        1,
        "refusals leave no partial state"
    );

    assert_eq!(
        store
            .import(&later, &confirmed(&later), &later.digest)
            .unwrap()
            .outcome,
        Outcome::Recorded
    );

    let mut older = snapshot();
    older["captured_at"] = json!("2026-11-01T12:00:00Z");
    let older = crate::validated(&older);
    let error = store
        .import(&older, &confirmed(&older), &older.digest)
        .unwrap_err();
    assert!(format!("{error:#}").contains("stale"), "{error:#}");
    drop(store);
    let summary = ObservationStore::open(&path).unwrap().verify().unwrap();
    assert_eq!(summary.imports.len(), 2);
    assert_eq!(
        summary.snapshots.len(),
        2,
        "refused imports did not record their snapshots"
    );
}

#[test]
fn host_identity_changes_are_refused() {
    let directory = TempDir::new().unwrap();
    let store = ObservationStore::open(&store_path(&directory)).unwrap();
    let first = crate::validated(&snapshot());
    store.record_snapshot(&first).unwrap();

    let mut changed = snapshot();
    changed["captured_at"] = json!("2026-11-10T00:00:00Z");
    changed["host"]["machine_id_sha256"] = json!("2".repeat(64));
    let error = store.record_snapshot(&validated(&changed)).unwrap_err();
    assert!(
        format!("{error:#}").contains("host identity changed"),
        "{error:#}"
    );

    let mut rekeyed = snapshot();
    rekeyed["host"]["host_key_sha256"] = json!(OTHER_KEY);
    let rekeyed = validate_snapshot(
        &serde_json::to_vec(&rekeyed).unwrap(),
        &HostPin {
            node: "pve1".into(),
            host_key_sha256: OTHER_KEY.into(),
        },
    )
    .unwrap();
    let error = store.record_snapshot(&rekeyed).unwrap_err();
    assert!(
        format!("{error:#}").contains("differs from the key previously recorded"),
        "{error:#}"
    );
}

fn tamper(path: &Path, table: &str, edit: impl Fn(&mut Value)) {
    let database = Database::open(path).unwrap();
    let definition: TableDefinition<&str, &[u8]> = TableDefinition::new(table);
    let transaction = database.begin_write().unwrap();
    {
        let mut table = transaction.open_table(definition).unwrap();
        let (key, mut value) = {
            let mut iter = redb::ReadableTable::iter(&table).unwrap();
            let (key, value) = iter.next().unwrap().unwrap();
            (
                key.value().to_owned(),
                serde_json::from_slice::<Value>(value.value()).unwrap(),
            )
        };
        edit(&mut value);
        table
            .insert(key.as_str(), serde_json::to_vec(&value).unwrap().as_slice())
            .unwrap();
    }
    transaction.commit().unwrap();
}

fn imported_store(directory: &TempDir) -> PathBuf {
    let path = store_path(directory);
    let store = ObservationStore::open(&path).unwrap();
    let validated = crate::validated(&snapshot());
    store
        .import(&validated, &confirmed(&validated), &validated.digest)
        .unwrap();
    path
}

#[test]
fn tampered_database_is_refused_on_open() {
    let directory = TempDir::new().unwrap();
    let path = imported_store(&directory);
    tamper(&path, IMPORTS_TABLE, |record| {
        record["observed"]["vms"][1]["owner"] = json!("erin");
    });
    let error = ObservationStore::open(&path).err().unwrap();
    assert!(
        format!("{error:#}").contains("modified after it was logged"),
        "{error:#}"
    );

    let directory = TempDir::new().unwrap();
    let path = imported_store(&directory);
    tamper(&path, discovery::HOSTS_TABLE, |host| {
        host["ownership"]["vm:102"] = json!("erin");
    });
    let error = ObservationStore::open(&path).err().unwrap();
    assert!(
        format!("{error:#}").contains("do not match the logged history"),
        "{error:#}"
    );

    let directory = TempDir::new().unwrap();
    let path = imported_store(&directory);
    tamper(&path, discovery::SNAPSHOTS_TABLE, |stored| {
        stored["snapshot"]["datasets"]["rpool/data/dev-frank"]["quota"] = json!("1");
    });
    assert!(ObservationStore::open(&path).is_err());

    let directory = TempDir::new().unwrap();
    let path = imported_store(&directory);
    tamper(&path, IMPORTS_TABLE, |record| {
        record["managed"] = json!(true);
    });
    assert!(ObservationStore::open(&path).is_err());
}

#[test]
fn foreign_databases_are_refused() {
    let directory = TempDir::new().unwrap();
    let controller = directory.path().join("state.redb");
    {
        let database = Database::create(&controller).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .open_table(TableDefinition::<u8, &[u8]>::new("controller_catalog_v1"))
            .unwrap();
        transaction.commit().unwrap();
    }
    let error = ObservationStore::open(&controller).err().unwrap();
    assert!(format!("{error:#}").contains("foreign table"), "{error:#}");

    let garbage = directory.path().join("garbage.redb");
    fs::write(&garbage, b"not a database").unwrap();
    assert!(ObservationStore::open(&garbage).is_err());
}

#[test]
fn collector_source_is_read_only_and_parameterized() {
    let source = collector_source("rpool/data").unwrap();
    assert!(source.contains("ZFS_ROOT = \"rpool/data\""));
    for forbidden in [
        "authorized_keys",
        "/var/lib/vz/snippets",
        "sshd",
        "read_text",
        "\"set\"",
        "\"create\"",
        "qm ",
        "pct ",
    ] {
        assert!(
            !source.contains(forbidden),
            "collector mentions {forbidden}"
        );
    }
    assert!(collector_source("rpool/data\"; import os").is_err());
    assert!(collector_source("rpool/data@snap").is_err());
    let args =
        discovery::ssh_arguments("root@pve1", Path::new("/k/known_hosts"), None, 22).unwrap();
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "UserKnownHostsFile=/k/known_hosts",
        "GlobalKnownHostsFile=/dev/null",
        "ProxyCommand=none",
    ] {
        assert!(args.iter().any(|a| a == option), "missing {option}");
    }
    assert_eq!(args.last().unwrap(), "python3 -I -");
    assert!(discovery::ssh_arguments("-oProxyCommand=x", Path::new("/k"), None, 22).is_err());
    assert!(discovery::ssh_arguments("root@pve1", Path::new("relative"), None, 22).is_err());
}

const FAKE: &str = r#"
import json, os, sys
name = os.path.basename(sys.argv[0])
args = sys.argv[1:]
state = os.environ["FAKE_STATE"]
with open(os.path.join(state, "calls.log"), "a") as log:
    log.write(json.dumps([name] + args) + "\n")
counter = os.path.join(state, "zfs-calls")
def zfs_pass():
    count = int(open(counter).read()) if os.path.exists(counter) else 0
    open(counter, "w").write(str(count + 1))
    return count
SECRET = "SECRET-SENTINEL-VALUE"
datasets = {
    "rpool/data": {"type": "filesystem", "guid": "1000", "quota": "0", "reservation": "0", "refreservation": "0"},
    "rpool/data/dev-erin": {"type": "filesystem", "guid": "1001", "quota": "100", "reservation": "100", "refreservation": "0"},
    "rpool/data/dev-erin/vm-101-disk-0": {"type": "volume", "guid": "2001", "quota": "-", "reservation": "0", "refreservation": "60", "volsize": "50"},
}
if name == "hostname":
    print("pve1")
elif name == "ssh-keygen":
    print("256 SHA256:abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ root@pve1 (ED25519)")
elif name == "pveversion":
    print("pve-manager/9.0.11/abcdef")
elif name == "zpool":
    print("rpool\t424242\tONLINE")
elif name == "zfs":
    assert args[0] == "get"
    kinds, props = args[args.index("-t") + 1], args[args.index("-o") + 2].split(",")
    if kinds == "snapshot":
        rows = {"rpool/data/dev-erin/vm-101-disk-0@pre": {"guid": "3001", "used": "0", "createtxg": "9"}}
    else:
        rows = json.loads(json.dumps(datasets))
        if os.environ.get("FAKE_CHANGE") and zfs_pass() >= 1:
            rows["rpool/data/dev-erin/vm-101-disk-0"]["guid"] = "2999"
    for dataset, values in rows.items():
        for prop in props:
            print("%s\t%s\t%s" % (dataset, prop, values.get(prop, "4096" if prop in ("used", "available") else "-")))
elif name == "pvesh":
    assert args[0] == "get" and args[2:4] == ["--output-format", "json"]
    path = args[1]
    responses = {
        "/nodes": [{"node": "pve1", "status": "online"}],
        "/nodes/pve1/qemu": [{"vmid": 101, "status": "running", "name": "dev-erin"}],
        "/nodes/pve1/qemu/101/config": {
            "digest": "abc101", "name": "dev-erin", "cores": 4, "memory": "2048",
            "scsi0": "dev-erin:vm-101-disk-0,size=50", "ide2": "none,media=cdrom",
            "net0": "virtio=BC:24:11:00:00:01,bridge=vmbr1,firewall=1",
            "sshkeys": "ssh-ed25519%20AAAAC3Nza" + SECRET, "cipassword": "**********",
            "cicustom": "user=local:snippets/" + SECRET + ".yaml", "description": SECRET,
            "args": "-fw_cfg " + SECRET, "smbios1": "uuid=" + SECRET,
        },
        "/nodes/pve1/qemu/101/pending": [{"key": "cores", "value": 4}, {"key": "sshkeys", "value": SECRET}],
        "/nodes/pve1/qemu/101/snapshot": [{"name": "current"}, {"name": "pre", "description": SECRET}],
        "/nodes/pve1/lxc": [],
        "/storage": [
            {"storage": "dev-erin", "type": "zfspool", "pool": "rpool/data/dev-erin", "content": "images", "sparse": 0, "digest": SECRET},
            {"storage": "local", "type": "dir", "path": "/var/lib/vz", "content": "iso,snippets"},
        ],
        "/pools": [{"poolid": "dev-erin"}],
        "/pools/dev-erin": {"comment": "Erin", "members": [{"id": "qemu/101", "vmid": 101, "type": "qemu"}]},
        "/nodes/pve1/network": [{"iface": "vmbr1", "type": "bridge", "cidr": "10.50.0.1/16", "active": 1, "comments": SECRET}],
    }
    if path == "/storage":
        counter = os.path.join(state, "storage-pass")
        current = int(open(counter).read()) if os.path.exists(counter) else 0
        open(counter, "w").write(str(current + 1))
        responses[path][1]["content"] = "snippets,iso" if current % 2 == 0 else "iso,snippets"
        if os.environ.get("FAKE_CHANGE") and current >= 1:
            responses[path][1]["content"] = "iso,backup"
    print(json.dumps(responses[path]))
else:
    sys.exit(97)
"#;

fn run_fake_collector(change: bool) -> (std::process::Output, PathBuf, TempDir) {
    let directory = TempDir::new().unwrap();
    let bin = directory.path().join("bin");
    let state = directory.path().join("state");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&state).unwrap();
    let fake = directory.path().join("fake.py");
    fs::write(&fake, FAKE).unwrap();
    for name in [
        "hostname",
        "ssh-keygen",
        "pveversion",
        "zpool",
        "zfs",
        "pvesh",
    ] {
        let script = bin.join(name);
        fs::write(&script, format!("#!/bin/sh\nexec python3 -I -c 'import sys; sys.argv[0]=\"{name}\"; exec(open(\"{}\").read())' \"$@\"\n", fake.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let machine = directory.path().join("machine-id");
    fs::write(&machine, "0123456789abcdef0123456789abcdef\n").unwrap();
    let source = collector_source("rpool/data")
        .unwrap()
        .replace(
            "\"/etc/machine-id\"",
            &format!("{:?}", machine.display().to_string()),
        )
        .replace(
            "\"/etc/ssh/ssh_host_ed25519_key.pub\"",
            "\"/fake/host_key.pub\"",
        );
    let collector = directory.path().join("collector.py");
    fs::write(&collector, source).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg(&collector)
        .env("PATH", path)
        .env("FAKE_STATE", &state);
    if change {
        command.env("FAKE_CHANGE", "1");
    }
    let output = command
        .output()
        .expect("python3 is required for collector tests");
    (output, state, directory)
}

#[test]
fn collector_emits_validatable_snapshot_without_secrets() {
    let (output, state, _directory) = run_fake_collector(false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(
        !text.contains("SECRET-SENTINEL"),
        "collector leaked a secret value"
    );
    assert!(
        !text.contains("snippets/"),
        "collector leaked a snippet reference"
    );
    let validated = validate_snapshot(&output.stdout, &pin()).unwrap();
    let vm = &validated.snapshot.vms["101"];
    assert!(validated.snapshot.consistency.config_stable);
    assert_eq!(
        validated.snapshot.storages["local"].content.as_deref(),
        Some("iso,snippets")
    );
    assert_eq!(
        vm.pending_keys,
        Vec::<String>::new(),
        "non-pending entries are not pending"
    );
    assert_eq!(vm.snapshots, ["pre"]);
    for key in [
        "args",
        "cicustom",
        "cipassword",
        "description",
        "smbios1",
        "sshkeys",
    ] {
        assert!(
            vm.omitted_keys.contains(&key.to_owned()),
            "{key} must be reported as omitted"
        );
        assert!(!vm.config.contains_key(key));
    }
    assert_eq!(validated.attachments[&101][0].kind, "media");
    assert_eq!(validated.attachments[&101][1].guid.as_deref(), Some("2001"));
    assert_eq!(validated.snapshot.host.machine_id_sha256.len(), 64);
    assert!(
        !text.contains("0123456789abcdef0123456789abcdef"),
        "raw machine-id leaked"
    );

    let calls: Vec<Vec<String>> = fs::read_to_string(state.join("calls.log"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!calls.is_empty());
    for call in &calls {
        let read_only = match call[0].as_str() {
            "pvesh" | "zfs" => call[1] == "get",
            "zpool" => call[1] == "list",
            "hostname" | "ssh-keygen" | "pveversion" => true,
            _ => false,
        };
        assert!(read_only, "non-read-only call {call:?}");
    }

    let mut mapping = propose_mapping(&validated);
    mapping.allocations[0].owner = Some("erin".into());
    mapping.vms[0].disposition = Some("owned".into());
    mapping.vms[0].owner = Some("erin".into());
    let observed = check_mapping(&validated, &mapping, &validated.digest).unwrap();
    assert_eq!(observed.allocations[0].volumes[0].snapshots[0].guid, "3001");
}

#[test]
fn collector_reports_instability_and_it_is_refused() {
    let (output, _state, _directory) = run_fake_collector(true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["consistency"]["config_stable"], json!(false));
    let error = validate_snapshot(&output.stdout, &pin()).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed during collection"),
        "{error:#}"
    );
}

fn cli(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pve-dev-inventory"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn cli_end_to_end_offline() {
    let directory = TempDir::new().unwrap();
    let p = |name: &str| directory.path().join(name).display().to_string();
    let output = cli(&["collector"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        collector_source("rpool/data").unwrap()
    );

    let output = cli(&[
        "ssh-command",
        "--destination",
        "root@pve1",
        "--known-hosts",
        "/k/known_hosts",
        "--collector",
        "/c.py",
        "--output",
        "/s.json",
    ]);
    assert!(output.status.success());
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed["executed"], json!(false));
    let command = printed["command"].as_str().unwrap();
    assert!(command.contains("StrictHostKeyChecking=yes") && command.contains("BatchMode=yes"));
    assert!(
        command.ends_with("'python3 -I -' < /c.py > /s.json"),
        "{command}"
    );

    fs::write(
        p("snapshot.json"),
        serde_json::to_vec_pretty(&snapshot()).unwrap(),
    )
    .unwrap();
    let pin_args = [
        "--snapshot",
        &p("snapshot.json"),
        "--node",
        "pve1",
        "--host-key",
        HOST_KEY,
    ];
    let output = cli(&[&["validate"][..], &pin_args].concat());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let digest = report["snapshot_digest"].as_str().unwrap().to_owned();

    let output = cli(&[
        &["validate"][..],
        &[
            "--snapshot",
            &p("snapshot.json"),
            "--node",
            "pve1",
            "--host-key",
            OTHER_KEY,
        ],
    ]
    .concat());
    assert!(!output.status.success());

    let output = cli(&[&["propose", "--output", &p("proposal.json")][..], &pin_args].concat());
    assert!(output.status.success());
    let output = cli(&[&["propose", "--output", &p("proposal.json")][..], &pin_args].concat());
    assert!(!output.status.success(), "propose must not overwrite");

    let db = p("observations.redb");
    let output = cli(&[
        &[
            "import",
            "--db",
            &db,
            "--mapping",
            &p("proposal.json"),
            "--confirm-snapshot",
            &digest,
        ][..],
        &pin_args,
    ]
    .concat());
    assert!(!output.status.success(), "unconfirmed proposal imported");
    assert!(String::from_utf8_lossy(&output.stderr).contains("no explicit owner"));

    let validated = crate::validated(&snapshot());
    fs::write(
        p("mapping.json"),
        serde_json::to_vec_pretty(&confirmed(&validated)).unwrap(),
    )
    .unwrap();
    let mapping_path = p("mapping.json");
    let import = [
        &[
            "import",
            "--db",
            &db,
            "--mapping",
            &mapping_path,
            "--confirm-snapshot",
            &digest,
        ][..],
        &pin_args,
    ]
    .concat();
    let output = cli(&import);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["outcome"], json!("Recorded"));
    assert_never_managed(&result);
    let output = cli(&import);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["outcome"], json!("Duplicate"));

    let output = cli(&["verify", "--db", &db]);
    assert!(output.status.success());
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["log_entries"], json!(2));

    let output = cli(&[
        "show",
        "--db",
        &db,
        "--host-id",
        &validated.host_id(),
        "--snapshot-digest",
        &digest,
    ]);
    assert!(output.status.success());
    assert_never_managed(&serde_json::from_slice(&output.stdout).unwrap());

    let output = cli(&["verify", "--db", &p("missing.redb")]);
    assert!(!output.status.success());
    assert!(
        !directory.path().join("missing.redb").exists(),
        "verify must not create a database"
    );
}
