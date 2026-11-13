#!/usr/bin/env python3
"""Two independent disposable PVE nodes; never accepts a production endpoint."""
import argparse
import concurrent.futures
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import subprocess
import tempfile
import time

import lab

ROOT = Path(__file__).resolve().parents[1]
WORKER = ROOT / "lab/remote_migrate_worker.py"


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def private_json(path, value):
    with open(path, "x", opener=lambda name, flags: os.open(name, flags, 0o600)) as out:
        json.dump(value, out)


def seed(directory, role, nonce):
    """Keep cached management identity until independent host setup via SSH.

    Management MACs may match: the two QEMU user-NAT domains are disjoint.
    Re-running new-instance cloud-init before pmxcfs mounts is unsafe on a PVE
    clone because its root authorized_keys can point into that mount.
    """
    if role not in ("a", "b") or not re.fullmatch("[a-f0-9]{32}", nonce):
        raise ValueError("invalid disposable seed identity")
    shutil.copyfile(lab.HOME / "base/seed.iso", directory / "seed.iso")


def qemu_command(directory, role, link_port, ssh_port, api_port, qmp_path, *, tcg_thread="multi"):
    if role not in ("a", "b") or directory.parent.parent != lab.HOME:
        raise ValueError("only a fresh two-node lab directory is accepted")
    if tcg_thread not in ("single", "multi"):
        raise ValueError("invalid fixed TCG threading mode")
    private_link = (f"socket,id=migration,listen=127.0.0.1:{link_port}" if role == "a"
                    else f"socket,id=migration,connect=127.0.0.1:{link_port}")
    args = ["qemu-system-x86_64", "-name", f"pve-migration-spike-{role}", "-machine", "q35",
            "-accel", "tcg,thread=" + tcg_thread, "-cpu", "max", "-smp", "4", "-m", "4096",
            "-display", "none", "-monitor", "none", "-serial", f"file:{directory / 'serial.log'}",
            "-qmp", f"unix:{qmp_path},server=on,wait=off",
            "-drive", f"file={directory / 'root.qcow2'},format=qcow2,if=none,id=rootdisk,discard=unmap",
            "-device", "virtio-blk-pci,drive=rootdisk,bootindex=1,addr=0x3",
            "-drive", f"file={directory / 'seed.iso'},format=raw,media=cdrom,readonly=on",
            "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22,hostfwd=tcp:127.0.0.1:{api_port}-:8006",
            "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56,addr=0x2",
            "-netdev", private_link,
            "-device", f"virtio-net-pci,netdev=migration,mac=02:50:77:00:00:0{1 if role == 'a' else 2},addr=0x4"]
    for letter in ("a", "b"):
        args += ["-drive", f"file={directory / ('data-' + letter + '.qcow2')},format=qcow2,if=none,id=data-{letter},discard=unmap",
                 "-device", f"virtio-blk-pci,drive=data-{letter},serial=lab-data-{letter}"]
    return args


@contextlib.contextmanager
def boot_pair(run_dir, nonce, *, tcg_threads=None):
    """Owns both child PIDs; no broad kill or public host listener."""
    link_port = port()
    processes, logs = {}, []
    with tempfile.TemporaryDirectory(prefix="pve-migrate-qmp-", dir="/tmp") as short:
        try:
            for role in ("a", "b"):
                directory = run_dir / role
                ssh_port, api_port = port(), port()
                key = (lab.identity() / "host.pub").read_text().strip()
                (directory / "known_hosts").write_text(f"[127.0.0.1]:{ssh_port} {key}\n")
                lab.atomic_json(directory / "endpoint.json", {"port": ssh_port, "api_port": api_port, "nonce": nonce})
                qmp_path = Path(short) / f"{role}.sock"
                lab.atomic_json(directory / "qmp.json", {"path": str(qmp_path)})
                args = qemu_command(directory, role, link_port, ssh_port, api_port, qmp_path,
                                    tcg_thread=(tcg_threads or {}).get(role, "multi"))
                log = (directory / "qemu.log").open("a")
                logs.append(log)
                process = subprocess.Popen(args, stdout=log, stderr=log)
                processes[role] = process
                lab.atomic_json(run_dir / "qemu-ownership.json", {
                    "supervisor_pid": os.getpid(), "nonce": nonce,
                    "roles": {name: {"pid": child.pid, "directory": str(run_dir / name)}
                              for name, child in processes.items()}})
                if role == "a":
                    deadline = time.monotonic() + 15
                    while time.monotonic() < deadline:
                        # Verify listener readiness without opening a spurious
                        # socket-network client which would claim its one peer.
                        if qmp_path.exists():
                            break
                        if process.poll() is not None:
                            raise RuntimeError("source QEMU failed before private link setup")
                        time.sleep(.05)
            yield processes
        finally:
            for role, process in reversed(list(processes.items())):
                if process.poll() is None:
                    try:
                        lab.qmp(run_dir / role, "system_powerdown")
                        process.wait(timeout=60)
                    except (OSError, ValueError, subprocess.TimeoutExpired, RuntimeError):
                        process.terminate()
                        try:
                            process.wait(timeout=15)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()
            for log in logs:
                log.close()
            lab.atomic_json(run_dir / "qemu-cleanup.json", {
                "supervisor_pid": os.getpid(), "nonce": nonce,
                "roles": {name: {"pid": child.pid, "exit": child.poll()}
                          for name, child in processes.items()}})


