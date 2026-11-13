//! Explicit, pinned, read-only SSH collection for the native inventory viewer.
//! It never opens an observation/controller DB and cannot submit mutations.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::{
    AlwaysFalse, HostPin, ValidatedSnapshot, collector_sha256, collector_source, host_id,
    sha256_hex, ssh_arguments, validate_snapshot,
};

pub const PROFILE_SCHEMA: &str = "pve-dev-observation-profile/1";
pub const RESULT_SCHEMA: &str = "pve-dev-observation-result/1";
pub const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 64 * 1024;
const MAX_KNOWN_HOSTS_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationProfile {
    pub schema: String,
    pub destination: String,
    pub port: u16,
    pub known_hosts: PathBuf,
    pub known_hosts_sha256: String,
    #[serde(default)]
    pub identity: Option<PathBuf>,
    pub node: String,
    pub host_key_sha256: String,
    pub machine_id_sha256: String,
    pub zpool_guid: String,
    pub zfs_root: String,
    #[serde(default)]
    pub vm_notes: BTreeMap<u32, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationIdentity {
    pub host_id: String,
    pub binding: String,
    pub node: String,
    pub host_key_sha256: String,
    pub machine_id_sha256: String,
    pub zpool_guid: String,
    pub zfs_root: String,
    pub known_hosts_sha256: String,
}

impl ObservationProfile {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute(),
            "observation profile path must be absolute"
        );
        ensure!(
            path.extension().is_some_and(|e| e == "json"),
            "observation profile must be a JSON file"
        );
        let bytes = read_bounded(path, MAX_PROFILE_BYTES)?;
        let profile: Self =
            serde_json::from_slice(&bytes).context("invalid read-only observation profile")?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == PROFILE_SCHEMA,
            "unsupported observation profile schema"
        );
        ensure!(self.port != 0, "invalid SSH port");
        ensure!(
            !self.node.is_empty()
                && self.node.len() <= 128
                && self
                    .node
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c)),
            "invalid pinned node"
        );
        ensure!(
            hex64(&self.machine_id_sha256) && hex64(&self.known_hosts_sha256),
            "invalid machine or known-hosts SHA256 pin"
        );
        ensure!(
            self.zpool_guid.parse::<u64>().is_ok_and(|v| v > 0),
            "invalid pinned pool GUID"
        );
        let key = self.host_key_sha256.strip_prefix("SHA256:").unwrap_or("");
        ensure!(
            key.len() == 43
                && key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"+/".contains(&c)),
            "invalid pinned host key fingerprint"
        );
        collector_source(&self.zfs_root)?;
        validate_path(&self.known_hosts)?;
        if let Some(path) = &self.identity {
            validate_path(path)?;
        }
        ssh_arguments(
            &self.destination,
            &self.known_hosts,
            self.identity.as_deref(),
            self.port,
        )?;
        ensure!(
            self.vm_notes.len() <= 1024
                && self.vm_notes.iter().all(|(id, note)| *id > 0
                    && note.len() <= 512
                    && !note.chars().any(char::is_control)),
            "invalid informational VM notes"
        );
        Ok(())
    }

    pub fn public_identity(&self) -> ObservationIdentity {
        // A public host binding, never an authorization or an ownership mapping.
        let binding = sha256_hex(
            serde_json::to_vec(&(
                "pve-dev-observation-binding/1",
                &self.node,
                &self.host_key_sha256,
                &self.machine_id_sha256,
                &self.zpool_guid,
                &self.zfs_root,
                &self.known_hosts_sha256,
            ))
            .expect("string tuple serialization")
            .as_slice(),
        );
        ObservationIdentity {
            host_id: host_id(&self.host_key_sha256),
            binding,
            node: self.node.clone(),
            host_key_sha256: self.host_key_sha256.clone(),
            machine_id_sha256: self.machine_id_sha256.clone(),
            zpool_guid: self.zpool_guid.clone(),
            zfs_root: self.zfs_root.clone(),
            known_hosts_sha256: self.known_hosts_sha256.clone(),
        }
    }

    pub fn checked_known_hosts(&self) -> Result<Vec<u8>> {
        let bytes = read_bounded(&self.known_hosts, MAX_KNOWN_HOSTS_BYTES)?;
        ensure!(
            sha256_hex(&bytes) == self.known_hosts_sha256,
            "known-hosts file changed; review the pinned observation profile"
        );
        let text = std::str::from_utf8(&bytes).context("known-hosts file must be UTF-8")?;
        let lines: Vec<_> = text
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .collect();
        ensure!(
            lines.len() == 1,
            "use a dedicated known-hosts file with one pinned Ed25519 key"
        );
        let fields: Vec<_> = lines[0].split_whitespace().collect();
        ensure!(
            fields.len() >= 3 && !fields[0].starts_with('@') && fields[1] == "ssh-ed25519",
            "known-hosts must contain one unmarked Ed25519 entry"
        );
        let key = STANDARD
            .decode(fields[2])
            .context("invalid Ed25519 known-hosts key")?;
        let fingerprint = format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(key)));
        ensure!(
            fingerprint == self.host_key_sha256,
            "known-hosts key does not match pinned host fingerprint"
        );
        Ok(bytes)
    }

    pub fn validate_capture(&self, bytes: &[u8]) -> Result<ValidatedSnapshot> {
        ensure!(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "collector output exceeded the observation limit"
        );
        let validated = validate_snapshot(
            bytes,
            &HostPin {
                node: self.node.clone(),
                host_key_sha256: self.host_key_sha256.clone(),
            },
        )?;
        ensure!(
            validated.snapshot.host.machine_id_sha256 == self.machine_id_sha256,
            "observed machine identity mismatch"
        );
        ensure!(
            validated.snapshot.zpool.guid == self.zpool_guid,
            "observed pool GUID mismatch"
        );
        ensure!(
            validated.snapshot.zfs_root == self.zfs_root,
            "observed ZFS root mismatch"
        );
        Ok(validated)
    }
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn validate_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path
                .to_str()
                .is_some_and(|v| !v.chars().any(|c| c.is_control() || c == '\0')),
        "SSH paths must be absolute UTF-8 paths without control characters"
    );
    Ok(())
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .context("cannot open observation input")?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "observation input exceeds its size limit"
    );
    Ok(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedVmSummary {
    pub id: String,
    pub vmid: u32,
    pub name: String,
    pub status: String,
    pub cores: Option<u64>,
    pub memory_mib: Option<u64>,
    pub balloon_mib: Option<u64>,
    pub disk_bytes: u64,
    pub note: Option<String>,
    pub pending_changes: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedDatasetSummary {
    pub name: String,
    pub guid: String,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub quota_bytes: Option<u64>,
    pub reservation_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationResult {
    pub schema: String,
    pub identity: ObservationIdentity,
    pub captured_at: String,
    pub received_at_unix_ms: u64,
    pub snapshot_digest: String,
    pub collector_sha256: String,
    pub pve_version: String,
    pub pool_name: String,
    pub pool_health: String,
    pub root_used_bytes: Option<u64>,
    pub root_available_bytes: Option<u64>,
    pub vms: Vec<ObservedVmSummary>,
    pub datasets: Vec<ObservedDatasetSummary>,
    pub container_count: usize,
    pub warnings: Vec<String>,
    pub managed: AlwaysFalse,
    pub authorizes_management: AlwaysFalse,
    pub creates_controller_state: AlwaysFalse,
}

pub fn summarize(
    profile: &ObservationProfile,
    validated: &ValidatedSnapshot,
) -> Result<ObservationResult> {
    let s = &validated.snapshot;
    ensure!(
        s.host.node == profile.node
            && s.host.host_key_sha256 == profile.host_key_sha256
            && s.host.machine_id_sha256 == profile.machine_id_sha256
            && s.zpool.guid == profile.zpool_guid
            && s.zfs_root == profile.zfs_root,
        "cannot summarize a snapshot under a different host binding"
    );
    let identity = profile.public_identity();
    let root = s
        .datasets
        .get(&s.zfs_root)
        .context("missing observed ZFS root")?;
    let number = |value: Option<&String>| {
        value.and_then(|v| v.strip_prefix("current=").unwrap_or(v).parse::<u64>().ok())
    };
    let mut vms = Vec::new();
    for (vmid, vm) in &s.vms {
        let vmid = vmid.parse::<u32>()?;
        let mut disk_bytes = 0_u64;
        for disk in &validated.attachments[&vmid] {
            if !disk.cdrom
                && let Some(dataset) = disk.dataset.as_ref().and_then(|name| s.datasets.get(name))
            {
                disk_bytes = disk_bytes
                    .checked_add(number(dataset.volsize.as_ref()).unwrap_or(0))
                    .context("observed VM disks overflow")?;
            }
        }
        vms.push(ObservedVmSummary {
            id: format!("{}/{vmid}", identity.host_id),
            vmid,
            name: vm
                .config
                .get("name")
                .cloned()
                .unwrap_or_else(|| format!("VM {vmid}")),
            status: vm.status.clone(),
            cores: number(vm.config.get("cores"))
                .and_then(|v| v.checked_mul(number(vm.config.get("sockets")).unwrap_or(1))),
            memory_mib: number(vm.config.get("memory")),
            balloon_mib: number(vm.config.get("balloon")),
            disk_bytes,
            note: profile.vm_notes.get(&vmid).cloned(),
            pending_changes: !vm.pending_keys.is_empty(),
        });
    }
    vms.sort_by_key(|vm| vm.vmid);
    let datasets = s
        .datasets
        .iter()
        .filter(|(_, ds)| ds.kind.as_deref() == Some("filesystem"))
        .map(|(name, ds)| ObservedDatasetSummary {
            name: name.clone(),
            guid: ds.guid.clone().unwrap_or_default(),
            used_bytes: number(ds.used.as_ref()),
            available_bytes: number(ds.available.as_ref()),
            quota_bytes: number(ds.quota.as_ref()),
            reservation_bytes: number(ds.reservation.as_ref()),
        })
        .collect();
    Ok(ObservationResult {
        schema: RESULT_SCHEMA.into(),
        identity,
        captured_at: s.captured_at.clone(),
        received_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?,
        snapshot_digest: validated.digest.clone(),
        collector_sha256: collector_sha256(&s.zfs_root)?,
        pve_version: s.host.pve_version.clone(),
        pool_name: s.zpool.name.clone(),
        pool_health: s.zpool.health.clone(),
        root_used_bytes: number(root.used.as_ref()),
        root_available_bytes: number(root.available.as_ref()),
        vms,
        datasets,
        container_count: s.containers.len(),
        warnings: validated.warnings.clone(),
        managed: AlwaysFalse,
        authorizes_management: AlwaysFalse,
        creates_controller_state: AlwaysFalse,
    })
}

struct KnownHostsCopy {
    directory: PathBuf,
    path: PathBuf,
}
impl KnownHostsCopy {
    fn new(bytes: &[u8]) -> Result<Self> {
        use std::io::Write;
        let mut nonce = [0_u8; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let token: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("pve-observe-{token}"));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let result = Self {
            path: directory.join("known_hosts"),
            directory,
        };
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&result.path)?
            .write_all(bytes)?;
        Ok(result)
    }
}
impl Drop for KnownHostsCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

async fn bounded_output(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(bytes);
        }
        ensure!(
            bytes.len() + count <= limit,
            "SSH output exceeded the observation limit"
        );
        bytes.extend_from_slice(&chunk[..count]);
    }
}

