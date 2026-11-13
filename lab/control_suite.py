import json
import hashlib
import pathlib
import re
import secrets
import shutil
import subprocess
import time


POLICY_FIELDS = ("budget_mib", "min_memory_budget_mib", "max_memory_budget_mib", "max_cores", "max_vms")


def policy_request(user, request_id, **changes):
    policy = {key: user[key] for key in POLICY_FIELDS}
    policy.update(changes)
    if set(policy) != set(POLICY_FIELDS):
        raise ValueError("unknown policy field")
    return {"action": "update_user_policy", "request_id": request_id, "user_id": user["user_id"],
            "expected_revision": user.get("policy_revision", 0), "policy": policy}


def verify_quota_transition(before, after, budget_mib):
    dataset = before["dataset"]
    if after["dataset"] != dataset or set(before["zfs"]) != set(after["zfs"]):
        raise RuntimeError("Quota edit changed the exact dataset/zvol inventory")
    for key in ("guid", "createtxg", "org.pve-dev:owner"):
        if not before["zfs"][dataset][key] or before["zfs"][dataset][key] != after["zfs"][dataset][key]:
            raise RuntimeError(f"Quota edit changed owner {key}")
    properties = after["zfs"][dataset]
    if any(int(properties[key]) != budget_mib * 1024**2 for key in ("quota", "reservation")):
        raise RuntimeError("Actual quota/reservation do not match the verified policy")
    if int(properties["used"]) > budget_mib * 1024**2:
        raise RuntimeError("Actual dataset usage exceeds the target policy")
    for name in before["zfs"]:
        if name != dataset:
            stable = lambda value: {key: item for key, item in value.items() if key != "used"}
            if stable(before["zfs"][name]) != stable(after["zfs"][name]):
                raise RuntimeError(f"Quota edit changed managed volume identity/backing: {name}")
            if int(after["zfs"][name]["refreservation"]) < int(after["zfs"][name]["volsize"]):
                raise RuntimeError(f"Quota edit left thin backing: {name}")
    if before["configs"] != after["configs"] or before["storage"] != after["storage"]:
        raise RuntimeError("Quota edit changed configured VMs or storage binding")


def quota_snapshot(remote, vm_ids):
    if not vm_ids or any(not isinstance(vmid, int) or not 9001 <= vmid <= 9099 for vmid in vm_ids):
        raise ValueError("quota evidence accepts only disposable VM IDs")
    script = '''import json, subprocess, sys
dataset = "labpool/control/frank"
def call(*arguments):
    return subprocess.check_output(arguments, text=True)
properties = "guid,createtxg,quota,reservation,refreservation,volsize,used,org.pve-dev:owner"
zfs = {}
for line in call("zfs", "get", "-Hpr", "-o", "name,property,value", properties, dataset).splitlines():
    name, key, value = line.split("\\t")
    zfs.setdefault(name, {})[key] = value
configs = {vmid: json.loads(call("pvesh", "get", f"/nodes/pve-lab/qemu/{vmid}/config", "--output-format", "json")) for vmid in sys.argv[1:]}
storage = json.loads(call("pvesh", "get", "/storage/ctl-frank", "--output-format", "json"))
print(json.dumps({"dataset": dataset, "zfs": zfs, "configs": configs, "storage": storage}))
'''
    return json.loads(remote("python3 - " + " ".join(map(str, vm_ids)), input=script, text=True, timeout=180).stdout)


def quota_fault_install(nonce):
    if not re.fullmatch(r"[0-9a-f]{32}", nonce):
        raise ValueError("invalid disposable lab nonce")
    # This wrapper exists only in the fresh disposable root overlay. It runs
    # real ZFS, then stops after one exact successful effect, before helper ack.
    wrapper = f'''#!/usr/bin/python3
import os, pathlib, signal, subprocess, sys
if pathlib.Path("/etc/pve-dev-lab-id").read_text().strip() != {nonce!r}:
    raise RuntimeError("wrong disposable lab")
result = subprocess.run(["/usr/sbin/zfs.pve-dev-quota-real", *sys.argv[1:]])
if result.returncode == 0 and sys.argv[1:] == ["set", "quota=134217728", "labpool/control/frank"]:
    pathlib.Path("/run/pve-dev-quota-effect").write_text({nonce!r})
    os.kill(os.getpid(), signal.SIGSTOP)
sys.exit(result.returncode)
'''
    return f'''import os, pathlib, socket
if pathlib.Path("/etc/pve-dev-lab-id").read_text().strip() != {nonce!r} or socket.gethostname() != "pve-lab":
    raise RuntimeError("wrong disposable lab")
original = pathlib.Path("/usr/sbin/zfs")
saved = pathlib.Path("/usr/sbin/zfs.pve-dev-quota-real")
if not original.is_file() or original.is_symlink() or saved.exists():
    raise RuntimeError("unexpected ZFS executable layout")
os.rename(original, saved)
original.write_text({wrapper!r})
original.chmod(0o755)
'''


