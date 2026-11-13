use pve_dev::control::JobKind;
use pve_dev::control::{
    guest::{address, encode_key, image_metadata},
    protocol::Guest,
};
use serde_json::json;

#[test]
fn bootable_creation_durably_freezes_image_and_key_and_charges_the_whole_disk() {
    use pve_dev::control::{
        protocol::{OperationStatus, Request},
        store::{Binding, Store},
    };
    let directory = tempfile::tempdir().unwrap();
    let administrator = iroh::SecretKey::generate().public().to_string();
    let binding = Binding {
        environment: "a".repeat(32),
        backend: "simulated".into(),
        disk_capacity_mib: 8192,
        min_memory_capacity_mib: 2048,
        guest_images: vec!["a".repeat(64)],
        physical_trial: None,
        managed: None,
    };
    let store = Store::create(
        &directory.path().join("state.redb"),
        binding.clone(),
        &administrator,
    )
    .unwrap();
    let complete = |store: &Store| {
        for record in store.pending().unwrap() {
            store
                .progress(
                    &record.public.operation_id,
                    OperationStatus::Verified,
                    "observed",
                    None,
                )
                .unwrap();
        }
    };
    complete(&store);
    let owner: Request = serde_json::from_value(json!({"action":"create_user", "request_id":"owner", "user_id":"labdev", "budget_mib":5120, "min_memory_budget_mib":512, "max_memory_budget_mib":2048, "max_cores":4, "max_vms":2})).unwrap();
    store.handle(&administrator, owner).unwrap();
    complete(&store);
    let request = json!({"action":"create_vm", "request_id":"guest", "name":"dev-guest", "disk_mib":4096, "memory_mib":1024, "min_memory_mib":512, "cores":1, "owner":"labdev", "guest":{"image_sha256":"a".repeat(64), "ssh_public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"}});
    let mut unknown_image = request.clone();
    unknown_image["request_id"] = json!("unknown-image");
    unknown_image["guest"]["image_sha256"] = json!("b".repeat(64));
    assert_eq!(
        store
            .handle(
                &administrator,
                serde_json::from_value(unknown_image).unwrap()
            )
            .unwrap_err()
            .code,
        "invalid"
    );
    assert!(store.read().unwrap().vms.is_empty());
    assert_eq!(
        store
            .handle(&administrator, Request::ListImages {})
            .unwrap(),
        json!(["a".repeat(64)])
    );
    let accepted = store
        .handle(
            &administrator,
            serde_json::from_value(request.clone()).unwrap(),
        )
        .unwrap();
    drop(store);
    let store = Store::open(&directory.path().join("state.redb"), &binding).unwrap();
    assert_eq!(
        accepted,
        store
            .handle(
                &administrator,
                serde_json::from_value(request.clone()).unwrap()
            )
            .unwrap()
    );
    let frozen = &store.read().unwrap().operations[accepted["operation_id"].as_str().unwrap()];
    assert_eq!(frozen.job.kind, JobKind::CreateGuest);
    assert_eq!(
        frozen
            .job
            .vm
            .as_ref()
            .unwrap()
            .guest
            .as_ref()
            .unwrap()
            .image_sha256,
        "a".repeat(64)
    );
    let mut changed = request.clone();
    changed["guest"]["image_sha256"] = json!("b".repeat(64));
    assert_eq!(
        store
            .handle(&administrator, serde_json::from_value(changed).unwrap())
            .unwrap_err()
            .code,
        "conflict"
    );
    let mut extra = request;
    extra["request_id"] = json!("extra");
    extra["name"] = json!("extra-guest");
    assert_eq!(
        store
            .handle(&administrator, serde_json::from_value(extra).unwrap())
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(store.read().unwrap().vms.len(), 1);
}

#[test]
fn old_blank_vm_wire_encoding_is_unchanged_and_guest_fields_are_strict() {
    use pve_dev::control::protocol::Request;
    let mut value = json!({"action":"create_vm", "request_id":"blank", "name":"blank", "disk_mib":32, "memory_mib":128, "min_memory_mib":128, "cores":1, "owner":null});
    let request: Request = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), value);
    value["guest"] =
        json!({"image_sha256":"a".repeat(64), "ssh_public_key":"key", "image_path":"/dev/sda"});
    assert!(serde_json::from_value::<Request>(value).is_err());
}

#[test]
fn pinned_image_must_fit_and_have_no_external_storage() {
    let image = json!({"format":"qcow2", "virtual-size": 32 * 1024 * 1024});
    assert!(image_metadata(&image, 32).is_ok());
    assert!(image_metadata(&image, 31).is_err());
    for (key, value) in [
        ("format", json!("raw")),
        ("backing-filename", json!("/etc/shadow")),
        ("full-backing-filename", json!("/some/image")),
        ("data-file", json!("/dev/sda")),
        ("encrypted", json!(true)),
        ("virtual-size", json!(0)),
        ("virtual-size", json!(null)),
    ] {
        let mut invalid = image.clone();
        invalid[key] = value;
        assert!(image_metadata(&invalid, 32).is_err(), "{key}");
    }
    let mut corrupt = image.clone();
    corrupt["format-specific"] = json!({"data":{"corrupt":true}});
    assert!(image_metadata(&corrupt, 32).is_err());
}

#[test]
fn guest_addresses_are_deterministic_unique_and_lab_only() {
    let mut addresses = std::collections::BTreeSet::new();
    for vm_id in 9001..=9099 {
        assert!(addresses.insert(address(vm_id).unwrap()));
    }
    assert_eq!(address(9001).unwrap(), "10.77.0.11");
    for vm_id in [0, 101, 9000, 9100, u16::MAX] {
        assert!(address(vm_id).is_err());
    }
}

#[test]
fn guest_identity_accepts_only_pinned_hash_and_canonical_public_keys() {
    let mut guest = Guest {
        image_sha256: "a".repeat(64),
        ssh_public_key:
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGT3dRd3uA9zT/qLn2AsdPk+8AtrOaE1abxprqSEusfe"
                .into(),
    };
    guest.validate().unwrap();
    let public_key_file = format!("{} owner@example.com\n", guest.ssh_public_key);
    assert_eq!(
        pve_dev::control::guest::public_key_file(&public_key_file).unwrap(),
        guest.ssh_public_key
    );
    assert!(
        pve_dev::control::guest::public_key_file(&format!(
            "{}\n{}\n",
            guest.ssh_public_key, guest.ssh_public_key
        ))
        .is_err()
    );
    assert!(encode_key(&guest.ssh_public_key).starts_with("ssh-ed25519%20"));
    assert!(encode_key(&guest.ssh_public_key).contains("%2B"));
    guest.ssh_public_key.push_str(" extra-comment");
    assert!(guest.validate().is_err());
    guest.image_sha256 = "A".repeat(64);
    assert!(guest.validate().is_err());
}
