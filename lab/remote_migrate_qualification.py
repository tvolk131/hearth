#!/usr/bin/env python3
"""Actual expanded two-host qualification; local sealed base and toy guests only."""
import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import signal
import subprocess
import time

import guest_suite
import lab
import remote_migrate_spike as spike

ROOT = spike.ROOT
QWORKER = ROOT / "lab/remote_migrate_qualification_worker.py"


def call(directory, nonce, action, *, rpc_timeout=300, **fields):
    output = spike.checked_ssh(directory, "python3 /root/pve-migration-spike/qualification_worker.py",
                               payload=json.dumps({"nonce": nonce, "action": action, **fields}), timeout=rpc_timeout)
    return json.loads(output)


def await_task(directory, nonce, upid, evidence, name, timeout):
    deadline = time.monotonic() + timeout
    states = []
    while True:
        lab.space()
        state = call(directory, nonce, "task", upid=upid)
        states.append(state)
        spike.record(evidence, name, {"upid": upid, "observations": states})
        if state["status"] == "stopped": return state
        if time.monotonic() >= deadline: raise TimeoutError("native source task is still running; outcome uncertain")
        time.sleep(5)


def guest_identity(directory, evidence, key, nonce, *, first_contact, create_sentinel, timeout):
    script = ("import hashlib,json,pathlib,subprocess\n"
              "p=pathlib.Path('/home/dev/migration-sentinel.txt')\n" +
              (f"p.write_text({nonce!r}+'\\n')\n" if create_sentinel else "") +
              "c=subprocess.run(['cloud-init','status','--format','json'],capture_output=True,text=True)\n"
              "print(json.dumps({'cloud_exit':c.returncode,'cloud':json.loads(c.stdout),"
              "'machine_id':pathlib.Path('/etc/machine-id').read_text().strip(),"
              "'instance_id':pathlib.Path('/var/lib/cloud/data/instance-id').read_text().strip(),"
              "'ssh_host_public':pathlib.Path('/etc/ssh/ssh_host_ed25519_key.pub').read_text().strip(),"
              "'sentinel_sha256':hashlib.sha256(p.read_bytes()).hexdigest()}))\n")
    command = "sudo -n python3 -c " + shlex.quote(script)
    deadline = time.monotonic() + timeout
    with guest_suite.serial_capture(lab, directory, evidence, 9002, min(timeout + 60, 7200)) as console:
        while True:
            guest_suite.check_console(evidence, console)
            try:
                result = subprocess.run(guest_suite.guest_ssh(lab, directory, key, "10.77.0.12", command,
                                                              first_contact=first_contact),
                                        capture_output=True, text=True, timeout=180)
            except subprocess.TimeoutExpired:
                result = subprocess.CompletedProcess([], 255, "", "read-only guest SSH readiness timed out")
            guest_suite.check_console(evidence, console)
            if result.returncode == 0:
                identity = json.loads(result.stdout)
                if guest_suite.cloud_readiness(identity["cloud_exit"], json.dumps(identity["cloud"])):
                    if identity["instance_id"] != "migration-guest-" + nonce:
                        raise RuntimeError("custom cloud-init instance identity changed")
                    return identity
            elif "REMOTE HOST IDENTIFICATION HAS CHANGED" in result.stderr:
                raise RuntimeError("guest SSH host identity changed after migration")
            if time.monotonic() >= deadline:
                raise TimeoutError("nested guest OS/SSH did not become healthy: " + result.stderr[-1500:])
            print("Waiting for nested guest OS/SSH: " + result.stderr[-120:].replace("\n", " "), flush=True)
            time.sleep(15)


def prepare_programs(directory, sources):
    for name, content in sources.items():
        if name == "worker.py": name = "remote_migrate_worker.py"
        if name not in ("qualification_worker.py", "prepare_guest_image.py", "remote_migrate_worker.py"):
            continue
        spike.checked_ssh(directory, "cat >/root/pve-migration-spike/" + name,
                          payload=content, timeout=180)


def verified_cached_image(digest):
    if not isinstance(digest, str) or not re.fullmatch("[a-f0-9]{64}", digest):
        raise RuntimeError("guest profile requires an explicit cached image SHA256")
    image = lab.HOME / "physical-trial-image" / (digest + ".qcow2")
    manifest = json.loads(image.with_suffix(".json").read_text())
    if (image.is_symlink() or image.stat().st_mode & 0o222 or image.stat().st_size > 1024**3
            or manifest.get("image_sha256") != digest or manifest.get("export_sha256_verified") is not True
            or manifest.get("exported_before_guest_import_or_boot") is not True
            or manifest.get("root_zvol_exported") is not False or manifest.get("default_user") != "dev"):
        raise RuntimeError("cached pristine image provenance or immutable-file check failed")
    with image.open("rb") as data:
        if hashlib.file_digest(data, "sha256").hexdigest() != digest:
            raise RuntimeError("cached pristine image digest changed")
    return image, manifest