def quota_checks(lab, evidence, invoke, operation, remote, check, nonce, vm_ids, timeout):
    def user():
        return next(item for item in invoke("erin", {"action": "dashboard"})["users"] if item["user_id"] == "frank")

    snapshots = {"before": quota_snapshot(remote, vm_ids)}
    requests = {}
    refusals = {}
    initial = user()
    growth = policy_request(initial, "quota-grow", budget_mib=256, min_memory_budget_mib=384,
                            max_memory_budget_mib=1280, max_cores=10, max_vms=6)
    invoke("frank", growth, "forbidden")
    remote("systemctl kill --signal=STOP pve-dev-host.service", text=True, timeout=45)
    try:
        requests["growth"] = invoke("erin", growth)
        if invoke("erin", growth)["operation_id"] != requests["growth"]["operation_id"]:
            raise RuntimeError("Pending quota retry changed operation identity")
        frozen = user()
        if frozen["ready"] or frozen["budget_mib"] != 128 or frozen.get("policy_revision", 0) != 0:
            raise RuntimeError("Accepted quota intent did not preserve/freeze the old policy")
        invoke("frank", {"action": "vm_action", "request_id": "quota-frozen-start", "vm_id": vm_ids[0], "kind": "start"}, "conflict")
        invoke("frank", {"action": "create_vm", "request_id": "quota-frozen-create", "name": "quota-frozen", "disk_mib": 32,
                         "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None}, "conflict")
    finally:
        remote("systemctl kill --signal=CONT pve-dev-host.service", text=True, timeout=45)
    operation("erin", requests["growth"])
    grown = user()
    if not grown["ready"] or grown.get("policy_revision") != 1 or any(grown[key] != growth["policy"][key] for key in POLICY_FIELDS):
        raise RuntimeError("Verified growth did not commit the exact full resource policy")
    snapshots["grown"] = quota_snapshot(remote, vm_ids)
    verify_quota_transition(snapshots["before"], snapshots["grown"], 256)
    check("real_quota_growth_full_policy_and_pending_owner_freeze")

    before_refusals = len(invoke("erin", {"action": "dashboard"})["operations"])
    refusals["below_stopped_commitments"] = invoke("erin", policy_request(grown, "quota-too-small", budget_mib=64), "conflict")
    refusals["zero"] = invoke("erin", policy_request(grown, "quota-zero", budget_mib=0), "invalid")
    refusals["stale_revision"] = invoke("erin", policy_request(initial, "quota-stale", budget_mib=96), "conflict")
    changed_retry = policy_request(initial, "quota-grow", budget_mib=192)
    refusals["changed_retry_payload"] = invoke("erin", changed_retry, "conflict")
    if len(invoke("erin", {"action": "dashboard"})["operations"]) != before_refusals or user() != grown:
        raise RuntimeError("Refused quota requests changed policy or queued operations")
    snapshots["after_refusals"] = quota_snapshot(remote, vm_ids)
    verify_quota_transition(snapshots["grown"], snapshots["after_refusals"], 256)
    check("real_quota_below_commitment_zero_stale_and_payload_reuse_refused")

    contraction = policy_request(grown, "quota-contract", budget_mib=96, min_memory_budget_mib=256,
                                 max_memory_budget_mib=1024, max_cores=8, max_vms=4)
    requests["contraction"] = invoke("erin", contraction)
    operation("erin", requests["contraction"])
    snapshots["contracted"] = quota_snapshot(remote, vm_ids)
    verify_quota_transition(snapshots["before"], snapshots["contracted"], 96)
    contracted = user()
    if not contracted["ready"] or contracted.get("policy_revision") != 2:
        raise RuntimeError("Safe contraction did not advance the policy revision")
    nonstorage = policy_request(contracted, "quota-nonstorage", max_cores=7)
    requests["nonstorage"] = invoke("erin", nonstorage)
    operation("erin", requests["nonstorage"])
    snapshots["nonstorage"] = quota_snapshot(remote, vm_ids)
    verify_quota_transition(snapshots["contracted"], snapshots["nonstorage"], 96)
    check("real_unused_quota_contraction_and_nonstorage_edit_preserve_configurations")

    restore = policy_request(user(), "quota-after-effect-recovery", budget_mib=128)
    old_pid = remote("systemctl show -p MainPID --value pve-dev-host.service", text=True, timeout=45).stdout.strip()
    boot = remote("cat /proc/sys/kernel/random/boot_id", text=True, timeout=45).stdout.strip()
    remote("python3 -", input=quota_fault_install(nonce), text=True, timeout=45)
    try:
        requests["interrupted_growth"] = invoke("erin", restore)
        marker = f'''import pathlib, time
deadline = time.monotonic() + {timeout}
while time.monotonic() < deadline:
    path = pathlib.Path("/run/pve-dev-quota-effect")
    if path.exists() and path.read_text() == {nonce!r}:
        print("real quota effect completed; helper acknowledgement withheld")
        break
    time.sleep(1)
else:
    raise RuntimeError("quota effect gate was not reached")
'''
        remote("python3 -", input=marker, text=True, timeout=timeout + 30)
        snapshots["after_effect_before_ack"] = quota_snapshot(remote, vm_ids)
        properties = snapshots["after_effect_before_ack"]["zfs"]["labpool/control/frank"]
        if int(properties["quota"]) != 128 * 1024**2 or int(properties["reservation"]) != 96 * 1024**2:
            raise RuntimeError("Quota recovery fixture did not stop between the real property effects")
        if user()["ready"] or user()["budget_mib"] != 96:
            raise RuntimeError("Partial physical effect prematurely released the old catalog policy")
        remote("systemctl kill --signal=KILL --kill-whom=all pve-dev-host.service", text=True, timeout=45)
    finally:
        remote("test -f /usr/sbin/zfs.pve-dev-quota-real && mv -f /usr/sbin/zfs.pve-dev-quota-real /usr/sbin/zfs", text=True, timeout=45)
        remote("systemctl restart pve-dev-host.service", text=True, timeout=60)
    operation("erin", requests["interrupted_growth"])
    new_pid = remote("systemctl show -p MainPID --value pve-dev-host.service", text=True, timeout=45).stdout.strip()
    if not new_pid or new_pid == old_pid or remote("cat /proc/sys/kernel/random/boot_id", text=True, timeout=45).stdout.strip() != boot:
        raise RuntimeError("Quota recovery did not establish a fresh helper in the same host boot")
    if invoke("erin", restore)["operation_id"] != requests["interrupted_growth"]["operation_id"]:
        raise RuntimeError("Recovered quota retry changed operation identity")
    if invoke("erin", growth)["operation_id"] != requests["growth"]["operation_id"]:
        raise RuntimeError("Historical quota retry changed operation identity")
    final = user()
    if not final["ready"] or final["budget_mib"] != 128 or final.get("policy_revision") != 4:
        raise RuntimeError("Quota recovery/historical retry changed the final policy")
    snapshots["recovered"] = quota_snapshot(remote, vm_ids)
    verify_quota_transition(snapshots["before"], snapshots["recovered"], 128)
    lab.atomic_json(evidence / "quota-workflow.json", {"requests": requests, "refusals": refusals, "snapshots": snapshots,
                                                     "old_helper_pid": old_pid, "new_helper_pid": new_pid,
                                                     "same_boot_verified": True, "final_user": final})
    check("real_quota_effect_before_ack_helper_sigkill_recovery_and_historical_retry")


def controller_capacity_arguments(bootable):
    return ["--disk-capacity-mib", "8192" if bootable else "2048",
            "--min-memory-capacity-mib", "1536" if bootable else "1024"]


def verify_quota_refusal(result, quota, pool_available, volume_names):
    capacity_error = any(message in result.stderr.lower() for message in ("out of space", "quota exceeded", "size is greater than available space"))
    if result.returncode == 0 or not capacity_error:
        raise RuntimeError(f"Expected a ZFS capacity refusal, received {result.returncode}: {result.stderr}")
    if int(quota) != 128 * 1024 * 1024 or int(pool_available) < 256 * 1024 * 1024:
        raise RuntimeError("Quota probe did not establish an owner limit with sufficient global pool capacity")
    if "labpool/control/frank/over-budget" in volume_names:
        raise RuntimeError("Failed quota probe left an unexpected volume")


def run(lab, timeout, linux_bin_dir, bootable=False, shrink=False, shrink_prepare_only=False):
    if shrink and not bootable:
        raise ValueError("Shrink qualification requires a bootable guest")
    if shrink_prepare_only and not shrink:
        raise ValueError("Preparation-only qualification requires shrink mode")
    lab.build(timeout)
    lab.space()
    started = time.monotonic()
    root = lab.ROOT
    linux_bin_dir = linux_bin_dir or root / "target/x86_64-unknown-linux-musl/release"
    for name in ("pve-devd", "pve-dev-host"):
        if not (linux_bin_dir / name).is_file():
            raise RuntimeError(f"Missing Linux binary {linux_bin_dir / name}; run scripts/build-lab-binaries.sh")
    prepared_client = linux_bin_dir / "pve-devctl-native"
    if not prepared_client.is_file():
        lab.run(["cargo", "build", "--locked", "--bins"], cwd=root)
        prepared_client = root / "target/debug/pve-devctl"
    nonce = secrets.token_hex(16)
    directory = lab.HOME / ("run-controller-" + time.strftime("%Y%m%dT%H%M%S") + "-" + nonce[:6])
    directory.mkdir(mode=0o700)
    binaries = directory / "bin"
    binaries.mkdir(mode=0o700)
    client = binaries / "pve-devctl"
    shutil.copy2(prepared_client, client)
    for name in ("pve-devd", "pve-dev-host"):
        shutil.copy2(linux_bin_dir / name, binaries / name)
    evidence = directory / "evidence"
    evidence.mkdir()
    clients = directory / "clients"
    clients.mkdir(mode=0o700)
    base = lab.HOME / "base"
    base_info = json.loads((base / "build.json").read_text())
    lab.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(base / "root.qcow2"), str(directory / "root.qcow2")])
    shutil.copyfile(base / "seed.iso", directory / "seed.iso")
    for letter in ("a", "b"):
        lab.run(["qemu-img", "create", "-f", "qcow2", str(directory / f"data-{letter}.qcow2"), "20G" if shrink else ("8G" if bootable else "4G")])
    identities = {}
    profiles = {}
    for name in ("erin", "frank", "heidi", "unknown"):
        output = lab.run([str(client), "identity", "new", "--path", str(clients / f"{name}.key")], capture_output=True, text=True)
        identities[name] = json.loads(output.stdout)["endpoint_id"]
    summary = {"schema": 1, "backend": "real-proxmox-iroh-controller", "environment": nonce,
               "production_contacted": False, "disk_shrink_qualification": shrink, "nested_guest_boot_verified": False, "complete": False, "checks": []}
    if shrink_prepare_only:summary["qualification_scope"]="preparation_only_without_guest_boot_or_recovery_deletion"
    summary["emulation"] = {"accelerator": "tcg", "threading": "multi", "outer_vcpus": 4}
    summary["artifacts_sha256"] = {}
    for binary in binaries.iterdir():
        with binary.open("rb") as source:
            summary["artifacts_sha256"][binary.name] = hashlib.file_digest(source, "sha256").hexdigest()
    if (linux_bin_dir / "build-manifest.json").is_file():
        manifest = json.loads((linux_bin_dir / "build-manifest.json").read_text())
        for name, actual in summary["artifacts_sha256"].items():
            if manifest["binaries"].get("pve-devctl-native" if name == "pve-devctl" else name) != actual:
                raise RuntimeError("Frozen artifact does not match its build manifest")
        lab.atomic_json(evidence / "build-manifest.json", manifest)

    def remote(command, **options):
        output = lab.ssh(directory, command, capture_output=True, **options)
        if output.returncode:
            raise RuntimeError(f"Disposable lab command failed: {command}\n{output.stderr!r}")
        return output

    def invoke(name, request, expected=None):
        output = subprocess.run([str(client), "--profile", str(profiles[name]), "rpc"], input=json.dumps(request), capture_output=True, text=True, timeout=45)
        if not output.stdout:
            raise RuntimeError(f"iroh client failed: {output.stderr}")
        response = json.loads(output.stdout)
        if expected:
            if response.get("ok") or response.get("error", {}).get("code") != expected:
                raise RuntimeError(f"Expected {expected}, received {response}")
            return response
        if not response.get("ok"):
            raise RuntimeError(f"Request rejected: {response}")
        return response["result"]

    def operation(name, result, wanted="verified"):
        deadline = time.monotonic() + timeout
        previous = None
        while time.monotonic() < deadline:
            current = invoke(name, {"action": "get_operation", "operation_id": result["operation_id"]})
            state = (current["status"], current["step"])
            if state != previous:
                print(f"Controller {name} {result['operation_id'][:10]}: {state}", flush=True)
                previous = state
            if current["status"] == "blocked":
                raise RuntimeError(f"Controller operation blocked: {current}")
            if current["status"] == wanted:
                return current
            time.sleep(3)
        raise TimeoutError(f"Controller operation did not reach {wanted}: {result}")

    def check(name):
        summary["checks"].append(name)
        lab.atomic_json(evidence / "summary.json", summary)
        print(f"Controller check passed: {name}", flush=True)

    def create(name, request_id, vm_name):
        return invoke(name, {"action": "create_vm", "request_id": request_id, "name": vm_name,
                             "disk_mib": 32, "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None})

    with lab.boot(directory, nonce, data_disks=True, control_port=True) as process:
        try:
            summary["proxmox"] = lab.ready(directory, process, timeout)
            lab.disable_host_cloud_init(directory)
            remote(f'test "$(cat /etc/pve-dev-lab-id)" = {base_info["nonce"]} && printf "%s\\n" {nonce} >/etc/pve-dev-lab-id', text=True, timeout=45)
            setup = remote(f"python3 - {nonce}", input=(root / "lab/setup.py").read_text(), text=True, timeout=180)
            (evidence / "setup.log").write_text(setup.stdout + setup.stderr)
            pool_guid = remote("zpool get -Hp -o value guid labpool", text=True, timeout=45).stdout.strip()
            configuration = json.dumps({"environment": nonce, "pool_guid": pool_guid})
            if bootable:
                import guest_suite
                image = lab.download()
                with image.open("rb") as source:
                    image_sha256 = hashlib.file_digest(source, "sha256").hexdigest()
                image_path = f"/var/lib/pve-dev-images/{image_sha256}.qcow2"
                remote("install -d -m 0755 -o root -g root /var/lib/pve-dev-images", text=True, timeout=45)
                with image.open("rb") as source:
                    remote(f"cat >{image_path} && chmod 0444 {image_path}", stdin=source, timeout=600)
                prepared = remote(f"python3 - {nonce} {pool_guid} {image_sha256}",
                                  input=(root / "lab/prepare_guest_image.py").read_text(),
                                  text=True, timeout=900)
                manifest = json.loads(prepared.stdout)
                if (manifest["schema"] != "pve-dev-lab-image/1"
                        or manifest["recipe"] != "pve-dev-lab-boot-policy/2"
                        or manifest["timeout_seconds"] != 600
                        or manifest["default_user"] != "dev"
                        or manifest["environment"] != nonce or manifest["parent_sha256"] != image_sha256
                        or manifest["parent_image_unchanged"] is not True
                        or not isinstance(manifest["image_sha256"], str)
                        or not re.fullmatch(r"[0-9a-f]{64}", manifest["image_sha256"])):
                    raise RuntimeError("prepared image manifest does not match the trusted parent")
                manifest["parent_sha512"] = lab.INPUTS["debian_sha512"]
                lab.atomic_json(evidence / "guest-image-build.json", manifest)
                image_sha256 = manifest["image_sha256"]
                image_path = f"/var/lib/pve-dev-images/{image_sha256}.qcow2"
                if manifest["path"] != image_path:
                    raise RuntimeError("prepared image did not use a content-addressed path")
                remote("ip link add vmbr77 type bridge && ip address add 10.77.0.1/24 dev vmbr77 && ip link set vmbr77 up", text=True, timeout=45)
                remote("printf '536870912\\n' >/sys/module/zfs/parameters/zfs_arc_max", text=True, timeout=45)
                configuration = json.dumps({"environment": nonce, "pool_guid": pool_guid,
                                            "images": [{"path": image_path, "sha256": image_sha256}]})
            remote("cat >/etc/pve-dev-controller-lab.json && chmod 600 /etc/pve-dev-controller-lab.json", input=configuration, text=True, timeout=45)
            for name, destination in (("pve-devd", "/usr/local/bin/pve-devd"), ("pve-dev-host", "/usr/local/sbin/pve-dev-host")):
                remote(f"cat >{destination} && chmod 755 {destination}", input=(binaries / name).read_bytes(), timeout=180)
            remote("set -e; useradd --system --home-dir /var/lib/pve-dev --shell /usr/sbin/nologin pve-dev; install -d -m 0750 -o root -g pve-dev /run/pve-dev-host; install -d -m 0700 -o pve-dev -g pve-dev /var/lib/pve-dev", text=True, timeout=45)
            helper_unit = """[Unit]
Description=Disposable pve-dev privileged host helper
After=pve-cluster.service
[Service]
Type=simple
User=root
Group=pve-dev
UMask=0077
ExecStart=/usr/local/sbin/pve-dev-host serve --state-dir /var/lib/pve-dev-host --socket /run/pve-dev-host/control.sock --lab-config /etc/pve-dev-controller-lab.json
Restart=on-failure
RestartSec=2
KillMode=control-group
TimeoutStopSec=30
[Install]
WantedBy=multi-user.target
"""
            daemon_unit = """[Unit]
Description=Disposable pve-dev unprivileged iroh controller
After=pve-dev-host.service
[Service]
Type=simple
User=pve-dev
Group=pve-dev
UMask=0077
ExecStart=/usr/local/bin/pve-devd serve --data-dir /var/lib/pve-dev/controller
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/pve-dev
CapabilityBoundingSet=
RestrictSUIDSGID=true
KillMode=control-group
[Install]
WantedBy=multi-user.target
"""
            remote("cat >/etc/systemd/system/pve-dev-host.service", input=helper_unit, text=True, timeout=45)
            remote("cat >/etc/systemd/system/pve-dev-controller.service", input=daemon_unit, text=True, timeout=45)
            remote("systemctl daemon-reload && systemctl start pve-dev-host.service", text=True, timeout=45)
            image_option = f" --guest-image-sha256 {image_sha256}" if bootable else ""
            capacity_options = "--disk-capacity-mib 16384 --min-memory-capacity-mib 1536" if shrink else " ".join(controller_capacity_arguments(bootable))
            remote(f"runuser -u pve-dev -- /usr/local/bin/pve-devd init --data-dir /var/lib/pve-dev/controller --admin {identities['erin']} --environment {nonce} --host-socket /run/pve-dev-host/control.sock --backend lab --network direct --port 7777 {capacity_options}{image_option}", text=True, timeout=90)
            remote("systemctl start pve-dev-controller.service", text=True, timeout=45)
            descriptor = None
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                output = lab.ssh(directory, "cat /var/lib/pve-dev/controller/server.json", capture_output=True, text=True, timeout=45)
                if output.returncode == 0:
                    descriptor = json.loads(output.stdout)
                    break
                time.sleep(3)
            if descriptor is None:
                raise RuntimeError("iroh controller did not start; inspect systemd diagnostics")
            (directory / "server.json").write_text(json.dumps(descriptor))
            udp_port = json.loads((directory / "endpoint.json").read_text())["control_udp_port"]
            for name in identities:
                profiles[name] = clients / f"{name}.json"
                lab.run([str(client), "profile", "--identity", str(clients / f"{name}.key"), "--server", str(directory / "server.json"),
                         "--expected-server", descriptor["endpoint"]["id"], "--direct-address", f"127.0.0.1:{udp_port}", "--output", str(profiles[name])], capture_output=True, text=True)
            dashboard = invoke("erin", {"action": "dashboard"})
            for item in dashboard["operations"]:
                operation("erin", item)
            invoke("unknown", {"action": "dashboard"}, "forbidden")
            check("authenticated_iroh_and_unknown_device_refusal")
            if bootable:
                guest_suite.run(lab, directory, evidence, invoke, operation, remote,
                                timeout, image_sha256, summary, check, shrink=shrink,prepare_only=shrink_prepare_only)
            if not shrink:
                for name in ("frank", "heidi"):
                    created = invoke("erin", {"action": "create_user", "request_id": f"new-{name}", "user_id": name, "budget_mib": 128,
                                               "min_memory_budget_mib": 256, "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4})
                    operation("erin", created)
                    invoke("erin", {"action": "enroll_device", "user_id": name, "endpoint_id": identities[name], "label": f"{name} test device"})
                    if invoke(name, {"action": "whoami"})["user_id"] != name:
                        raise RuntimeError("Authenticated identity mapping mismatch")
                check("per_person_zfs_reservations_and_device_enrollment")
                frank_vm = create("frank", "frank-first", "dev-frank")
                operation("frank", frank_vm)
                heidi_vm = create("heidi", "heidi-first", "dev-heidi")
                operation("heidi", heidi_vm)
                remote("systemctl kill --signal=STOP pve-dev-host.service", text=True, timeout=45)
                try:
                    extra = create("frank", "frank-second", "dev-frank-extra")
                    if extra["status"] != "queued":
                        raise RuntimeError("Pending-budget fixture did not durably queue the VM")
                    invoke("frank", {"action": "create_vm", "request_id": "too-many-pending", "name": "over-budget", "disk_mib": 32,
                                     "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None}, "conflict")
                finally:
                    remote("systemctl kill --signal=CONT pve-dev-host.service", text=True, timeout=45)
                operation("frank", extra)
                if create("frank", "frank-first", "dev-frank")["operation_id"] != frank_vm["operation_id"]:
                    raise RuntimeError("Idempotent request changed identity")
                invoke("frank", {"action": "create_vm", "request_id": "too-many", "name": "over-budget", "disk_mib": 32,
                                 "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None}, "conflict")
                check("shared_owner_budget_includes_multiple_vms_and_pending_claims")
                for action in ("start", "stop", "delete"):
                    invoke("frank", {"action": "vm_action", "request_id": f"foreign-{action}", "vm_id": heidi_vm["vm_id"], "kind": action}, "forbidden")
                invoke("frank", {"action": "get_operation", "operation_id": heidi_vm["operation_id"]}, "forbidden")
                invoke("frank", {"action": "create_vm", "request_id": "spoof", "name": "spoofed", "disk_mib": 32,
                                 "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": "heidi"}, "forbidden")
                invoke("frank", {"action": "enroll_device", "user_id": "heidi", "endpoint_id": identities["unknown"], "label": "forbidden"}, "forbidden")
                own = invoke("frank", {"action": "dashboard"})
                if any(item["owner"] != "frank" for item in own["vms"] + own["operations"]):
                    raise RuntimeError("Tenant-scoped dashboard leaked another user's resource")
                check("malicious_cross_tenant_requests_and_visibility_refused")
                quota_checks(lab, evidence, invoke, operation, remote, check, nonce, [frank_vm["vm_id"], extra["vm_id"]], timeout)
                for action in ("start", "stop"):
                    operation("frank", invoke("frank", {"action": "vm_action", "request_id": f"own-{action}", "vm_id": frank_vm["vm_id"], "kind": action}))
                operation("frank", invoke("frank", {"action": "vm_action", "request_id": "delete-extra", "vm_id": extra["vm_id"], "kind": "delete"}))
                if invoke("frank", {"action": "budget"})["allocated_mib"] != 44:
                    raise RuntimeError("Verified deletion did not release precisely one VM's accounting")
                check("owned_start_stop_delete_and_verified_capacity_release")
                remote("systemctl kill --signal=STOP pve-dev-host.service", text=True, timeout=45)
                try:
                    restarted = create("frank", "survive-daemon", "dev-frank-recovered")
                    pending = invoke("frank", {"action": "get_operation", "operation_id": restarted["operation_id"]})
                    if pending["status"] not in ("queued", "running"):
                        raise RuntimeError("Crash recovery fixture did not hold an unfinished durable intent")
                    old_pid = remote("systemctl show -p MainPID --value pve-dev-controller.service", text=True, timeout=45).stdout.strip()
                    remote("systemctl kill --signal=KILL pve-dev-controller.service", text=True, timeout=45)
                finally:
                    remote("systemctl kill --signal=CONT pve-dev-host.service", text=True, timeout=45)
                time.sleep(10)
                deadline = time.monotonic() + 180
                while True:
                    try:
                        resumed = create("frank", "survive-daemon", "dev-frank-recovered")
                        break
                    except (RuntimeError, subprocess.TimeoutExpired):
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(3)
                if resumed["operation_id"] != restarted["operation_id"] or resumed["vm_id"] != restarted["vm_id"]:
                    raise RuntimeError("Fresh daemon duplicated an accepted operation")
                operation("frank", resumed)
                new_pid = remote("systemctl show -p MainPID --value pve-dev-controller.service", text=True, timeout=45).stdout.strip()
                if not new_pid or old_pid == new_pid:
                    raise RuntimeError("Daemon crash test did not establish a fresh process")
                check("daemon_sigkill_after_async_submission_recovers_same_operation")
                oversized = lab.ssh(directory, "zfs create -V 256M labpool/control/frank/over-budget", capture_output=True, text=True, timeout=60)
                (evidence / "quota-refused.log").write_text(oversized.stdout + oversized.stderr)
                quota = remote("zfs get -Hp -o value quota labpool/control/frank", text=True, timeout=60).stdout.strip()
                available = remote("zfs get -Hp -o value available labpool", text=True, timeout=60).stdout.strip()
                probe_volumes = remote("zfs list -H -t volume -o name", text=True, timeout=60).stdout.splitlines()
                verify_quota_refusal(oversized, quota, available, probe_volumes)
                lab.atomic_json(evidence / "quota-properties.json", {"quota": quota, "pool_available": available, "probe_volume_absent": True})
                invoke("erin", {"action": "revoke_device", "endpoint_id": identities["frank"]})
                invoke("frank", {"action": "dashboard"}, "forbidden")
                if invoke("heidi", {"action": "whoami"})["user_id"] != "heidi":
                    raise RuntimeError("Revocation affected the wrong account")
                check("real_zfs_quota_and_live_device_revocation")
            if not shrink_prepare_only:
                dashboard = invoke("erin", {"action": "dashboard"})
                lab.atomic_json(evidence / "dashboard.json", dashboard)
                machines = [vm for vm in dashboard["vms"] if vm["state"] != "deleted"]
                actual = json.loads(remote("pvesh get /nodes/pve-lab/qemu --output-format json", text=True, timeout=120).stdout)
                if {int(vm["vmid"]) for vm in actual} != {vm["vm_id"] for vm in machines}:
                    raise RuntimeError("Actual Proxmox VM inventory differs from controller catalog")
                expected = {f"labpool/control/{vm['owner']}/vm-{vm['vm_id']}-{suffix}" for vm in machines for suffix in ("disk-0", "cloudinit")}
                volumes = remote("zfs list -H -t volume -o name", text=True, timeout=60).stdout.splitlines()
                if set(volumes) != expected:
                    raise RuntimeError("Unexpected, duplicate, or orphan controller zvols")
                if any(vm["status"] != "stopped" for vm in actual) or any(vm["state"] != "stopped" for vm in machines):
                    raise RuntimeError("Final lab inventory contains a running VM")
                check("normal_shrink_cleanup_no_orphans" if shrink else "exact_real_inventory_no_orphans_and_clean_shutdown")
                if shrink:
                    import disk_shrink_suite
                    disk_shrink_suite.interruption(lab, directory, evidence, invoke, operation, remote, check, timeout, image_sha256, summary)
            summary["complete"] = True
        except BaseException as error:
            summary["error"] = (str(error) or type(error).__name__)[:4000]
            print(f"Controller lab failed: {error}", flush=True)
            raise
        finally:
            summary["seconds"] = round(time.monotonic() - started, 1)
            lab.atomic_json(evidence / "summary.json", summary)
            try:
                diagnostics = lab.ssh(directory, "pveversion --verbose; zpool status; zfs list -Hp -t all -o name,used,available; journalctl --no-pager -n 160 -u pve-dev-controller -u pve-dev-host", capture_output=True, text=True, timeout=180)
                (evidence / "diagnostics.log").write_text(diagnostics.stdout + diagnostics.stderr)
                lab.ssh(directory, "systemctl stop pve-dev-controller.service pve-dev-host.service; for vmid in $(qm list | awk 'NR>1 {print $1}'); do if test \"$vmid\" -ge 9001 && test \"$vmid\" -le 9099; then qm stop \"$vmid\"; fi; done", capture_output=True, text=True, timeout=180)
            except (OSError, subprocess.TimeoutExpired) as error:
                (evidence / "cleanup-error.log").write_text(str(error))
    output = root / "evidence" / directory.name.replace("run-controller-", "real-controller-", 1)
    shutil.copytree(evidence, output)
    print(f"Real controller evidence: {output}", flush=True)
