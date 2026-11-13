use iroh::{EndpointAddr, SecretKey};
use pve_dev::control::transport::{self, Descriptor, Network, Profile, ProfileBinding};
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

fn fixture() -> (tempfile::TempDir, Profile, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("device.key");
    transport::generate_identity(&key).unwrap();
    let profile = Profile {
        schema: 1,
        identity: key,
        server: Descriptor {
            schema: 1,
            endpoint: EndpointAddr::new(SecretKey::generate().public())
                .with_ip_addr("127.0.0.1:9".parse().unwrap()),
            environment: "0123456789abcdef0123456789abcdef".into(),
            network: Network::Loopback,
        },
    };
    let path = directory.path().join("profile.json");
    fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    (directory, profile, path)
}

fn invoke(path: &std::path::Path, binding: &ProfileBinding) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pve-devctl"))
        .arg("--profile")
        .arg(path)
        .args([
            "--expected-server-id",
            &binding.server_id,
            "--expected-environment",
            &binding.environment,
            "--expected-device-id",
            &binding.device_id,
            "dashboard",
        ])
        .output()
        .unwrap()
}

#[test]
fn local_profile_info_returns_only_public_identifiers_without_connecting() {
    let (_directory, profile, path) = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_pve-devctl"))
        .arg("--profile")
        .arg(&path)
        .arg("profile-info")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 3);
    assert_eq!(value["server_id"], profile.server.endpoint.id.to_string());
    assert_eq!(value["environment"], profile.server.environment);
    assert_eq!(
        value["device_id"],
        transport::load_identity(&profile.identity)
            .unwrap()
            .public()
            .to_string()
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("device.key"));
    assert!(output.stderr.is_empty());
}

#[test]
fn every_expected_identity_mismatch_fails_before_connecting() {
    let (_directory, profile, path) = fixture();
    let actual = transport::profile_binding(&profile).unwrap();
    for field in ["server", "environment", "device"] {
        let mut expected = actual.clone();
        match field {
            "server" => expected.server_id = SecretKey::generate().public().to_string(),
            "environment" => expected.environment = "ffffffffffffffffffffffffffffffff".into(),
            "device" => expected.device_id = SecretKey::generate().public().to_string(),
            _ => unreachable!(),
        }
        let start = Instant::now();
        let output = invoke(&path, &expected);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("profile identity changed; no request was sent")
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "unexpected transport timeout"
        );
    }
}

#[test]
fn editing_profile_or_replacing_device_key_cannot_retarget_saved_binding() {
    let (_directory, mut profile, path) = fixture();
    let expected = transport::profile_binding(&profile).unwrap();
    let original_endpoint = profile.server.endpoint.clone();
    profile.server.endpoint = EndpointAddr::new(SecretKey::generate().public());
    fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let output = invoke(&path, &expected);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no request was sent"));
    profile.server.endpoint = original_endpoint;
    fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    fs::remove_file(&profile.identity).unwrap();
    transport::generate_identity(&profile.identity).unwrap();
    let output = invoke(&path, &expected);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no request was sent"));
}

#[test]
fn inspection_preserves_schema_environment_and_private_file_guards() {
    let (_directory, mut profile, path) = fixture();
    for (schema, environment) in [(2, "0123456789abcdef0123456789abcdef"), (1, "wrong")] {
        profile.schema = schema;
        profile.server.environment = environment.into();
        assert!(transport::profile_binding(&profile).is_err());
    }
    profile.schema = 1;
    profile.server.environment = "0123456789abcdef0123456789abcdef".into();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&profile.identity, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    assert!(transport::profile_binding(&profile).is_err());
    let output = Command::new(env!("CARGO_BIN_EXE_pve-devctl"))
        .arg("--profile")
        .arg(&path)
        .args(["--expected-server-id", "partial", "dashboard"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--expected-device-id"));
}

#[test]
fn existing_profile_creation_expected_server_option_remains_compatible() {
    let (directory, profile, _) = fixture();
    let descriptor = directory.path().join("server.json");
    fs::write(&descriptor, serde_json::to_vec(&profile.server).unwrap()).unwrap();
    let destination = directory.path().join("generated.json");
    let output = Command::new(env!("CARGO_BIN_EXE_pve-devctl"))
        .arg("profile")
        .arg("--identity")
        .arg(&profile.identity)
        .arg("--server")
        .arg(descriptor)
        .arg("--expected-server")
        .arg(profile.server.endpoint.id.to_string())
        .arg("--output")
        .arg(destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
