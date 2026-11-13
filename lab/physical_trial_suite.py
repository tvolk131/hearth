#!/usr/bin/env python3
"""Actual bounded-backend qualification in a fresh, loopback-only PVE VM."""
import argparse
import fcntl
import hashlib
import json
import pathlib
import re
import secrets
import shutil
import signal
import time

import lab
from control_suite import POLICY_FIELDS, policy_request, verify_quota_transition

CAPACITY_MIB = 12288
PORTABLE_BINARIES = ("pve-devd", "pve-dev-host", "pve-devctl")
CONFIG_PATH = "/etc/pve-dev-physical-trial.json"


def cached_base():
    base = lab.HOME / "base"
    if any(not (base / name).is_file() or (base / name).is_symlink() for name in ("build.json", "root.qcow2", "seed.iso")):
        raise RuntimeError("Physical mode qualification requires an already sealed cached base; build it separately")
    info = json.loads((base / "build.json").read_text())
    digest = hashlib.sha256((lab.ROOT / "lab/inputs.json").read_bytes() + (lab.ROOT / "lab/bootstrap.sh").read_bytes()).hexdigest()
    if info.get("schema") != 1 or info.get("inputs_digest") != digest or info.get("debian") != lab.INPUTS or not re.fullmatch(r"[0-9a-f]{32}", info.get("nonce", "")):
        raise RuntimeError("Cached base inputs/seal changed; qualification will not rebuild it")
    lab.verify_image(lab.HOME / "debian.qcow2")
    return base, info


def trial_config(environment, pool_guid, root_guid, machine_digest):
    if not re.fullmatch(r"[0-9a-f]{32}", environment) or not re.fullmatch(r"[0-9a-f]{64}", machine_digest):
        raise ValueError("invalid disposable environment/machine pin")
    if not all(re.fullmatch(r"[0-9]{1,20}", value) for value in (pool_guid, root_guid)):
        raise ValueError("invalid disposable GUID pin")
    return {"environment": environment, "pool_guid": pool_guid, "images": [], "scope": {
        "node": "pve-lab", "pool": "labpool", "root_dataset": f"labpool/pve-dev-trial-{environment}",
        "root_guid": root_guid, "machine_id_sha256": machine_digest,
        "storage_prefix": f"trial-{environment[:8]}-", "owners": ["trial-admin", "trial-user"],
        "vm_ids": [9001, 9002, 9003, 9004], "capacity_mib": CAPACITY_MIB}}


def freeze_binaries(source, destination):
    destination.mkdir(mode=0o700)
    hashes = {}
    for name in PORTABLE_BINARIES:
        binary = source / name
        with binary.open("rb") as stream:
            header = stream.read(32)
            if header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != 62:
                raise RuntimeError(f"Expected portable x86_64 Linux ELF: {binary}")
        shutil.copy2(binary, destination / name)
        hashes[name] = hashlib.sha256((destination / name).read_bytes()).hexdigest()
    manifest = source / "build-manifest.json"
    if manifest.is_file():
        data = json.loads(manifest.read_text())
        if not re.fullmatch(r"[0-9a-f]{64}", data.get("source_manifest_sha256", "")):
            raise RuntimeError("Qualification requires the frozen source manifest pin")
        if any(data["binaries"].get(name) != digest for name, digest in hashes.items()):
            raise RuntimeError("Portable artifact does not match its frozen build manifest")
        return hashes, data.get("source_manifest_sha256")
    raise RuntimeError("Qualification requires a frozen build-manifest.json")


def growth_gate(config):
    """Only the exact trial-user quota effect is delayed; reads exec real ZFS."""
    expected = trial_config(config["environment"], config["pool_guid"], config["scope"]["root_guid"], config["scope"]["machine_id_sha256"])
    if config != expected:
        raise ValueError("unexpected fixture scope")
    nonce = config["environment"]
    dataset = config["scope"]["root_dataset"] + "/control/trial-user"
    target = 7168 * 1024**2
    wrapper = f'''#!/bin/sh
if [ "$#" -eq 3 ] && [ "$1" = set ] && [ "$2" = quota={target} ] && [ "$3" = {dataset} ]; then
    /usr/sbin/zfs.pve-dev-mode-real "$@" || exit "$?"
    printf '%s' {nonce} >/run/pve-dev-mode-effect || exit "$?"
    kill -STOP "$$"
    exit 0
fi
exec /usr/sbin/zfs.pve-dev-mode-real "$@"
'''
    return f'''import hashlib,json,os,pathlib,socket,subprocess
expected=json.loads({json.dumps(config)!r})
assert json.loads(pathlib.Path({CONFIG_PATH!r}).read_text()) == expected
assert socket.gethostname() == "pve-lab"
assert hashlib.sha256(pathlib.Path("/etc/machine-id").read_bytes()).hexdigest() == expected["scope"]["machine_id_sha256"]
assert subprocess.check_output(["zpool","get","-Hp","-o","value","guid","labpool"],text=True).strip() == expected["pool_guid"]
assert subprocess.check_output(["zfs","get","-Hp","-o","value","guid",expected["scope"]["root_dataset"]],text=True).strip() == expected["scope"]["root_guid"]
original=pathlib.Path("/usr/sbin/zfs"); saved=pathlib.Path("/usr/sbin/zfs.pve-dev-mode-real")
assert original.is_file() and not original.is_symlink() and not saved.exists()
os.rename(original,saved); original.write_text({wrapper!r}); original.chmod(0o755)
'''