/// Run one fixed collector. Cancellation/timeout kills and reaps SSH; no shell,
/// forwarding, config hooks, password prompt, or general remote command exists.
pub async fn collect(
    profile: &ObservationProfile,
    expected_binding: &str,
    timeout_seconds: u64,
) -> Result<ObservationResult> {
    profile.validate()?;
    ensure!(
        profile.public_identity().binding == expected_binding,
        "observation profile identity changed; review and trust it again"
    );
    ensure!(
        (5..=600).contains(&timeout_seconds),
        "observation timeout must be 5–600 seconds"
    );
    let copy = KnownHostsCopy::new(&profile.checked_known_hosts()?)?;
    let mut arguments = ssh_arguments(
        &profile.destination,
        &copy.path,
        profile.identity.as_deref(),
        profile.port,
    )?;
    // OpenSSH parses -o values itself; preserve a single file even if TMPDIR
    // contains spaces or quotes. This is configuration quoting, never a shell.
    for argument in &mut arguments {
        if argument.starts_with("UserKnownHostsFile=") {
            let path = copy
                .path
                .to_str()
                .context("temporary known-hosts path must be UTF-8")?;
            ensure!(
                !path.chars().any(char::is_control),
                "temporary known-hosts path contains control characters"
            );
            *argument = format!(
                "UserKnownHostsFile=\"{}\"",
                path.replace('\\', "\\\\").replace('"', "\\\"")
            );
        }
    }
    // Force key-based auth only; local profile routes cannot inject commands.
    arguments.splice(
        1..1,
        [
            "-o".into(),
            "PasswordAuthentication=no".into(),
            "-o".into(),
            "KbdInteractiveAuthentication=no".into(),
        ],
    );
    let source = collector_source(&profile.zfs_root)?;
    let bytes = run_collector_process(
        "/usr/bin/ssh",
        &arguments[1..],
        &source,
        std::time::Duration::from_secs(timeout_seconds),
    )
    .await?;
    profile
        .validate_capture(&bytes)
        .and_then(|validated| summarize(profile, &validated))
}

