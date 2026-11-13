#!/usr/bin/env python3
"""Boot/data-preservation qualification on one fresh, loopback-only PVE VM."""
import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import secrets
import signal
import shlex
import subprocess
import sys
import os
import time

import lab
import guest_suite


def layout_preflight(lab, directory, evidence, remote, check, vm_id, filename="shrink-admission-layout.json", workspace_id=None):
    if workspace_id is not None and (len(workspace_id)!=64 or any(c not in "0123456789abcdef" for c in workspace_id)):
        raise ValueError("invalid read-only workspace probe identity")
    source = (lab.ROOT / "src/control/shrink_worker.py").read_text()
    manifest = json.loads((evidence / "build-manifest.json").read_text())
    if hashlib.sha256(source.encode()).hexdigest() != manifest["source_files_sha256"]["src/control/shrink_worker.py"]:
        raise RuntimeError("layout probe source differs from frozen helper source")
    environment = json.loads((directory / "endpoint.json").read_text())["nonce"]
    script = f'''import json,pathlib,socket
assert socket.gethostname()=="pve-lab" and pathlib.Path("/etc/pve-dev-lab-id").read_text().strip()=={environment!r}
scope={{"__name__":"shrink_readonly_preflight"}}
exec(compile({source!r},"<pinned-shrink-adapter>","exec"),scope)
disk=scope["device"]("labpool/control/labdev/vm-{vm_id}-disk-0")
value,root=scope["layout"](disk)
scope["partition_number"](disk,root)
preparation={workspace_id!r}
if preparation is not None:
 work="labpool/pve-dev-shrink-"+preparation
 assert scope["run"]("zfs","get","-H","-o","value","org.pve-dev:shrink",work)==preparation
 scope["empty_filesystem"](work)
print(json.dumps(value))
'''
    result = remote("python3 -", input=script, text=True, timeout=180)
    value=json.loads(result.stdout)
    lab.atomic_json(evidence / filename, value)
    if filename=="shrink-admission-layout.json":check("pinned_image_shrink_layout_preflight_before_guest_boot")
    return value


def preparation_only(lab, directory, evidence, invoke, operation, remote, check, vm_id, summary):
    # Qualify actual copy/fsck/resize/GPT/swap first, without waiting for a cold
    # nested guest. No guest-verification or recovery-deletion attestation is made.
    request = {"action":"shrink_vm","request_id":"prepare-only-shrink","vm_id":vm_id,
               "expected_disk_mib":4096,"target_disk_mib":2560,"confirm_downtime":True}
    prepared = invoke("erin", request)
    operation("erin", prepared)
    if invoke("erin", request)["operation_id"] != prepared["operation_id"]:
        raise RuntimeError("prepare-only retry changed operation identity")
    before = invoke("erin", {"action":"dashboard"})
    hold = next(s for s in before["disk_shrinks"] if s["vm_id"] == vm_id)
    if not hold["prepared"] or hold["operation_id"] != prepared["operation_id"]:
        raise RuntimeError("prepared recovery hold missing")
    root = f"labpool/control/labdev/vm-{vm_id}"
    expected = {root+"-disk-0", root+"-cloudinit", "labpool/pve-dev-shrink-"+prepared["operation_id"]+"/original"}
    if set(remote("zfs list -H -t volume -o name",text=True,timeout=60).stdout.splitlines()) != expected:
        raise RuntimeError("prepare-only volumes differ from the exact recovery/target set")
    remote("systemctl restart pve-dev-host.service pve-dev-controller.service",text=True,timeout=180)
    deadline = time.monotonic()+180
    while True:
        try:
            after = invoke("erin", {"action":"dashboard"})
            if before != after:raise RuntimeError("prepared state changed after paired service restart")
            break
        except (RuntimeError,subprocess.TimeoutExpired):
            if time.monotonic()>=deadline:raise
            time.sleep(3)
    lab.atomic_json(evidence/"shrink-prepare-only-dashboard.json",after)
    provenance=json.loads(remote("/usr/local/sbin/pve-dev-host provenance --socket /run/pve-dev-host/control.sock",text=True,timeout=60).stdout)
    lab.atomic_json(evidence/"shrink-prepare-only-provenance.json",provenance)
    before_layout=json.loads((evidence/"shrink-admission-layout.json").read_text())
    after_layout=layout_preflight(lab,directory,evidence,remote,check,vm_id,"shrink-prepare-only-layout.json",prepared["operation_id"])
    if not after_layout["filesystem_bytes"]<before_layout["filesystem_bytes"]:
        raise RuntimeError("preparation-only fixture did not actually reduce the ext4 filesystem")
    for identity in ("disk_guid","root_partition_uuid","root_fs_uuid","root_start"):
        if before_layout[identity]!=after_layout[identity]:raise RuntimeError("offline guest disk identity changed")
    if int(provenance["resources"]["machines"][str(vm_id)]["identities"]["volume"]["volsize"])!=2560*1024**2:
        raise RuntimeError("prepared target backing size differs")
    check("actual_preparation_copy_ext4_GPT_swap_and_restart_with_recovery_retained")
    check("read_only_workspace_mount_and_unmount_qualified_before_guest_boot")
    summary.update(shrink_preparation_verified=True,source_disk_mib=4096,target_disk_mib=2560,guest_boot_or_recovery_deletion_qualified=False,
                   preparation_source_disk_mib=4096,preparation_target_disk_mib=2560,initial_preparation_operation_id=prepared["operation_id"],
                   intentional_prepared_fixture_retained_in_retired_overlay=True)