def reboot_fence(source, target, nonce, vmid, evidence, timeout):
    old = call(source, nonce, "boot_identity")
    spike.record(evidence, "source-before-reboot", old)
    try:
        call(source, nonce, "reboot_source")
    except RuntimeError as error:
        spike.record(evidence, "source-reboot-disconnected-ack", {"error": str(error)})
    deadline = time.monotonic() + min(timeout, 600)
    while True:
        try:
            new = call(source, nonce, "boot_identity")
            if new["boot_id"] != old["boot_id"]: break
        except RuntimeError: pass
        if time.monotonic() >= deadline: raise TimeoutError("source did not return with a new boot identity")
        time.sleep(10)
    spike.record(evidence, "source-after-reboot", new)
    if new["machine_id_sha256"] != old["machine_id_sha256"]:
        raise RuntimeError("source machine identity changed during ordinary reboot")
    probe = call(source, nonce, "start_probe", vmid=vmid)
    spike.record(evidence, "rebooted-source-start-refused", probe)
    if probe["start_exit"] == 0 or probe["running_after_start"] or "locked (migrate)" not in probe["stderr"]:
        raise RuntimeError("retained source did not refuse ordinary start after reboot")
    active = call(target, nonce, "inventory")
    spike.record(evidence, "active-target-during-source-fence", active)
    if not any(int(v["vmid"]) == vmid and v["status"] == "running" for v in active["vms"]):
        raise RuntimeError("target was not active during source reboot fence test")


def fence_gate(source, target, nonce, nodes, evidence, run_dir, tasks, jobs, timeout, summary):
    before = call(source, nonce, "create", vmid=9001)
    spike.record(evidence, "protected-source-small-fixture", before)
    if before["protection"] != 1 or before["onboot"] != 0:
        raise RuntimeError("source small fixture is not protected with autostart disabled")
    token = call(target, nonce, "token", vmid=9001)
    spike.private_json(run_dir / "target9001-token.json", token)
    jobs["fence-copy"] = secrets.token_hex(16)
    spike.record(evidence, "fence-copy-intent", {"receipt_id": jobs["fence-copy"], "native_acceptance_may_be_uncertain": True})
    receipt = call(source, nonce, "migrate", vmid=9001, target_vmid=9001, token=token["token"],
                   fingerprint=nodes["b"]["certificate_sha256"], loopback_tunnel=True,
                   bwlimit=0, receipt_id=jobs["fence-copy"])
    tasks["fence-copy"] = receipt["upid"]
    spike.record(evidence, "fence-copy-submission", receipt)
    result = await_task(source, nonce, receipt["upid"], evidence, "fence-copy-task", timeout)
    if result.get("exitstatus") != "OK": raise RuntimeError("protected small cold copy failed")
    after = call(target, nonce, "inspect", vmid=9001)
    spike.record(evidence, "protected-small-target-after", after)
    fields = ("root_sha256", "root_size", "smbios1", "vmgenid", "cloudinit_sha256", "cloudinit_size", "protection", "onboot")
    if any(before[f] != after[f] for f in fields): raise RuntimeError("protected small cold copy changed required identity")
    for name in ("root", "cloudinit"):
        if after[name + "_refreservation"] < after[name + "_size"]: raise RuntimeError("small target disk is sparse")
    summary["checks"].append("protected_small_cold_copy_preserves_identity_thick_storage_and_protection")
    call(target, nonce, "guest_start", vmid=9001)
    reboot_fence(source, target, nonce, 9001, evidence, timeout)
    summary["checks"].append("retained_source_lock_survives_host_reboot_and_refuses_start_while_target_is_active")
    call(target, nonce, "qemu_stop", vmid=9001)
    for directory in (source, target):
        final = call(directory, nonce, "inventory")
        spike.record(evidence, directory.name + "-final-inventory", final)
        volumes = [line.split("\t")[0] for line in final["volumes"]]
        if sorted(volumes) != ["labpool/alloc/vm-9001-cloudinit", "labpool/alloc/vm-9001-disk-0"]:
            raise RuntimeError("small successful transfer retained unexpected volumes/snapshots")
    summary["guest_os_boot_verified"] = False
    summary["source_host_reboot_tested"] = True
    summary["remaining_gates"] = ["guest OS/SSH/custom metadata/lost-ack gate is a separate multi-thread fixture",
                                  "Rust migration API and iroh tunnel are not implemented"]
    summary["complete"] = True