// Private executable injection is only for local process-boundary unit tests.
// The public collection API always passes the fixed /usr/bin/ssh executable.
async fn run_collector_process(
    executable: &str,
    arguments: &[String],
    source: &str,
    timeout: std::time::Duration,
) -> Result<Vec<u8>> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut child = tokio::process::Command::new(executable)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting read-only SSH collector")?;
    let mut input = child.stdin.take().context("SSH stdin unavailable")?;
    let output = child.stdout.take().context("SSH stdout unavailable")?;
    let errors = child.stderr.take().context("SSH stderr unavailable")?;
    let work = async {
        let ((), bytes, _errors) = tokio::try_join!(
            async {
                input.write_all(source.as_bytes()).await?;
                input.shutdown().await?;
                // ChildStdin::shutdown is not an ownership/EOF guarantee.
                // Python reads its whole program before executing it.
                drop(input);
                anyhow::Ok(())
            },
            bounded_output(output, MAX_SNAPSHOT_BYTES),
            bounded_output(errors, 64 * 1024),
        )?;
        let status = child.wait().await?;
        ensure!(
            status.success(),
            "read-only SSH collection failed (exit {}); check pinned host, key access and collector compatibility",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        );
        Ok(bytes)
    };
    let result = tokio::select! {
        result = work => result,
        _ = tokio::time::sleep(timeout) => Err(anyhow::anyhow!("read-only observation timed out")),
        _ = terminate.recv() => Err(anyhow::anyhow!("read-only observation cancelled")),
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("read-only observation cancelled")),
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[tokio::test]
    async fn collector_receives_eof_before_output_is_awaited() {
        let bytes = run_collector_process(
            "/bin/cat",
            &[],
            "collector source\n",
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(bytes, b"collector source\n");
    }

    #[tokio::test]
    async fn collector_timeout_kills_and_reaps_its_child() {
        let directory = tempfile::TempDir::new().unwrap();
        let marker = directory.path().join("pid");
        let arguments = vec![
            "-c".into(),
            format!("echo $$ > '{}'; exec sleep 30", marker.display()),
        ];
        let error = run_collector_process(
            "/bin/sh",
            &arguments,
            "",
            std::time::Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let pid = fs::read_to_string(marker).unwrap();
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out collector child remains alive");
    }

    #[tokio::test]
    async fn collector_stdout_and_stderr_are_bounded_and_fail_closed() {
        for redirect in ["", " >&2"] {
            let script = format!(
                "while :; do printf '0123456789012345678901234567890123456789012345678901234567890123'{redirect}; done"
            );
            let error = run_collector_process(
                "/bin/sh",
                &["-c".into(), script],
                "",
                std::time::Duration::from_secs(10),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("output exceeded"));
        }
    }

    #[tokio::test]
    async fn failed_collector_output_is_not_accepted() {
        let error = run_collector_process(
            "/bin/sh",
            &["-c".into(), "printf '{}'; exit 7".into()],
            "",
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exit 7"));
    }
}