def exercise(lab, directory, evidence, invoke, operation, remote, check, key, address, vm_id, timeout, summary,
             source_mib=4096,target_mib=3072,final_budget_mib=4096,previous_preparation=None):
    summary["shrink_exercise_sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    nonce = secrets.token_hex(32)
    # Fill with unwritten extents, allocate actual random data, then remove the
    # padding. Record physical extents rather than assuming allocation order.
    seed = "set -eu; available=$(df -B1 --output=avail / | tail -n1); " \
        'fallocate -l "$((available-268435456))" "$HOME/shrink-padding"; ' \
        'dd if=/dev/urandom of="$HOME/shrink-sentinel.bin" bs=1M count=32 status=none; ' \
        f'printf %s {shlex.quote(nonce)} >"$HOME/shrink-nonce"; sync; ' \
        '/usr/sbin/filefrag -v "$HOME/shrink-sentinel.bin"; rm "$HOME/shrink-padding"; sync'
    result = subprocess.run(guest_suite.guest_ssh(lab, directory, key, address, seed), capture_output=True, text=True, timeout=600)
    (evidence / "shrink-sentinel-extents.txt").write_text(result.stdout + result.stderr)
    result.check_returncode()
    identity_script = '''import hashlib,json,pathlib,subprocess
def call(*args):return subprocess.check_output(args,text=True).strip()
print(json.dumps({"machine_id":pathlib.Path("/etc/machine-id").read_text().strip(),
"instance_id":call("cloud-init","query","instance_id"),
"ssh_host_key":pathlib.Path("/etc/ssh/ssh_host_ed25519_key.pub").read_text().split()[:2],
"sentinel_sha256":hashlib.sha256((pathlib.Path.home()/"shrink-sentinel.bin").read_bytes()).hexdigest(),
"nonce":(pathlib.Path.home()/"shrink-nonce").read_text()}))'''
    identity_command = "python3 -c " + shlex.quote(identity_script)
    def identity():
        # Nested TCG can take several minutes for Python + cloud-init query +
        # hashing. Keep the identity comparison strict, with a bounded deadline
        # consistent with the existing sentinel creation deadline.
        result = subprocess.run(guest_suite.guest_ssh(lab, directory, key, address, identity_command), capture_output=True, text=True, timeout=600)
        result.check_returncode()
        value = json.loads(result.stdout)
        if value["nonce"] != nonce:
            raise RuntimeError("sentinel nonce changed")
        return value
    before = identity()
    lab.atomic_json(evidence / "shrink-identity-before.json", before)
    operation("erin", invoke("erin", {"action":"vm_action","request_id":"shrink-clean-shutdown","vm_id":vm_id,"kind":"shutdown"}))
    check("clean_guest_shutdown_without_force_stop")
    if previous_preparation is not None:
        if not summary["nested_guest_boot_verified"]:raise RuntimeError("cannot attest initial preparation without guest health")
        initial=invoke("erin", {"action":"finish_vm_shrink","request_id":"finish-initial-preparation","vm_id":vm_id,
            "preparation_id":previous_preparation,"confirm_guest_verified":True,"confirm_delete_recovery":True})
        operation("erin",initial)
        if invoke("erin", {"action":"dashboard"}).get("disk_shrinks"):raise RuntimeError("initial preparation hold was not released")
        lab.atomic_json(evidence/"shrink-initial-preparation-finished.json",invoke("erin",{"action":"get_operation","operation_id":initial["operation_id"]}))
        summary.update(initial_preparation_boot_and_recovery_deletion_verified=True,guest_boot_or_recovery_deletion_qualified=True,
                       intentional_prepared_fixture_retained_in_retired_overlay=False)
        check("pre_boot_preparation_guest_verified_and_recovery_explicitly_removed")
    plan = invoke("erin", {"action":"plan_vm_shrink","vm_id":vm_id,"target_disk_mib":target_mib})
    lab.atomic_json(evidence / "shrink-plan.json", plan)
    if plan["expected_disk_mib"] != source_mib or not plan["recovery_copy_retained"]:
        raise RuntimeError("shrink plan does not match the admitted original")
    request = {"action":"shrink_vm","request_id":"real-disk-shrink","vm_id":vm_id,"expected_disk_mib":source_mib,
               "target_disk_mib":target_mib,"confirm_downtime":True}
    prepared = invoke("erin", request)
    operation("erin", prepared)
    if invoke("erin", request)["operation_id"] != prepared["operation_id"]:
        raise RuntimeError("shrink retry changed operation identity")
    dashboard = invoke("erin", {"action":"dashboard"})
    hold = next(s for s in dashboard["disk_shrinks"] if s["vm_id"] == vm_id)
    if not hold["prepared"] or hold["operation_id"] != prepared["operation_id"]:
        raise RuntimeError("prepared shrink recovery/capacity hold missing")
    owner = next(u for u in dashboard["users"] if u["user_id"] == "labdev")
    policy = {k:owner[k] for k in ("budget_mib","min_memory_budget_mib","max_memory_budget_mib","max_cores","max_vms")}
    policy["budget_mib"] = final_budget_mib
    update = {"action":"update_user_policy","request_id":"shrink-owner-contraction","user_id":"labdev",
              "expected_revision":owner.get("policy_revision",0),"policy":policy}
    invoke("erin", update, "conflict")
    invoke("erin", {"action":"vm_action","request_id":"shrink-delete-before-finish","vm_id":vm_id,"kind":"delete"}, "conflict")
    lab.atomic_json(evidence / "shrink-prepared-dashboard.json", dashboard)
    check("real_offline_ext4_GPT_and_zvol_shrink_with_retained_original")
    # New service processes must read schema5 and retain the exact prepared root
    # identity and capacity hold, without treating a journal as fresh state.
    remote("systemctl restart pve-dev-host.service pve-dev-controller.service", text=True, timeout=180)
    deadline = time.monotonic() + 180
    while True:
        try:
            current = invoke("erin", {"action":"dashboard"})
            if current.get("disk_shrinks") != dashboard["disk_shrinks"]:
                raise RuntimeError("service restart lost the recovery hold")
            break
        except (RuntimeError,subprocess.TimeoutExpired):
            if time.monotonic() >= deadline:
                raise
            time.sleep(3)
    check("paired_service_restart_preserves_prepared_shrink")
    for name in ("guest-serial.log", "guest-serial-errors.log"):
        if (evidence/name).exists():
            (evidence/name).rename(evidence/("shrink-source-"+name))
    operation("erin", invoke("erin", {"action":"vm_action","request_id":"shrink-replacement-start","vm_id":vm_id,"kind":"start"}))
    deadline = time.monotonic() + timeout
    with guest_suite.serial_capture(lab, directory, evidence, vm_id, min(timeout+600,7200)) as console:
        while time.monotonic() < deadline:
            guest_suite.check_console(evidence, console)
            try:
                result = subprocess.run(guest_suite.guest_ssh(lab, directory, key, address, "cloud-init status --format json && systemctl is-system-running --wait"),
                                        capture_output=True,text=True,timeout=180)
                if result.returncode == 0:
                    # Parse the first JSON value, then require systemd healthy.
                    decoded, end = json.JSONDecoder().raw_decode(result.stdout.lstrip())
                    if result.stdout.lstrip()[end:].strip() != "running":
                        raise RuntimeError("replacement systemd is not healthy")
                    if guest_suite.cloud_readiness(0, json.dumps(decoded)) is not None:
                        break
                if "REMOTE HOST IDENTIFICATION HAS CHANGED" in result.stderr:
                    raise RuntimeError("guest SSH identity changed")
            except subprocess.TimeoutExpired:
                pass
            time.sleep(10)
        else:
            raise TimeoutError("smaller replacement did not boot healthily")
    after = identity()
    lab.atomic_json(evidence / "shrink-identity-after.json", after)
    if before != after:
        raise RuntimeError("guest machine/cloud-init/SSH/data identity changed")
    disk = subprocess.run(guest_suite.guest_ssh(lab, directory, key, address, "lsblk -b -J -o NAME,TYPE,SIZE,FSTYPE,UUID && df -B1 /"),
                          capture_output=True,text=True,timeout=180)
    disk.check_returncode()
    (evidence / "shrink-guest-storage-after.txt").write_text(disk.stdout)
    check("smaller_guest_boot_cloud_init_SSH_systemd_and_random_sentinel_preserved")
    operation("erin", invoke("erin", {"action":"vm_action","request_id":"shrink-final-clean-shutdown","vm_id":vm_id,"kind":"shutdown"}))
    finished = invoke("erin", {"action":"finish_vm_shrink","request_id":"finish-real-shrink","vm_id":vm_id,
        "preparation_id":prepared["operation_id"],"confirm_guest_verified":True,"confirm_delete_recovery":True})
    operation("erin", finished)
    if invoke("erin", {"action":"dashboard"}).get("disk_shrinks"):
        raise RuntimeError("verified finish retained a capacity hold")
    check("explicit_verified_recovery_deletion_and_workspace_release")
    operation("erin", invoke("erin", update))
    root = f"labpool/control/labdev/vm-{vm_id}-disk-0"
    sizes = remote(f"zfs get -Hp -o property,value quota,reservation labpool/control/labdev; zfs get -Hp -o property,value volsize,refreservation {root}",
                   text=True,timeout=180).stdout
    values = dict(line.split("\t") for line in sizes.splitlines())
    if any(int(values[k]) != final_budget_mib*1024**2 for k in ("quota","reservation")) or int(values["volsize"]) != target_mib*1024**2:
        raise RuntimeError("owner contraction or smaller disk enforcement mismatch")
    (evidence / "shrink-final-storage.txt").write_text(sizes)
    check("post_shrink_unused_quota_contraction_releases_real_reservation")
    summary["disk_shrink_verified"] = True
    summary.update(source_disk_mib=source_mib,target_disk_mib=target_mib,final_owner_budget_mib=final_budget_mib)


def interruption(lab, directory, evidence, invoke, operation, remote, check, timeout, image_sha256, summary):
    """One observed real copy effect with its acknowledgement deliberately lost.

    All interposition is confined to the fresh outer overlay. We restore the
    original executable byte-for-byte. The blocked test resources stay in that
    retired overlay for inspection; they are never passed off as normal cleanup.
    """
    dashboard = invoke("erin", {"action":"dashboard"})
    owner = next(u for u in dashboard["users"] if u["user_id"] == "labdev")
    policy = {k:owner[k] for k in ("budget_mib","min_memory_budget_mib","max_memory_budget_mib","max_cores","max_vms")}
    policy["budget_mib"] = 6144
    operation("erin", invoke("erin", {"action":"update_user_policy","request_id":"crash-fixture-budget","user_id":"labdev",
        "expected_revision":owner.get("policy_revision",0),"policy":policy}))
    public = " ".join((directory/"clients/guest-ssh.pub").read_text().split()[:2])
    created = invoke("erin", {"action":"create_vm","request_id":"crash-fixture-guest","name":"dev-shrink-crash","owner":"labdev",
        "disk_mib":4096,"memory_mib":1024,"min_memory_mib":512,"cores":4,
        "guest":{"image_sha256":image_sha256,"ssh_public_key":public}})
    operation("erin", created)
    vmid = created["vm_id"]
    source = f"labpool/control/labdev/vm-{vmid}-disk-0"
    before_script = '''import hashlib,json,pathlib,subprocess,sys
name=sys.argv[1]
guid=subprocess.check_output(["zfs","get","-Hp","-o","value","guid",name],text=True).strip()
with open(pathlib.Path("/dev/zvol")/name,"rb") as data: digest=hashlib.file_digest(data,"sha256").hexdigest()
print(json.dumps({"guid":guid,"sha256":digest}))'''
    before = json.loads(remote("python3 - "+shlex.quote(source),input=before_script,text=True,timeout=600).stdout)
    remote("systemctl kill --signal=STOP pve-dev-host.service",text=True,timeout=60)
    try:
        request={"action":"shrink_vm","request_id":"real-copy-lost-ack","vm_id":vmid,"expected_disk_mib":4096,
                 "target_disk_mib":3072,"confirm_downtime":True}
        accepted=invoke("erin",request)
        prep=accepted["operation_id"]
        environment=summary["environment"]
        replacement=f"labpool/pve-dev-shrink-{prep}/replacement"
        wrapper='''#!/usr/bin/python3
import json,os,pathlib,signal,subprocess,sys
env=ENVIRONMENT
if pathlib.Path("/etc/pve-dev-lab-id").read_text().strip()!=env:raise RuntimeError("wrong disposable lab")
source=str((pathlib.Path("/dev/zvol")/SOURCE).resolve())
target=str((pathlib.Path("/dev/zvol")/TARGET).resolve())
expected=["convert","-n","-f","raw","-O","raw",source,target]
if sys.argv[1:]!=expected:os.execv("/usr/bin/qemu-img.pve-dev-shrink-real",["/usr/bin/qemu-img.pve-dev-shrink-real",*sys.argv[1:]])
result=subprocess.run(["/usr/bin/qemu-img.pve-dev-shrink-real",*sys.argv[1:]])
if result.returncode:sys.exit(result.returncode)
marker=pathlib.Path("/run/pve-dev-shrink-copy-effect")
if marker.exists():raise RuntimeError("copy effect was replayed")
marker.write_text(json.dumps({"environment":env,"pid":os.getpid(),"effect_completed":True}))
os.kill(os.getpid(),signal.SIGSTOP)
sys.exit(0)
'''.replace("ENVIRONMENT",repr(environment)).replace("SOURCE",repr(source)).replace("TARGET",repr(replacement))
        install='''import hashlib,json,os,pathlib,socket
assert socket.gethostname()=="pve-lab" and pathlib.Path("/etc/pve-dev-lab-id").read_text().strip()==ENVIRONMENT
original=pathlib.Path("/usr/bin/qemu-img"); saved=pathlib.Path("/usr/bin/qemu-img.pve-dev-shrink-real")
assert original.is_file() and not original.is_symlink() and not saved.exists()
assert not pathlib.Path("/run/pve-dev-shrink-copy-effect").exists()
digest=hashlib.sha256(original.read_bytes()).hexdigest()
os.rename(original,saved); original.write_text(WRAPPER); original.chmod(0o755)
print(json.dumps({"original_sha256":digest}))
'''.replace("ENVIRONMENT",repr(environment)).replace("WRAPPER",repr(wrapper))
        installed=json.loads(remote("python3 -",input=install,text=True,timeout=60).stdout)
    finally:
        remote("systemctl kill --signal=CONT pve-dev-host.service",text=True,timeout=60)
    try:
        marker_script=f'''import json,pathlib,time
deadline=time.monotonic()+{timeout}
while time.monotonic()<deadline:
 p=pathlib.Path("/run/pve-dev-shrink-copy-effect")
 if p.exists():
  value=json.loads(p.read_text());assert value["environment"]=={environment!r} and value["effect_completed"]
  print(json.dumps(value));break
 time.sleep(1)
else:raise TimeoutError("copy effect marker deadline expired")
'''
        marker=json.loads(remote("python3 -",input=marker_script,text=True,timeout=timeout+30).stdout)
        pid=remote("systemctl show -p MainPID --value pve-dev-host.service",text=True,timeout=60).stdout.strip()
        remote("systemctl kill --kill-whom=main --signal=KILL pve-dev-host.service",text=True,timeout=60)
        deadline=time.monotonic()+180
        while time.monotonic()<deadline:
            result=invoke("erin",{"action":"get_operation","operation_id":prep})
            if result["status"]=="blocked":break
            time.sleep(3)
        else:raise TimeoutError("interrupted shrink did not block after helper restart")
        if "ambiguous shrink intent" not in result.get("error",""):
            raise RuntimeError(f"interruption blocked for an unexpected reason: {result}")
        after=json.loads(remote("python3 - "+shlex.quote(source),input=before_script,text=True,timeout=600).stdout)
        if before!=after:raise RuntimeError("original GUID or bytes changed during actual interruption")
        new_pid=remote("systemctl show -p MainPID --value pve-dev-host.service",text=True,timeout=60).stdout.strip()
        if not new_pid or new_pid==pid:raise RuntimeError("fresh helper process not established")
        provenance=json.loads(remote("/usr/local/sbin/pve-dev-host provenance --socket /run/pve-dev-host/control.sock",text=True,timeout=60).stdout)
        if provenance["resources"]["unsettled_jobs"].get(prep)!="blocked":raise RuntimeError("helper did not retain unsettled operation")
        if invoke("erin",request)["operation_id"]!=prep:raise RuntimeError("lost-ack retry changed identity")
        fenced=lab.ssh(directory,f"qm start {vmid}",capture_output=True,text=True,timeout=60)
        if fenced.returncode==0 or "locked" not in (fenced.stdout+fenced.stderr).lower():raise RuntimeError("ordinary start did not refuse fenced original")
        # The controlled command's process group must be retired, not merely
        # hidden by a restarted systemd MainPID or a disconnected client.
        absent=f'''import os,pathlib,time
pid={marker['pid']}
deadline=time.monotonic()+30
while pathlib.Path(f"/proc/{{pid}}").exists() and time.monotonic()<deadline:time.sleep(.2)
assert not pathlib.Path(f"/proc/{{pid}}").exists(),"copy process remains after helper restart"
print("owned copy process reaped")
'''
        remote("python3 -",input=absent,text=True,timeout=45)
        lab.atomic_json(evidence/"shrink-real-copy-interruption.json",{"complete":True,"effect_completed_before_ack":True,
            "source_before":before,"source_after":after,"operation":result,"same_request_retry":True,
            "original_start_refused":True,"owned_copy_process_reaped":True,"helper_pid_changed":True})
        lab.atomic_json(evidence/"shrink-interrupted-provenance.json",provenance)
        summary["real_shrink_copy_interruption_verified"]=True
        summary["intentional_blocked_fixture_retained_in_retired_overlay"]=True
        check("real_copy_effect_helper_SIGKILL_blocks_without_replay_and_preserves_original")
    finally:
        restore='''import hashlib,os,pathlib
original=pathlib.Path("/usr/bin/qemu-img"); saved=pathlib.Path("/usr/bin/qemu-img.pve-dev-shrink-real")
assert hashlib.sha256(saved.read_bytes()).hexdigest()==DIGEST
os.replace(saved,original)
assert hashlib.sha256(original.read_bytes()).hexdigest()==DIGEST
print("original qemu-img restored byte-for-byte")
'''.replace("DIGEST",repr(installed["original_sha256"]))
        remote("python3 -",input=restore,text=True,timeout=60)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-bin-dir", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument("--prepare-only", action="store_true", help="qualify actual preparation without guest boot or recovery deletion")
    parser.add_argument("--lock-fd", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--session-anchor", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--status-path", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    import control_suite
    import physical_trial_suite
    import qualification_matrix as ownership
    physical_trial_suite.cached_base()  # Never rebuild or replace the sealed base.
    argv = [sys.executable,str(Path(__file__).resolve()),"--linux-bin-dir",str(args.linux_bin_dir.resolve()),"--timeout",str(args.timeout)]
    if args.prepare_only:argv.append("--prepare-only")
    if args.lock_fd is not None:
        ownership.validate_inherited_lock(args.lock_fd,lab.HOME/"lab.lock")
        if args.session_anchor:
            if args.status_path is None:
                parser.error("anchor status path required")
            ownership.session_anchor(args.lock_fd,args.status_path,argv+["--lock-fd",str(args.lock_fd)])
        try:
            control_suite.run(lab,args.timeout,args.linux_bin_dir,bootable=True,shrink=True,shrink_prepare_only=args.prepare_only)
        finally:
            os.close(args.lock_fd)
        return
    if args.session_anchor or args.status_path is not None:
        parser.error("internal ownership options require an inherited lease")
    lab.HOME.mkdir(mode=0o700, exist_ok=True)
    with (lab.HOME/"lab.lock").open("a") as lease:
        fcntl.flock(lease,fcntl.LOCK_EX|fcntl.LOCK_NB)
        ownership.process_table()  # Refuse before starting guests if inspection is denied.
        directory=lab.HOME/("disk-shrink-qualification-"+time.strftime("%Y%m%dT%H%M%S")+"-"+secrets.token_hex(3))
        directory.mkdir(mode=0o700)
        summary={"schema":1,"complete":False,"production_contacted":False,"prepare_only":args.prepare_only,"single_global_lock_inode":os.fstat(lease.fileno()).st_ino}
        status=directory/"status.json"
        sessions=[]
        print("Owned shrink qualification:",directory,flush=True)
        with (directory/"worker.log").open("wb") as logfile:
            try:
                session=ownership.spawn_registered("shrink",argv+["--lock-fd",str(lease.fileno()),"--session-anchor","--status-path",str(status)],status,lease.fileno(),logfile,sessions)
                deadline=time.monotonic()+args.timeout*4
                while not session.finished(ownership.process_table()):
                    if time.monotonic()>deadline:
                        raise TimeoutError("bounded shrink fixture deadline expired")
                    time.sleep(1)
                summary["exit_code"]=session.status().get("exit_code",1)
                summary["complete"]=summary["exit_code"]==0
            finally:
                with ownership.defer_cancellation():
                    ownership.retire_sessions(sessions)
                    summary["owned_groups_retired"]=all(s.retired for s in sessions)
                    summary["forced_descendant_cleanup"]=any(s.forced_cleanup for s in sessions)
                    summary["complete"] &= summary["owned_groups_retired"] and not summary["forced_descendant_cleanup"]
                    lab.atomic_json(directory/"summary.json",summary)
        print(json.dumps(summary),flush=True)
        if not summary["complete"]:
            raise SystemExit(1)


if __name__ == "__main__":
    def interrupted(signum,frame): raise KeyboardInterrupt
    signal.signal(signal.SIGTERM,interrupted)
    main()