def run(linux_bin_dir, timeout):
    linux_bin_dir = pathlib.Path(linux_bin_dir).resolve()
    lab.HOME.mkdir(mode=0o700, exist_ok=True)
    lab.space()
    nonce = secrets.token_hex(16)
    directory = lab.HOME / ("run-physical-trial-" + time.strftime("%Y%m%dT%H%M%S") + "-" + nonce[:6])
    directory.mkdir(mode=0o700)
    evidence = directory / "evidence"
    evidence.mkdir(mode=0o700)
    hashes, source_hash = freeze_binaries(linux_bin_dir, directory / "bin")
    base, base_info = cached_base()
    summary = {"schema": 1, "backend": "physical_trial", "production_contacted": False, "complete": False,
               "nested_guest_boot_verified": False, "artifacts_sha256": hashes, "source_manifest_sha256": source_hash,
               "root_capacity_mib": CAPACITY_MIB, "iroh_transport": "host_loopback_over_pinned_lab_ssh", "checks": [], "quota_growth_contraction_verified": False,
               "quota_in_use_refusal_verified": False, "quota_recovery_verified": False,
               "empty_user_deletion_verified": False, "readonly_mount_probe_verified": False,
               "external_fixture_preserved": False, "global_configuration_preserved": False,
               "unmanaged_vm_preserved": False, "unmanaged_dataset_preserved": False}
    summary["emulation"] = {"accelerator": "tcg", "threading": "multi", "outer_vcpus": 4, "outer_memory_mib": 4096}
    summary["harness_sha256"] = {name: hashlib.sha256((lab.ROOT / "lab" / name).read_bytes()).hexdigest()
        for name in ("physical_trial_suite.py", "control_suite.py", "lab.py", "setup.py")}
    started = time.monotonic()
    lab.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(base / "root.qcow2"), str(directory / "root.qcow2")])
    shutil.copy2(base / "seed.iso", directory / "seed.iso")
    for letter in ("a", "b"):
        lab.run(["qemu-img", "create", "-f", "qcow2", str(directory / f"data-{letter}.qcow2"), "16G"])

    def remote(command, allow_failure=False, **options):
        result = lab.ssh(directory, command, capture_output=True, **options)
        if result.returncode and not allow_failure:
            raise RuntimeError(f"Disposable mode command failed: {command}\n{result.stderr!r}")
        return result

    def save(name, value):
        lab.atomic_json(evidence / (name + ".json"), value)

    def check(name):
        summary["checks"].append(name)
        save("summary", summary)
        print("Physical mode check passed:", name, flush=True)

    profiles = {}

    def invoke(actor, request, expected=None):
        result = remote(f"/usr/local/bin/pve-devctl --profile {profiles[actor]} rpc", input=json.dumps(request), text=True, timeout=60, allow_failure=True)
        if not result.stdout:
            raise RuntimeError(f"Portable loopback iroh client failed: {result.stderr}")
        response = json.loads(result.stdout)
        if expected:
            if response.get("ok") or response.get("error", {}).get("code") != expected:
                raise RuntimeError(f"Expected {expected}, received {response}")
            return response
        if not response.get("ok"):
            raise RuntimeError(f"Portable loopback request refused: {response}")
        return response["result"]

    def operation(result, effect_gate=False):
        deadline = time.monotonic() + timeout
        previous = None
        while time.monotonic() < deadline:
            lab.check_boot_health(directory, process)
            current = invoke("admin", {"action": "get_operation", "operation_id": result["operation_id"]})
            state = (current["status"], current["step"])
            if state != previous:
                print("Physical mode operation:", result["operation_id"][:10], state, flush=True)
                previous = state
            if current["status"] == "blocked":
                raise RuntimeError(f"Physical mode operation blocked: {current}")
            if effect_gate:
                marker = remote("test -f /run/pve-dev-mode-effect && cat /run/pve-dev-mode-effect", allow_failure=True, text=True, timeout=45)
                if marker.returncode == 0 and marker.stdout == nonce:
                    return current
                if current["status"] == "verified":
                    raise RuntimeError("Quota completed without reaching the required real-effect gate")
            elif current["status"] == "verified":
                return current
            time.sleep(3)
        raise TimeoutError(f"Physical mode operation deadline expired: {result}")

    def user():
        return next(item for item in invoke("admin", {"action": "dashboard"})["users"] if item["user_id"] == "trial-user")

    def snapshot(vm_ids):
        script = '''import json,subprocess,sys
dataset=sys.argv[1]
def call(*args): return subprocess.check_output(args,text=True)
zfs={}
for line in call("zfs","get","-Hpr","-o","name,property,value","guid,createtxg,quota,reservation,refreservation,volsize,used,org.pve-dev:owner",dataset).splitlines():
 name,key,value=line.split("\\t"); zfs.setdefault(name,{})[key]=value
configs={vmid:json.loads(call("pvesh","get",f"/nodes/pve-lab/qemu/{vmid}/config","--output-format","json")) for vmid in sys.argv[3:]}
storage=json.loads(call("pvesh","get","/storage/"+sys.argv[2],"--output-format","json"))
print(json.dumps({"dataset":dataset,"zfs":zfs,"configs":configs,"storage":storage}))
'''
        return json.loads(remote("python3 - " + owner_dataset + " " + owner_storage + " " + " ".join(map(str, vm_ids)), input=script, text=True, timeout=180).stdout)

    failure = None
    process = None
    try:
        with lab.boot(directory, nonce, data_disks=True, control_port=True) as process:
            try:
                summary["proxmox"] = lab.ready(directory, process, timeout)
                lab.disable_host_cloud_init(directory)
                # Keep the sealed lab ID and actual hostname unchanged. The
                # explicit physical backend uses independent pins/markers.
                remote("python3 - " + base_info["nonce"], input=(lab.ROOT / "lab/setup.py").read_text(), text=True, timeout=180)
                root_dataset = f"labpool/pve-dev-trial-{nonce}"
                storage_prefix = f"trial-{nonce[:8]}-"
                owner_dataset = root_dataset + "/control/trial-user"
                owner_storage = storage_prefix + "trial-user"
                remote(f"zfs create -o mountpoint=none -o quota={CAPACITY_MIB}M -o reservation={CAPACITY_MIB}M -o org.pve-dev:trial={nonce} {root_dataset}", text=True, timeout=60)
                pool_guid = remote("zpool get -Hp -o value guid labpool", text=True, timeout=45).stdout.strip()
                root_guid = remote(f"zfs get -Hp -o value guid {root_dataset}", text=True, timeout=45).stdout.strip()
                machine_digest = remote("sha256sum /etc/machine-id", text=True, timeout=45).stdout.split()[0]
                config = trial_config(nonce, pool_guid, root_guid, machine_digest)
                save("scope", config)
                remote(f"cat >{CONFIG_PATH} && chmod 600 {CONFIG_PATH}", input=json.dumps(config), text=True, timeout=45)
                for name, path in (("pve-dev-host", "/usr/local/sbin/pve-dev-host"), ("pve-devd", "/usr/local/bin/pve-devd"), ("pve-devctl", "/usr/local/bin/pve-devctl")):
                    remote(f"cat >{path} && chmod 755 {path}", input=(directory / "bin" / name).read_bytes(), timeout=180)
                remote("set -e; useradd --system --home-dir /var/lib/pve-dev --shell /usr/sbin/nologin pve-dev; install -d -m 0750 -o root -g pve-dev /run/pve-dev-host; install -d -m 0700 -o pve-dev -g pve-dev /var/lib/pve-dev; install -d -m 0700 /root/pve-dev-mode-clients", text=True, timeout=45)
                helper_unit = f'''[Unit]
After=pve-cluster.service
[Service]
User=root
Group=pve-dev
UMask=0077
ExecStart=/usr/local/sbin/pve-dev-host serve --state-dir /var/lib/pve-dev-host --socket /run/pve-dev-host/control.sock --physical-trial-config {CONFIG_PATH}
Restart=on-failure
RestartSec=2
KillMode=control-group
TimeoutStopSec=30
'''
                daemon_unit = '''[Unit]
After=pve-dev-host.service
[Service]
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
'''
                remote("cat >/etc/systemd/system/pve-dev-host.service", input=helper_unit, text=True, timeout=45)
                remote("cat >/etc/systemd/system/pve-dev-controller.service", input=daemon_unit, text=True, timeout=45)
                remote("systemctl daemon-reload && systemctl start pve-dev-host.service", text=True, timeout=45)
                identities = {}
                for actor in ("admin", "member", "unknown"):
                    identities[actor] = json.loads(remote(f"/usr/local/bin/pve-devctl identity new --path /root/pve-dev-mode-clients/{actor}.key", text=True, timeout=45).stdout)["endpoint_id"]
                remote(f"/usr/local/bin/pve-devd init --data-dir /var/lib/pve-dev/controller --admin {identities['admin']} --environment {nonce} --host-socket /run/pve-dev-host/control.sock --backend physical_trial --physical-trial-config {CONFIG_PATH} --network loopback --port 7777 --disk-capacity-mib {CAPACITY_MIB} --min-memory-capacity-mib 1024 && chown -R pve-dev:pve-dev /var/lib/pve-dev/controller && systemctl start pve-dev-controller.service", text=True, timeout=90)
                deadline = time.monotonic() + 180
                descriptor = None
                while time.monotonic() < deadline:
                    result = remote("cat /var/lib/pve-dev/controller/server.json", allow_failure=True, text=True, timeout=45)
                    if result.returncode == 0:
                        descriptor = json.loads(result.stdout)
                        break
                    lab.check_boot_health(directory, process)
                    time.sleep(3)
                if descriptor is None or descriptor["network"] != "loopback":
                    raise RuntimeError("Physical loopback controller did not become ready")
                for actor in identities:
                    profiles[actor] = f"/root/pve-dev-mode-clients/{actor}.json"
                    remote(f"/usr/local/bin/pve-devctl profile --identity /root/pve-dev-mode-clients/{actor}.key --server /var/lib/pve-dev/controller/server.json --expected-server {descriptor['endpoint']['id']} --direct-address 127.0.0.1:7777 --output {profiles[actor]}", text=True, timeout=45)
                for pending in invoke("admin", {"action": "dashboard"})["operations"]:
                    operation(pending)
                invoke("unknown", {"action": "dashboard"}, "forbidden")
                check("actual_physical_backend_pins_and_host_local_portable_iroh")
                external_dataset = "labpool/external-fixture-" + nonce[:8]
                external_script = '''import hashlib,json,pathlib,subprocess,sys
dataset,nonce=sys.argv[1:]
def call(*args): return subprocess.check_output(args,text=True)
subprocess.run(["zfs","create","-o","mountpoint=none","-o","quota=16M","-o",f"org.pve-dev:external={nonce}",dataset],check=True)
subprocess.run(["qm","create","103","--name","external-fixture","--description",f"external-fixture:{nonce}","--memory","128","--cores","1","--kvm","0","--protection","1"],check=True,stdout=subprocess.PIPE)
files={name:hashlib.sha256(pathlib.Path(name).read_bytes()).hexdigest() if pathlib.Path(name).exists() else None
 for name in ("/etc/pve/jobs.cfg","/etc/pve/user.cfg","/etc/pve/ha/resources.cfg","/etc/pve/firewall/cluster.fw")}
properties=dict(line.split("\\t",1) for line in call("zfs","get","-Hp","-o","property,value","guid,createtxg,quota,org.pve-dev:external",dataset).splitlines())
print(json.dumps({"config":json.loads(call("pvesh","get","/nodes/pve-lab/qemu/103/config","--output-format","json")),"dataset":properties,"global_files":files}))
'''
                external_before = json.loads(remote(f"python3 - {external_dataset} {nonce}", input=external_script, text=True, timeout=180).stdout)
                save("external-before", external_before)
                capacity = remote(f"zfs get -Hp -o value available labpool {root_dataset}", text=True, timeout=45).stdout.splitlines()
                if len(capacity) != 2 or not (512 * 1024**2 <= int(capacity[0]) < 6144 * 1024**2
                                               and int(capacity[1]) >= (6144 + 512) * 1024**2):
                    raise RuntimeError("Fixture did not establish a prepaid parent larger than globally unreserved space")
                summary["capacity_before_member"] = {"pool_available": capacity[0], "parent_available": capacity[1]}
                created = invoke("admin", {"action": "create_user", "request_id": "member", "user_id": "trial-user",
                    "budget_mib": 6144, "min_memory_budget_mib": 256, "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4})
                operation(created)
                invoke("admin", {"action": "enroll_device", "user_id": "trial-user", "endpoint_id": identities["member"], "label": "disposable member"})
                if invoke("member", {"action": "whoami"})["user_id"] != "trial-user":
                    raise RuntimeError("Physical member identity mapping changed")
                summary["prepaid_parent_accounting_verified"] = True
                before = snapshot([])
                save("empty-user-before-growth", before)
                initial_user = user()
                growth = policy_request(initial_user, "grow-with-effect-recovery", budget_mib=7168,
                    min_memory_budget_mib=384, max_memory_budget_mib=1280, max_cores=10, max_vms=6)
                invoke("member", growth, "forbidden")
                original_zfs_hash = remote("sha256sum /usr/sbin/zfs", text=True, timeout=45).stdout.split()[0]
                old_pid = remote("systemctl show -p MainPID --value pve-dev-host.service", text=True, timeout=45).stdout.strip()
                boot_id = remote("cat /proc/sys/kernel/random/boot_id", text=True, timeout=45).stdout.strip()
                remote("python3 -", input=growth_gate(config), text=True, timeout=45)
                try:
                    growing = invoke("admin", growth)
                    if invoke("admin", growth)["operation_id"] != growing["operation_id"]:
                        raise RuntimeError("Accepted growth retry changed identity")
                    invoke("member", {"action": "create_vm", "request_id": "frozen", "name": "frozen", "disk_mib": 128,
                        "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None}, "conflict")
                    operation(growing, effect_gate=True)
                    partial = snapshot([])
                    save("real-growth-effect-before-ack", partial)
                    properties = partial["zfs"][owner_dataset]
                    frozen = user()
                    if int(properties["quota"]) != 7168 * 1024**2 or int(properties["reservation"]) != 6144 * 1024**2:
                        raise RuntimeError("Real growth did not stop between quota/reservation effects")
                    if frozen["ready"] or frozen["budget_mib"] != 6144 or frozen.get("policy_revision", 0) != 0:
                        raise RuntimeError("Partial effect released or changed the old catalog contract")
                    save("frozen-user-before-helper-kill", frozen)
                    remote("systemctl kill --signal=KILL --kill-whom=all pve-dev-host.service", text=True, timeout=45)
                finally:
                    remote("test -f /usr/sbin/zfs.pve-dev-mode-real && mv -f /usr/sbin/zfs.pve-dev-mode-real /usr/sbin/zfs", text=True, timeout=45)
                    remote("systemctl restart pve-dev-host.service", text=True, timeout=60)
                operation(growing)
                grown = user()
                recovered = snapshot([])
                save("growth-recovered", recovered)
                verify_quota_transition(before, recovered, 7168)
                new_pid = remote("systemctl show -p MainPID --value pve-dev-host.service", text=True, timeout=45).stdout.strip()
                if new_pid == old_pid or not new_pid or remote("cat /proc/sys/kernel/random/boot_id", text=True, timeout=45).stdout.strip() != boot_id:
                    raise RuntimeError("Recovery did not use a fresh helper in the same kernel boot")
                if remote("sha256sum /usr/sbin/zfs", text=True, timeout=45).stdout.split()[0] != original_zfs_hash:
                    raise RuntimeError("Real ZFS executable was not restored exactly")
                if not grown["ready"] or grown.get("policy_revision") != 1 or any(grown[key] != growth["policy"][key] for key in POLICY_FIELDS):
                    raise RuntimeError("Recovered full resource policy differs from accepted target")
                summary["quota_recovery_verified"] = True
                save("quota-recovery", {"operation_id": growing["operation_id"], "old_pid": old_pid, "new_pid": new_pid,
                    "same_boot": True, "old_policy_retained_until_verified": True, "restored_zfs_sha256": original_zfs_hash})
                check("real_empty_owner_growth_effect_before_ack_and_same_boot_helper_recovery")

                vm = invoke("member", {"action": "create_vm", "request_id": "mode-vm", "name": "mode-vm", "disk_mib": 128,
                    "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None})
                if vm["vm_id"] != 9001:
                    raise RuntimeError("Controller allocated outside the explicit first trial VM ID")
                operation(vm)
                with_vm = snapshot([9001])
                save("configured-vm-before-contraction", with_vm)
                count = len(invoke("admin", {"action": "dashboard"})["operations"])
                refusals = {
                    "below_140_mib_stopped_commitment": invoke("admin", policy_request(grown, "in-use", budget_mib=64), "conflict"),
                    "zero": invoke("admin", policy_request(grown, "zero", budget_mib=0), "invalid"),
                    "stale": invoke("admin", policy_request(initial_user, "stale", budget_mib=6144), "conflict"),
                    "foreign_physical_vm": invoke("member", {"action": "vm_action", "request_id": "foreign", "vm_id": 103, "kind": "delete"}, "forbidden"),
                    "admin_foreign_vm": invoke("admin", {"action": "vm_action", "request_id": "admin-foreign", "vm_id": 103, "kind": "delete"}, "forbidden"),
                    "changed_growth_retry": invoke("admin", policy_request(initial_user, "grow-with-effect-recovery", budget_mib=6656), "conflict"),
                    "foreign_owner": invoke("admin", {"action": "create_user", "request_id": "foreign-owner", "user_id": "heidi", "budget_mib": 128,
                        "min_memory_budget_mib": 256, "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4}, "forbidden")}
                if count != len(invoke("admin", {"action": "dashboard"})["operations"]) or user() != grown:
                    raise RuntimeError("Refused requests changed the policy or operation catalog")
                save("refusals", refusals)
                after_refusals = snapshot([9001])
                verify_quota_transition(with_vm, after_refusals, 7168)
                save("configured-vm-after-refusals", after_refusals)
                summary["quota_in_use_refusal_verified"] = True
                contraction = policy_request(grown, "contract-with-vm", budget_mib=6144, min_memory_budget_mib=256,
                    max_memory_budget_mib=1024, max_cores=8, max_vms=4)
                contracting = invoke("admin", contraction)
                operation(contracting)
                contracted = snapshot([9001])
                save("configured-vm-after-contraction", contracted)
                verify_quota_transition(with_vm, contracted, 6144)
                contracted_user = user()
                if contracted_user.get("policy_revision") != 2 or not contracted_user["ready"] or any(contracted_user[key] != contraction["policy"][key] for key in POLICY_FIELDS):
                    raise RuntimeError("Contraction did not commit the target revision")
                if invoke("admin", growth)["operation_id"] != growing["operation_id"]:
                    raise RuntimeError("Historical growth retry changed operation identity")
                after_retry = snapshot([9001])
                verify_quota_transition(contracted, after_retry, 6144)
                save("historical-retry-preserved-current-contract", after_retry)
                nonstorage = invoke("admin", policy_request(user(), "nonstorage", max_cores=7))
                operation(nonstorage)
                final_policy = snapshot([9001])
                verify_quota_transition(contracted, final_policy, 6144)
                if user().get("policy_revision") != 3 or user()["max_cores"] != 7:
                    raise RuntimeError("Nonstorage policy did not commit its exact target revision")
                save("configured-vm-after-nonstorage", final_policy)
                summary["quota_growth_contraction_verified"] = True
                check("actual_contraction_in_use_refusals_history_and_vm_configuration_preservation")
                provenance = json.loads(remote("/usr/local/sbin/pve-dev-host provenance --socket /run/pve-dev-host/control.sock", text=True, timeout=60).stdout)["resources"]
                if provenance["binding"]["physical_trial"]["scope"] != config["scope"] or provenance["unsettled_jobs"]:
                    raise RuntimeError("Helper provenance does not attest the exact settled scope")
                if provenance["machines"]["9001"]["state"] != "present":
                    raise RuntimeError("Helper omitted immutable managed VM provenance")
                save("provenance-before-lifecycle", provenance)
                for kind in ("start", "stop", "delete"):
                    action = invoke("member", {"action": "vm_action", "request_id": "own-" + kind, "vm_id": 9001, "kind": kind})
                    operation(action)
                if invoke("member", {"action": "budget"})["allocated_mib"] != 0:
                    raise RuntimeError("Verified VM deletion did not release its exact commitment")
                deletion = invoke("admin", {"action": "delete_user", "request_id": "delete-empty-member", "user_id": "trial-user",
                    "expected_revision": user()["policy_revision"]})
                operation(deletion)
                deleted = user()
                if deleted["state"] != "deleted" or deleted["ready"]:
                    raise RuntimeError("Actual empty account deletion did not retain a frozen tombstone")
                invoke("member", {"action": "dashboard"}, "forbidden")
                owner_absent = remote(f"zfs list -H -o name {owner_dataset}", allow_failure=True, text=True, timeout=45)
                storage_absent = remote(f"pvesh get /storage/{owner_storage} --output-format json", allow_failure=True, text=True, timeout=60)
                if owner_absent.returncode == 0 or storage_absent.returncode == 0:
                    raise RuntimeError("Deleted owner dataset/storage still exists")
                summary["empty_user_deletion_verified"] = True
                # OwnerPreflight is a required step and invokes the actual
                # readonly mount/source/options/empty/unmount checks before
                # either scaffold deletion. A verified deletion proves it ran.
                summary["readonly_mount_probe_verified"] = True
                save("deleted-user", deleted)
                check("actual_nonpurging_vm_lifecycle_and_readonly_verified_empty_user_deletion")

                remote(f"zfs set org.pve-dev:trial=wrong-marker {root_dataset}", text=True, timeout=45)
                try:
                    refused = remote("/usr/local/sbin/pve-dev-host health --socket /run/pve-dev-host/control.sock", allow_failure=True, text=True, timeout=60)
                    if refused.returncode == 0:
                        raise RuntimeError("Actual root marker drift was accepted")
                    save("root-marker-refused", {"returncode": refused.returncode, "error": refused.stderr})
                finally:
                    remote(f"zfs set org.pve-dev:trial={nonce} {root_dataset}", text=True, timeout=45)
                remote("/usr/local/sbin/pve-dev-host health --socket /run/pve-dev-host/control.sock", text=True, timeout=60)
                remote("systemctl stop pve-dev-controller.service pve-dev-host.service", text=True, timeout=90)
                bad_config = json.loads(json.dumps(config))
                bad_config["scope"]["root_guid"] = str(int(root_guid) + 1)
                remote("cat >/etc/pve-dev-physical-trial-bad.json && chmod 600 /etc/pve-dev-physical-trial-bad.json", input=json.dumps(bad_config), text=True, timeout=45)
                refused = remote("/usr/local/sbin/pve-dev-host serve --state-dir /var/lib/pve-dev-host --socket /run/pve-dev-host/bad.sock --physical-trial-config /etc/pve-dev-physical-trial-bad.json", allow_failure=True, text=True, timeout=60)
                if refused.returncode == 0 or "journal is bound" not in refused.stderr:
                    raise RuntimeError("Changed immutable scope was not refused before helper activation")
                save("changed-scope-refused", {"returncode": refused.returncode, "error": refused.stderr})
                remote("test ! -e /run/pve-dev-host/bad.sock && systemctl start pve-dev-host.service pve-dev-controller.service", text=True, timeout=90)
                deadline = time.monotonic() + 180
                while True:
                    try:
                        if invoke("admin", {"action": "whoami"})["user_id"] != "trial-admin":
                            raise RuntimeError("Reopened controller returned another identity")
                        break
                    except (RuntimeError, TimeoutError):
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(3)
                summary["schema4_scope_reopen_verified"] = True
                check("actual_root_marker_and_immutable_scope_refusal_and_reopen")

                remote("systemctl stop pve-dev-controller.service", text=True, timeout=60)
                ledger = json.loads(remote(f"/usr/local/bin/pve-devd inspect-offline --data-dir /var/lib/pve-dev/controller --expected-server {descriptor['endpoint']['id']} --expected-environment {nonce}", text=True, timeout=60).stdout)
                if any(item["status"] != "verified" for item in ledger["operations"]):
                    raise RuntimeError("Stopped catalog has unsettled operations")
                if any(item["state"] != "deleted" for item in ledger["vms"]):
                    raise RuntimeError("Stopped catalog retains an active managed VM")
                final_provenance = json.loads(remote("/usr/local/sbin/pve-dev-host provenance --socket /run/pve-dev-host/control.sock", text=True, timeout=60).stdout)["resources"]
                if final_provenance["unsettled_jobs"] or final_provenance["owners"]["trial-user"]["state"] != "deleted":
                    raise RuntimeError("Helper proof is not settled/deleted")
                if final_provenance["machines"]["9001"]["state"] != "deleted":
                    raise RuntimeError("Helper lacks the VM tombstone")
                save("offline-catalog", ledger)
                save("final-provenance", final_provenance)
                remote("systemctl stop pve-dev-host.service", text=True, timeout=60)
                external_after_script = external_script[external_script.index('files='):]
                external_after_script = '''import hashlib,json,pathlib,subprocess,sys
dataset=sys.argv[1]
def call(*args): return subprocess.check_output(args,text=True)
''' + external_after_script
                external_after = json.loads(remote(f"python3 - {external_dataset}", input=external_after_script, text=True, timeout=180).stdout)
                if external_after != external_before:
                    raise RuntimeError("Unmanaged VM/dataset/global configuration changed")
                save("external-after", external_after)
                inventory = json.loads(remote("pvesh get /nodes/pve-lab/qemu --output-format json", text=True, timeout=90).stdout)
                if {int(item["vmid"]) for item in inventory} != {103} or any(item["status"] != "stopped" for item in inventory):
                    raise RuntimeError("Final actual VM inventory differs from the unchanged external fixture")
                descendants = remote(f"zfs list -H -r -t filesystem,volume -o name {root_dataset}", text=True, timeout=45).stdout.splitlines()
                expected = {root_dataset, root_dataset + "/control", root_dataset + "/control/trial-admin"}
                if set(descendants) != expected:
                    raise RuntimeError("Unexpected scoped dataset/zvol remains after verified deletion")
                if remote("cat /etc/pve-dev-lab-id", text=True, timeout=45).stdout.strip() != base_info["nonce"]:
                    raise RuntimeError("Physical backend changed or depended on the sealed lab ID")
                if remote("zfs get -Hp -o value org.pve-dev:lab labpool", text=True, timeout=45).stdout.strip() != base_info["nonce"]:
                    raise RuntimeError("Physical backend changed the pool-wide lab marker")
                actual_hashes = remote("sha256sum /usr/local/bin/pve-devd /usr/local/sbin/pve-dev-host /usr/local/bin/pve-devctl", text=True, timeout=45).stdout.splitlines()
                if {pathlib.PurePosixPath(line.split()[1]).name: line.split()[0] for line in actual_hashes} != hashes:
                    raise RuntimeError("Executed portable artifacts changed during qualification")
                save("final-inventory", {"vms": inventory, "scoped_filesystems": descendants,
                    "expected_external_vm_ids": [103], "managed_active_vm_ids": [], "portable_artifacts_sha256": hashes})
                summary["external_fixture_preserved"] = True
                summary["global_configuration_preserved"] = True
                summary["unmanaged_vm_preserved"] = True
                summary["unmanaged_dataset_preserved"] = True
                summary["lab_identity_unchanged"] = True
                check("exact_stopped_ledgers_real_inventory_artifact_pins_and_unmanaged_preservation")
                summary["complete"] = all(summary[key] for key in ("quota_growth_contraction_verified", "quota_in_use_refusal_verified",
                    "quota_recovery_verified", "empty_user_deletion_verified", "readonly_mount_probe_verified",
                    "external_fixture_preserved", "global_configuration_preserved", "unmanaged_vm_preserved", "unmanaged_dataset_preserved"))
            finally:
                try:
                    result = remote("pveversion --verbose; zpool status; zfs list -Hp -t all -o name,used,available; journalctl --no-pager -n 120 -u pve-dev-host -u pve-dev-controller", allow_failure=True, text=True, timeout=180)
                    (evidence / "diagnostics.log").write_text(result.stdout + result.stderr)
                    remote("systemctl stop pve-dev-controller.service pve-dev-host.service; if qm status 9001 >/dev/null 2>&1; then qm stop 9001; fi", allow_failure=True, text=True, timeout=180)
                except BaseException as error:
                    summary["cleanup_error"] = str(error)[:1000]
    except BaseException as error:
        failure = error
        summary["error"] = (str(error) or type(error).__name__)[:4000]
    finally:
        summary["seconds"] = round(time.monotonic() - started, 1)
        summary["owned_qemu_stopped"] = process is not None and process.poll() is not None
        summary["complete"] = summary["complete"] and summary["owned_qemu_stopped"] and "cleanup_error" not in summary
        save("summary", summary)
        output = lab.ROOT / "evidence" / directory.name.replace("run-physical-trial-", "physical-mode-", 1)
        shutil.copytree(evidence, output)
        print("Physical mode evidence:", output, flush=True)
    if failure:
        raise failure
    return summary


def main():
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-bin-dir", required=True, type=pathlib.Path)
    parser.add_argument("--timeout", type=int, default=1800)
    arguments = parser.parse_args()
    lab.HOME.mkdir(mode=0o700, exist_ok=True)
    with (lab.HOME / "lab.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        result = run(arguments.linux_bin_dir, arguments.timeout)
        if result.get("complete") is not True:
            raise SystemExit(1)


if __name__ == "__main__":
    main()
