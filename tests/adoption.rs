use pve_dev::control::JobKind;
use pve_dev::control::{
    managed::ManagedConfig,
    protocol::{OperationStatus, Request, VmAction},
    store::{Binding, Store},
};
use serde_json::json;

fn config() -> ManagedConfig {
    serde_json::from_str(include_str!("fixtures/managed-adoption-config.json")).unwrap()
}

#[test]
fn adoption_pins_existing_allocations_and_refuses_ambiguous_targets() {
    let original = config();
    original.binding.validate(&original.environment).unwrap();
    for change in 0..9 {
        let mut c = original.clone();
        match change {
            0 => c.binding.adopted_vms[0].vm.owner = "ivan".into(),
            1 => c.binding.adopted_vms[0].dataset = c.binding.scope.control_dataset(),
            2 => c.binding.adopted_vms[0].configuration["scsi1"] = json!("dev-erin:vm-101-disk-1"),
            3 => c.binding.adopted_vms[0].configuration["memory"] = json!(4096),
            4 => {
                c.binding.adopted_vms[0]
                    .root_properties
                    .insert("guid".into(), "not-a-guid".into());
            }
            5 => {
                c.binding.adopted_vms[0].storage_configuration["pool"] =
                    json!("rpool/data/someone-else")
            }
            6 => c.binding.adopted_vms.push(c.binding.adopted_vms[0].clone()),
            7 => {
                c.binding.adopted_vms[0].configuration["cicustom"] =
                    json!("user=local:snippets/unreviewed.yaml")
            }
            _ => c.binding.adopted_vms[0].configuration["args"] = json!("-some-unsafe-extension"),
        }
        assert!(c.binding.validate(&c.environment).is_err(), "case {change}");
    }
}

#[test]
fn adopted_vm_uses_its_allocation_without_consuming_the_new_namespace_budget() {
    let temp = tempfile::tempdir().unwrap();
    let c = config();
    let mut original = c.binding.clone();
    original.adopted_vms.clear();
    let binding = Binding {
        environment: c.environment.clone(),
        backend: "managed".into(),
        disk_capacity_mib: original.scope.capacity_mib,
        min_memory_capacity_mib: original.scope.min_memory_capacity_mib,
        guest_images: original
            .images
            .iter()
            .map(|i| i.image_sha256.clone())
            .collect(),
        physical_trial: None,
        managed: Some(original),
    };
    let peer = iroh::SecretKey::generate().public().to_string();
    let store = Store::create(&temp.path().join("state.redb"), binding, &peer).unwrap();
    for op in store.pending().unwrap() {
        store
            .progress(
                &op.public.operation_id,
                OperationStatus::Verified,
                "complete",
                None,
            )
            .unwrap();
    }
    let previous = store.read().unwrap();
    store
        .change(|catalog| {
            catalog.binding.managed = Some(c.binding.clone());
            catalog.vms.insert(101, c.binding.adopted_vms[0].vm.clone());
            Ok(())
        })
        .unwrap();
    let dashboard = store.handle(&peer, Request::Dashboard {}).unwrap();
    assert_eq!(dashboard["vms"].as_array().unwrap().len(), 1);
    assert!(dashboard.get("imported_vms").is_none());
    assert_eq!(dashboard["vms"][0]["owner"], "erin");
    assert_eq!(dashboard["budget"]["allocated_mib"], 0);
    assert_eq!(
        dashboard["budget"]["budget_mib"],
        c.binding.admin_policy.budget_mib
    );
    assert_eq!(
        dashboard["storage_allocations"].as_array().unwrap().len(),
        2
    );
    assert_eq!(store.read().unwrap().requests, previous.requests);
    assert_eq!(store.read().unwrap().imported_vms, previous.imported_vms);
    let accepted = store
        .handle(
            &peer,
            Request::VmAction {
                request_id: "adopted-start".into(),
                vm_id: 101,
                kind: VmAction::Start,
            },
        )
        .unwrap();
    let job = store
        .pending()
        .unwrap()
        .into_iter()
        .find(|r| r.public.operation_id == accepted["operation_id"])
        .unwrap()
        .job;
    c.binding.validate_job(&job).unwrap();
    let mut wrong = job.clone();
    wrong.vm.as_mut().unwrap().disk_mib -= 1;
    assert!(c.binding.validate_job(&wrong).is_err());
    wrong = job.clone();
    wrong.kind = JobKind::CreateGuest;
    assert!(c.binding.validate_job(&wrong).is_err());
    wrong = job;
    wrong.owner.user_id = "ivan".into();
    assert!(c.binding.validate_job(&wrong).is_err());
}