def checked_ssh(directory, command, *, payload=None, timeout=180):
    result = lab.ssh(directory, command, input=payload, text=True,
                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    if result.returncode:
        # Commands contain no token; worker output itself redacts token values.
        raise RuntimeError(f"lab command failed ({result.returncode}): {result.stderr[-3000:]}")
    return result.stdout


def call(directory, nonce, action, **fields):
    value = {"nonce": nonce, "action": action, **fields}
    output = checked_ssh(directory, "python3 /root/pve-migration-spike/worker.py",
                         payload=json.dumps(value), timeout=300)
    return json.loads(output)


def prepare(directory, role, nonce, base_nonce, program):
    args = [role, nonce, base_nonce]
    if not re.fullmatch("[a-f0-9]{32}", base_nonce):
        raise ValueError("invalid sealed base nonce")
    install = ('test "$(hostname)" = pve-lab && test "$(cat /etc/pve-dev-lab-id)" = '
               + base_nonce + ' && install -d -m 0700 /root/pve-migration-spike && '
               'cat >/root/pve-migration-spike/worker.py && chmod 0700 /root/pve-migration-spike/worker.py')
    checked_ssh(directory, install, payload=program)
    command = "python3 /root/pve-migration-spike/worker.py prepare " + " ".join(args)
    output = checked_ssh(directory, command, timeout=600)
    info = json.loads(output)
    endpoint = json.loads((directory / "endpoint.json").read_text())
    (directory / "known_hosts").write_text(f"[127.0.0.1]:{endpoint['port']} {info.pop('ssh_public_key')}\n")
    return info


def record(directory, name, value):
    lab.atomic_json(directory / f"{name}.json", value)


def await_task(source, nonce, upid, evidence, name, timeout):
    deadline = time.monotonic() + timeout
    observations = []
    while True:
        lab.space()
        status = call(source, nonce, "task", upid=upid)
        observations.append(status)
        record(evidence, name, {"upid": upid, "observations": observations})
        if status["status"] == "stopped":
            return status
        if time.monotonic() >= deadline:
            raise TimeoutError("accepted migration task remained running at deadline")
        time.sleep(5)


def run(timeout):
    lab.HOME.mkdir(mode=0o700, exist_ok=True)
    lab.space()
    if not (lab.HOME / "base/build.json").is_file():
        raise RuntimeError("sealed local base is required; this spike does not build/download one")
    lab.build(timeout)  # Verifies fixed inputs and cached parent checksum only.
    nonce = secrets.token_hex(16)
    run_dir = lab.HOME / ("migration-" + time.strftime("%Y%m%dT%H%M%S") + "-" + nonce[:6])
    run_dir.mkdir(mode=0o700)
    evidence = run_dir / "evidence"
    evidence.mkdir(mode=0o700)
    info = json.loads((lab.HOME / "base/build.json").read_text())
    summary = {"schema": 1, "backend": "real-two-node-proxmox-remote-migrate", "production_contacted": False,
               "public_host_listener": False, "nested_guest_boot_verified": False,
               "complete": False, "checks": [], "run_directory": str(run_dir)}
    started = time.monotonic()
    prepared = False
    processes = None
    accepted_tasks = {}
    worker_source = WORKER.read_text()
    (run_dir / "worker-frozen.py").write_text(worker_source)
    summary["worker_sha256"] = hashlib.sha256(worker_source.encode()).hexdigest()
    private_json(run_dir / "supervisor.json", {"pid": os.getpid(), "nonce": nonce})
    for role in ("a", "b"):
        directory = run_dir / role
        directory.mkdir(mode=0o700)
        subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b",
                        str(lab.HOME / "base/root.qcow2"), str(directory / "root.qcow2")], check=True)
        seed(directory, role, nonce)
        for letter in ("a", "b"):
            subprocess.run(["qemu-img", "create", "-f", "qcow2", str(directory / f"data-{letter}.qcow2"), "2G"], check=True)
    print(f"Migration spike private run: {run_dir}", flush=True)
    try:
        with boot_pair(run_dir, nonce) as processes:
            try:
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    futures = {role: pool.submit(lab.ready, run_dir / role, processes[role], timeout) for role in ("a", "b")}
                    summary["proxmox"] = {role: future.result() for role, future in futures.items()}
                    futures = {role: pool.submit(prepare, run_dir / role, role, nonce, info["nonce"], worker_source) for role in ("a", "b")}
                    nodes = {role: future.result() for role, future in futures.items()}
                record(evidence, "independent-nodes", nodes)
                for field in ("node", "machine_id_sha256", "certificate_sha256", "ca_sha256", "ssh_key_sha256", "root_rsa_public_sha256"):
                    if nodes["a"][field] == nodes["b"][field]:
                        raise RuntimeError(f"standalone identity duplicated: {field}")
                summary["checks"].append("independent_single_node_machine_ca_and_ssh_identities")
                prepared = True
                print("Both independent Proxmox nodes prepared", flush=True)
                source, target = run_dir / "a", run_dir / "b"
                for directory in (source, target):
                    record(evidence, f"{directory.name}-storage", call(directory, nonce, "storage"))
                record(evidence, "real-zfs-quota-semantics", call(source, nonce, "quota_probe"))
                record(evidence, "peer-connectivity", call(source, nonce, "peer", fingerprint=nodes["b"]["certificate_sha256"]))
                summary["checks"].append("private_direct_peer_tls_fingerprint")
                before = call(source, nonce, "create", vmid=9001)
                record(evidence, "source-before", before)
                token = call(target, nonce, "token", vmid=9001)
                private_json(run_dir / "target-token.json", token)
                record(evidence, "target-conflict-created", call(target, nonce, "create_conflict", vmid=9001))
                conflict_before = call(target, nonce, "inspect_conflict", vmid=9001)
                record(evidence, "target-conflict-before", conflict_before)
                try:
                    conflict = call(source, nonce, "migrate", vmid=9001, target_vmid=9001,
                                    fingerprint=nodes["b"]["certificate_sha256"], token=token["token"], bwlimit=0)
                except RuntimeError as error:
                    if "already exists" not in str(error):
                        raise
                    record(evidence, "same-id-conflict-refused", {"refused": True, "error": str(error)})
                else:
                    accepted_tasks["conflict"] = conflict["upid"]
                    record(evidence, "conflict-submission", conflict)
                    failed = await_task(source, nonce, conflict["upid"], evidence, "conflict-task", timeout)
                    reason = json.dumps(failed).lower()
                    if failed.get("exitstatus") == "OK" or not any(s in reason for s in ("already exists", "already defined", "exists on")):
                        raise RuntimeError("conflict task did not produce the expected duplicate-VM refusal: " + str(failed.get("exitstatus")))
                    record(evidence, "same-id-conflict-refused", {"refused": True, "asynchronous": True,
                           "upid": conflict["upid"], "exitstatus": failed.get("exitstatus")})
                conflict_after = call(target, nonce, "inspect_conflict", vmid=9001)
                record(evidence, "target-conflict-after", conflict_after)
                if conflict_after != conflict_before:
                    raise RuntimeError("failed conflict migration changed existing target UUID/config/GUID/disk bytes")
                record(evidence, "target-conflict-removed", call(target, nonce, "remove_conflict", vmid=9001))
                summary["checks"].append("same_id_target_conflict_refused_without_adoption")
                # Token values never enter a command argument, command log or
                # exported evidence. They pass only through pinned SSH stdin.
                result = call(source, nonce, "migrate", vmid=9001, target_vmid=9001,
                              fingerprint=nodes["b"]["certificate_sha256"], token=token["token"], bwlimit=0)
                record(evidence, "migration-submission", result)
                accepted_tasks["migration"] = result["upid"]
                print(f"Migration submitted; source task {result['upid']}", flush=True)
                status = await_task(source, nonce, result["upid"], evidence, "migration-task", timeout)
                if status.get("exitstatus") != "OK":
                    raise RuntimeError("remote migration failed: " + str(status.get("exitstatus")))
                after_source = call(source, nonce, "inspect", vmid=9001)
                after_target = call(target, nonce, "inspect", vmid=9001)
                record(evidence, "source-after", after_source)
                record(evidence, "target-after", after_target)
                mismatches = [field for field in ("root_sha256", "root_size", "smbios1", "vmgenid", "cloudinit_sha256", "cloudinit_meta_sha256", "cloudinit_size")
                              if before[field] != after_target[field]]
                summary["identity_mismatches"] = mismatches
                summary["primitive_transfer_completed"] = True
                record(evidence, "copy-assertions", {"mismatches": mismatches,
                       "target_root_thick": after_target["root_refreservation"] >= after_target["root_size"],
                       "target_cloudinit_thick": after_target["cloudinit_refreservation"] >= after_target["cloudinit_size"]})
                if after_target["root_refreservation"] < after_target["root_size"]:
                    raise RuntimeError("destination root is sparse despite thick target storage")
                summary["checks"] += ["stopped_vm_transfer_preserves_disk_bytes_uuid_and_cloudinit", "target_root_remains_thick"]
                try:
                    repeated = call(source, nonce, "migrate", vmid=9001, target_vmid=9001,
                                    fingerprint=nodes["b"]["certificate_sha256"], token=token["token"], bwlimit=0)
                except RuntimeError as error:
                    if "already exists" not in str(error) and "locked" not in str(error):
                        raise
                    record(evidence, "completed-transfer-repeat", {"refused": True, "error": str(error)})
                else:
                    accepted_tasks["repeat"] = repeated["upid"]
                    record(evidence, "repeat-submission", repeated)
                    failed = await_task(source, nonce, repeated["upid"], evidence, "repeat-task", timeout)
                    if failed.get("exitstatus") == "OK":
                        raise RuntimeError("completed transfer repeat unexpectedly completed successfully")
                    record(evidence, "completed-transfer-repeat", {"refused": True, "asynchronous": True,
                           "upid": repeated["upid"], "exitstatus": failed.get("exitstatus")})
                if call(target, nonce, "inspect", vmid=9001) != after_target:
                    raise RuntimeError("repeated transfer changed target identity or content")
                summary["checks"].append("completed_transfer_repeat_refused_target_unchanged")
                record(evidence, "source-start-probe", call(source, nonce, "start_probe", vmid=9001))
                if "root_sha256" not in mismatches:
                    record(evidence, "target-start-probe", call(target, nonce, "start_probe", vmid=9001))
                record(evidence, "revoked-token", call(target, nonce, "revoke", vmid=9001))
                summary["checks"].append("ephemeral_target_api_token_revoked")
                summary["complete"] = not mismatches
                if mismatches:
                    summary["failure"] = "primitive copied VM but required identity checks failed: " + ", ".join(mismatches)
            except Exception as error:
                summary["failure"] = str(error)
                record(evidence, "immediate-failure", {"error": str(error)})
                print(f"Migration phase failed before cleanup: {error}", flush=True)
                raise
            finally:
                if prepared:
                    for name, upid in accepted_tasks.items():
                        try:
                            state = call(run_dir / "a", nonce, "task", upid=upid)
                            record(evidence, name + "-before-cleanup", state)
                            if state.get("status") != "stopped":
                                summary["accepted_task_uncertain_before_cleanup"] = True
                                summary["complete"] = False
                        except Exception as error:
                            record(evidence, name + "-before-cleanup-error", {"upid": upid, "error": str(error)})
                            summary["accepted_task_uncertain_before_cleanup"] = True
                            summary["complete"] = False
                    summary["api_token_cleanup"] = {}
                    for role in ("a", "b"):
                        try:
                            cleanup = call(run_dir / role, nonce, "cleanup_tokens")
                            record(evidence, f"{role}-token-cleanup", cleanup)
                            summary["api_token_cleanup"][role] = cleanup["all_spike_tokens_removed"]
                        except Exception as error:
                            summary["api_token_cleanup"][role] = False
                            record(evidence, f"{role}-token-cleanup-error", {"error": str(error)})
                    if not all(summary["api_token_cleanup"].values()):
                        summary["complete"] = False
                for role in ("a", "b"):
                    try:
                        diagnostics = checked_ssh(run_dir / role, "pveversion --verbose; zpool status; pvesh get /cluster/resources --type vm --output-format json", timeout=90)
                        (evidence / f"{role}-diagnostics.log").write_text(diagnostics)
                    except Exception as error:
                        (evidence / f"{role}-diagnostics-error.txt").write_text(str(error))
    except Exception as error:
        summary["failure"] = str(error)
        print(f"Migration spike incomplete: {error}", flush=True)
    finally:
        summary["seconds"] = round(time.monotonic() - started, 1)
        summary["qemu_children_stopped"] = bool(processes) and all(process.poll() is not None for process in processes.values())
        if not summary["qemu_children_stopped"]:
            summary["complete"] = False
        record(evidence, "summary", summary)
        export = ROOT / "evidence" / run_dir.name.replace("migration-", "remote-migrate-spike-", 1)
        shutil.copytree(evidence, export)
        print(f"Migration evidence: {export}", flush=True)
    return summary


def main():
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    if not 60 <= args.timeout <= 5400:
        parser.error("timeout must be 60–5400 seconds")
    with (lab.HOME / "lab.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        summary = run(args.timeout)
    raise SystemExit(0 if summary["complete"] else 1)


if __name__ == "__main__":
    main()