def run(timeout, mode, image_sha256=None):
    lab.space()
    if mode not in ("fence", "guest"): raise ValueError("explicit fence or guest profile required")
    cached = verified_cached_image(image_sha256) if mode == "guest" else None
    threads = {"a": "single" if mode == "fence" else "multi", "b": "multi"}
    if not (lab.HOME / "base/build.json").is_file():
        raise RuntimeError("a sealed existing base is required; qualification never builds one")
    lab.build(timeout)
    nonce = secrets.token_hex(16)
    run_dir = lab.HOME / ("migration-qualification-" + time.strftime("%Y%m%dT%H%M%S") + "-" + nonce[:6])
    run_dir.mkdir(mode=0o700)
    evidence = run_dir / "evidence"
    evidence.mkdir(mode=0o700)
    sources = {"worker.py": spike.WORKER.read_text(), "qualification_worker.py": QWORKER.read_text(),
               "prepare_guest_image.py": (ROOT / "lab/prepare_guest_image.py").read_text()}
    for name, program in sources.items(): (run_dir / name).write_text(program)
    summary = {"schema": 1, "complete": False, "production_contacted": False, "iroh_used": False,
               "mode": mode, "outer_tcg_threading": threads,
               "checks": [], "program_sha256": {name: hashlib.sha256(program.encode()).hexdigest() for name, program in sources.items()},
               "remaining_gates": ["not yet run"]}
    tasks, jobs, prepared, processes = {}, {}, False, None
    started = time.monotonic()
    spike.private_json(run_dir / "supervisor.json", {"pid": os.getpid(), "nonce": nonce})
    for role in ("a", "b"):
        directory = run_dir / role
        directory.mkdir(mode=0o700)
        subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(lab.HOME / "base/root.qcow2"), str(directory / "root.qcow2")], check=True)
        spike.seed(directory, role, nonce)
        for letter in ("a", "b"):
            subprocess.run(["qemu-img", "create", "-f", "qcow2", str(directory / f"data-{letter}.qcow2"), "8G"], check=True)
    base = json.loads((lab.HOME / "base/build.json").read_text())
    print("Expanded migration qualification: " + str(run_dir), flush=True)
    try:
        with spike.boot_pair(run_dir, nonce, tcg_threads=threads) as processes:
            try:
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    ready = {r: pool.submit(lab.ready, run_dir / r, processes[r], timeout) for r in ("a", "b")}
                    summary["versions"] = {r: f.result() for r, f in ready.items()}
                    pending = {r: pool.submit(spike.prepare, run_dir / r, r, nonce, base["nonce"], sources["worker.py"]) for r in ("a", "b")}
                    nodes = {r: f.result() for r, f in pending.items()}
                source, target = run_dir / "a", run_dir / "b"
                spike.record(evidence, "independent-nodes", nodes)
                for field in ("node", "machine_id_sha256", "certificate_sha256", "ca_sha256", "ssh_key_sha256", "root_rsa_public_sha256"):
                    if nodes["a"][field] == nodes["b"][field]: raise RuntimeError("independent identity duplicated")
                prepared = True
                for directory in (source, target):
                    prepare_programs(directory, sources)
                    call(directory, nonce, "qualification_enable")
                    spike.record(evidence, directory.name + "-storage", call(directory, nonce, "storage"))
                    call(directory, nonce, "network_start")
                keyinfo = call(source, nonce, "tunnel_key")
                spike.record(evidence, "loopback-api", call(target, nonce, "tunnel_target", **keyinfo))
                host_key = spike.checked_ssh(target, "cat /etc/ssh/ssh_host_ed25519_key.pub").strip()
                tunnel = {"host_public_key": host_key, "fingerprint": nodes["b"]["certificate_sha256"]}
                spike.record(evidence, "scoped-ssh-tunnel", call(source, nonce, "tunnel_start", **tunnel))
                summary["checks"].append("loopback_only_destination_api_over_pinned_scoped_ssh_forward")
                print("Private destination API and scoped SSH tunnel ready; profile=" + mode, flush=True)
                if mode == "fence":
                    fence_gate(source, target, nonce, nodes, evidence, run_dir, tasks, jobs, timeout, summary)
                    return summary
                snippets = {r: call(run_dir / r, nonce, "snippet_stage") for r in ("a", "b")}
                if snippets["a"] != snippets["b"]: raise RuntimeError("staged custom metadata hash/identity differs")
                spike.record(evidence, "staged-custom-metadata", snippets)
                clean, manifest = cached
                digest = manifest["image_sha256"]
                command = "install -d -m 0700 /var/lib/pve-dev-images && cat >/var/lib/pve-dev-images/" + digest + ".qcow2 && chmod 0444 /var/lib/pve-dev-images/" + digest + ".qcow2"
                with clean.open("rb") as image:
                    uploaded = lab.ssh(source, command, stdin=image, capture_output=True, timeout=900)
                uploaded.check_returncode()
                summary["guest_image_sha256"] = digest
                spike.record(evidence, "reused-pristine-image-manifest", manifest)
                spike.record(evidence, "pristine-image-export", {"sha256": digest,
                    "exported_before_guest_import_or_boot": True, "root_zvol_exported": False,
                    "reused_original_verified_private_export": True})
                derived = manifest
                print("Original pristine prepared image reused by verified SHA256", flush=True)
                key = run_dir / "guest-ssh"
                lab.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)])
                spike.record(evidence, "source-guest-created", call(source, nonce, "guest_create", vmid=9002,
                             image_sha256=derived["image_sha256"], ssh_public_key=key.with_suffix(".pub").read_text().strip(), rpc_timeout=1200))
                call(source, nonce, "guest_start", vmid=9002)
                guest_evidence = evidence / "source-guest"
                guest_evidence.mkdir()
                initial = guest_identity(source, guest_evidence, key, nonce, first_contact=True, create_sentinel=True, timeout=timeout)
                spike.record(evidence, "source-guest-identity", initial)
                call(source, nonce, "guest_stop", vmid=9002)
                before = call(source, nonce, "inspect", vmid=9002, rpc_timeout=600)
                spike.record(evidence, "bootable-before", before)
                if before["protection"] != 1 or before["onboot"] != 0:
                    raise RuntimeError("bootable source is not protected with autostart disabled")
                token2 = call(target, nonce, "token", vmid=9002)
                spike.private_json(run_dir / "target9002-token.json", token2)
                job = secrets.token_hex(16)
                jobs["lost-ack"] = job
                spike.record(evidence, "lost-ack-intent", {"receipt_id": job, "native_acceptance_may_be_uncertain": True})
                # Deliberately discard this acknowledgement. A durable lab-only
                # source receipt is the witness; reconciliation must use its UPID.
                call(source, nonce, "migrate", vmid=9002, target_vmid=9002, token=token2["token"],
                     fingerprint=nodes["b"]["certificate_sha256"], loopback_tunnel=True,
                     bwlimit=0, receipt_id=job)
                recovered = call(source, nonce, "receipt", receipt_id=job)
                tasks["lost-ack"] = recovered["upid"]
                spike.record(evidence, "lost-ack-recovered-receipt", recovered)
                status = await_task(source, nonce, recovered["upid"], evidence, "bootable-transfer-task", timeout)
                if status.get("exitstatus") != "OK": raise RuntimeError("bootable/custom-snippet native transfer failed: " + str(status.get("exitstatus")))
                after = call(target, nonce, "inspect", vmid=9002, rpc_timeout=600)
                spike.record(evidence, "bootable-after", after)
                fields = ("root_sha256", "root_size", "smbios1", "vmgenid", "cloudinit_sha256", "cloudinit_meta_sha256", "cloudinit_size", "cicustom", "protection", "onboot")
                if any(before[f] != after[f] for f in fields): raise RuntimeError("cold copied guest identity or bytes differ")
                for name in ("root", "cloudinit"):
                    if after[name + "_refreservation"] < after[name + "_size"]: raise RuntimeError("target " + name + " is not thick-backed")
                summary["checks"].append("lost_ack_reconciled_without_native_resubmission_and_exact_bootable_copy")
                shutil.copyfile(source / "guest_known_hosts", target / "guest_known_hosts")
                call(target, nonce, "guest_start", vmid=9002)
                guest_evidence = evidence / "target-guest"
                guest_evidence.mkdir()
                migrated = guest_identity(target, guest_evidence, key, nonce, first_contact=False, create_sentinel=False, timeout=timeout)
                spike.record(evidence, "target-guest-identity", migrated)
                for field in ("machine_id", "instance_id", "ssh_host_public", "sentinel_sha256"):
                    if migrated[field] != initial[field]: raise RuntimeError("guest identity changed after transfer: " + field)
                summary["checks"].append("target_guest_os_boot_ssh_key_file_and_cloud_init_instance_identity_preserved")
                summary["guest_os_boot_verified"] = True
                summary["source_host_reboot_tested"] = False
                call(target, nonce, "guest_stop", vmid=9002)
                for directory in (source, target):
                    final = call(directory, nonce, "inventory")
                    spike.record(evidence, directory.name + "-final-inventory", final)
                    positive = [line.split("\t")[0] for line in final["volumes"] if "/vm-9002-" in line]
                    if sorted(positive) != ["labpool/alloc/vm-9002-cloudinit", "labpool/alloc/vm-9002-disk-0"]:
                        raise RuntimeError("completed bootable copy has unexpected snapshots or orphan volumes")
                summary["remaining_gates"] = ["Rust migration RPC/state machine and iroh tunnel are not implemented",
                    "source-receipt persistence has an acceptance-to-write ambiguity window; drill covers persisted native receipt only",
                    "not a test of arbitrary root override or live zero-downtime migration",
                    "source host reboot fence is a separate single-thread fixture"]
                summary["complete"] = True
            except Exception as error:
                summary["failure"] = str(error)
                spike.record(evidence, "immediate-failure", {"error": str(error)})
                print("Qualification phase failed: " + str(error), flush=True)
            finally:
                if prepared:
                    for name, job in jobs.items():
                        if name in tasks: continue
                        try:
                            receipt = call(run_dir / "a", nonce, "receipt", receipt_id=job)
                            spike.record(evidence, name + "-recovered-before-cleanup", receipt)
                            if "upid" in receipt: tasks[name] = receipt["upid"]
                            else: summary["accepted_task_outcome_uncertain"] = True
                        except Exception as error:
                            summary["accepted_task_outcome_uncertain"] = True
                            spike.record(evidence, name + "-receipt-recovery-error", {"error": str(error)})
                    if summary.get("accepted_task_outcome_uncertain"): summary["complete"] = False
                    for name, upid in tasks.items():
                        try:
                            state = call(run_dir / "a", nonce, "task", upid=upid)
                            spike.record(evidence, name + "-before-cleanup", state)
                            if state["status"] != "stopped": summary["complete"] = False
                        except Exception as error:
                            summary["complete"] = False
                            spike.record(evidence, name + "-cleanup-uncertain", {"upid": upid, "error": str(error)})
                    for role in ("a", "b"):
                        for action in (("tunnel_stop", "cleanup_tokens") if role == "a" else ("cleanup_tokens", "tunnel_account_remove")):
                            try:
                                result = call(run_dir / role, nonce, action)
                                spike.record(evidence, role + "-" + action, result)
                                if action == "cleanup_tokens" and result.get("all_spike_tokens_removed") is not True:
                                    summary["complete"] = False
                                if action == "tunnel_account_remove" and result.get("temporary_ssh_account_removed") is not True:
                                    summary["complete"] = False
                                if action == "tunnel_stop" and not (result.get("owned_tunnel_stopped") or result.get("already_absent")):
                                    summary["complete"] = False
                            except Exception as error:
                                summary["complete"] = False
                                spike.record(evidence, role + "-" + action + "-error", {"error": str(error)})
    except Exception as error:
        summary["failure"] = str(error)
    finally:
        summary["seconds"] = round(time.monotonic() - started, 1)
        summary["qemu_children_stopped"] = bool(processes) and all(p.poll() is not None for p in processes.values())
        if not summary["qemu_children_stopped"]: summary["complete"] = False
        spike.record(evidence, "summary", summary)
        export = ROOT / "evidence" / run_dir.name.replace("migration-qualification-", "remote-migrate-qualification-", 1)
        shutil.copytree(evidence, export)
        print("Qualification evidence: " + str(export), flush=True)
    return summary


if __name__ == "__main__":
    def interrupted(signum, frame): raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=int, default=2400)
    parser.add_argument("--mode", choices=("fence", "guest"), required=True)
    parser.add_argument("--image-sha256")
    args = parser.parse_args()
    if not 60 <= args.timeout <= 5400: parser.error("timeout must be 60–5400 seconds")
    with (lab.HOME / "lab.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        result = run(args.timeout, args.mode, args.image_sha256)
    raise SystemExit(0 if result["complete"] else 1)
