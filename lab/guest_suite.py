import json
import ipaddress
import contextlib
import shlex
import subprocess
import time

GUEST_CORES = 4
GUEST_SSH_TIMEOUT = 180


def serial_capture_script(vm_id, timeout):
    if type(vm_id) is not int or not 9001 <= vm_id <= 9099:
        raise ValueError("Serial capture is restricted to disposable lab VMIDs")
    if type(timeout) is not int or not 1 <= timeout <= 7200:
        raise ValueError("Serial capture requires a bounded timeout")
    return f"""import socket,sys,time
connection=socket.socket(socket.AF_UNIX)
connection.settimeout(5)
deadline=time.monotonic()+{timeout}
while True:
 try:
  connection.connect('/var/run/qemu-server/{vm_id}.serial0')
  break
 except (FileNotFoundError,ConnectionRefusedError):
  if time.monotonic()>=deadline:raise RuntimeError('serial socket did not appear')
  time.sleep(1)
total=0
while time.monotonic()<deadline:
 try:
  data=connection.recv(65536)
  if not data:raise RuntimeError('serial console disconnected')
  total+=len(data)
  if total>2097152:raise RuntimeError('serial evidence exceeded 2 MiB')
  sys.stdout.buffer.write(data);sys.stdout.buffer.flush()
 except socket.timeout:pass
"""


@contextlib.contextmanager
def serial_capture(lab, directory, evidence, vm_id, timeout):
    script = serial_capture_script(vm_id, timeout)
    with (evidence / "guest-serial.log").open("wb") as output, \
            (evidence / "guest-serial-errors.log").open("wb") as errors:
        process = subprocess.Popen(lab.ssh_arguments(directory) + ["python3 -I -"],
                                   stdin=subprocess.PIPE, stdout=output, stderr=errors)
        try:
            process.stdin.write(script.encode())
            process.stdin.close()
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def check_console(evidence, process):
    path = evidence / "guest-serial.log"
    with path.open("rb") as output:
        output.seek(max(0, path.stat().st_size - 131072))
        captured = output.read()
    failures = (b"Kernel panic - not syncing:", b"(initramfs)", b"Gave up waiting for root file system",
                b"You are in emergency mode.")
    if any(failure in captured for failure in failures):
        raise RuntimeError(f"Guest boot failed; inspect {path}; no automatic reboot or retry")
    if process.poll() is not None:
        raise RuntimeError("Guest serial collector exited unexpectedly; inspect guest-serial-errors.log")


def guest_ssh(lab, directory, key, address, command, first_contact=False):
    parsed = ipaddress.IPv4Address(address)
    if parsed not in ipaddress.IPv4Network("10.77.0.0/24") or not 11 <= int(str(parsed).split(".")[-1]) <= 109:
        raise ValueError("Guest readiness SSH is restricted to the disposable lab subnet")
    outer = lab.ssh_arguments(directory)
    proxy = shlex.join(outer[:-1] + ["-W", f"{address}:22", outer[-1]])
    return ["ssh", "-F", "/dev/null", "-i", str(key),
            "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
            "-o", "ForwardAgent=no", "-o", "GlobalKnownHostsFile=/dev/null",
            "-o", f"UserKnownHostsFile={directory / 'guest_known_hosts'}",
            "-o", f"StrictHostKeyChecking={'accept-new' if first_contact else 'yes'}",
            "-o", "UpdateHostKeys=no", "-o", "ConnectTimeout=30",
            "-o", "ConnectionAttempts=1", "-o", f"ProxyCommand={proxy}",
            f"dev@{address}", command]


def run(lab, directory, evidence, invoke, operation, remote, timeout,
        image_sha256, summary, check, shrink=False, prepare_only=False):
    summary["guest_vcpus"] = GUEST_CORES
    key = directory / "clients/guest-ssh"
    wrong_key = directory / "clients/wrong-guest-ssh"
    for path in (key, wrong_key):
        lab.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(path)])
    public_key = " ".join(key.with_suffix(".pub").read_text().split()[:2])
    owner = invoke("erin", {"action": "create_user", "request_id": "guest-owner",
                            "user_id": "labdev", "budget_mib": 5120,
                            "min_memory_budget_mib": 512, "max_memory_budget_mib": 1024,
                            "max_cores": GUEST_CORES, "max_vms": 1})
    operation("erin", owner)
    request = {"action": "create_vm", "request_id": "bootable-guest", "name": "dev-lab",
               "owner": "labdev", "disk_mib": 4096, "memory_mib": 1024,
               "min_memory_mib": 512, "cores": GUEST_CORES,
               "guest": {"image_sha256": image_sha256, "ssh_public_key": public_key}}
    created = invoke("erin", request)
    operation("erin", created)
    if invoke("erin", request)["operation_id"] != created["operation_id"]:
        raise RuntimeError("Guest creation retry did not return the durable original operation")
    vm_id = created["vm_id"]
    address = f"10.77.0.{vm_id - 9000 + 10}"
    prefix = "labpool/control/labdev"
    properties = remote(f"zfs get -Hp -o property,value quota,reservation {prefix}; "
                        f"zfs get -Hp -o property,value volsize,refreservation,org.pve-dev:image {prefix}/vm-{vm_id}-disk-0",
                        text=True, timeout=90).stdout
    values = dict(line.split("\t") for line in properties.splitlines())
    if any(int(values[name]) != 5120 * 1024**2 for name in ("quota", "reservation")):
        raise RuntimeError("Guest owner reservation or hard quota differs from admission policy")
    if int(values["volsize"]) != 4096 * 1024**2 or int(values["refreservation"]) < 4096 * 1024**2:
        raise RuntimeError("Bootable root disk is not thick-backed at its admitted size")
    if values["org.pve-dev:image"] != image_sha256:
        raise RuntimeError("Root image provenance digest mismatch")
    (evidence / "guest-storage.txt").write_text(properties)
    compared = remote(f"qemu-img compare -f qcow2 -F raw /var/lib/pve-dev-images/{image_sha256}.qcow2 /dev/zvol/{prefix}/vm-{vm_id}-disk-0", text=True, timeout=600)
    (evidence / "guest-image-comparison.txt").write_text(compared.stdout)
    check("pinned_guest_image_import_and_thick_reservation")
    if shrink:
        import disk_shrink_suite
        disk_shrink_suite.layout_preflight(lab,directory,evidence,remote,check,vm_id)
        disk_shrink_suite.preparation_only(lab,directory,evidence,invoke,operation,remote,check,vm_id,summary)
        if prepare_only:
            return
    with serial_capture(lab, directory, evidence, vm_id, min(timeout + 600, 7200)) as console:
        started = invoke("erin", {"action": "vm_action", "request_id": "guest-start",
                                    "vm_id": vm_id, "kind": "start"})
        operation("erin", started)
        deadline = time.monotonic() + timeout
        last_error = "guest has not answered"
        while time.monotonic() < deadline:
            check_console(evidence, console)
            try:
                response = subprocess.run(guest_ssh(lab, directory, key, address,
                    'test "$(id -un)" = dev && '
                    'test -w "$HOME" && cloud-init status --format json', first_contact=True),
                    capture_output=True, text=True,
                    timeout=min(GUEST_SSH_TIMEOUT, max(1, deadline - time.monotonic())))
                check_console(evidence, console)
                readiness = cloud_readiness(response.returncode, response.stdout)
                if readiness is not None:
                    break
                last_error = response.stderr[-2000:]
                if "REMOTE HOST IDENTIFICATION HAS CHANGED" in last_error:
                    raise RuntimeError("Disposable guest host key unexpectedly changed")
            except subprocess.TimeoutExpired:
                last_error = "read-only guest readiness SSH timed out"
            print(f"Waiting for nested guest OS/SSH at {address}: {last_error[-160:]}", flush=True)
            time.sleep(15)
        else:
            raise TimeoutError(f"Guest OS/SSH readiness timed out: {last_error}")
    rejected = subprocess.run(guest_ssh(lab, directory, wrong_key, address, "true"),
                              capture_output=True, text=True, timeout=GUEST_SSH_TIMEOUT)
    if rejected.returncode != 255 or "Permission denied" not in rejected.stderr:
        raise RuntimeError("An unenrolled SSH key was not unambiguously refused")
    details = subprocess.run(guest_ssh(lab, directory, key, address,
        'test -f /var/lib/cloud/instance/boot-finished && id && hostname && findmnt -no SOURCE,FSTYPE / && df -B1 / && '
        'test "$(sudo -n id -u)" = 0 && printf "owner-sudo-ok\\n" && '
        'systemctl --failed --no-legend --no-pager && systemctl is-system-running --wait'),
        capture_output=True, text=True, timeout=GUEST_SSH_TIMEOUT)
    (evidence / "guest-health.txt").write_text(details.stdout + details.stderr)
    details.check_returncode()
    (evidence / "guest-readiness.json").write_text(json.dumps({
        "vm_id": vm_id, "address": address, "cloud_init": readiness,
        "owner_ssh_verified": True, "foreign_ssh_key_refused": True,
        "systemd_healthy": True,
        "host_key_trust": "lab-only TOFU over pinned outer SSH connection",
        "details": details.stdout}, indent=2) + "\n")
    summary["nested_guest_boot_verified"] = True
    check("real_guest_os_cloud_init_owner_ssh_and_foreign_key_refusal")
    if shrink:
        import disk_shrink_suite
        disk_shrink_suite.exercise(lab, directory, evidence, invoke, operation, remote, check, key, address, vm_id, timeout, summary,
            source_mib=2560,target_mib=2048,final_budget_mib=3072,previous_preparation=summary["initial_preparation_operation_id"])
    operation("erin", invoke("erin", {"action": "vm_action", "request_id": "guest-stop",
                                        "vm_id": vm_id, "kind": "stop"}))
    operation("erin", invoke("erin", {"action": "vm_action", "request_id": "guest-delete",
                                        "vm_id": vm_id, "kind": "delete"}))
    if any(vm["vm_id"] == vm_id and vm["state"] != "deleted" for vm in invoke("erin", {"action": "list_vms"})):
        raise RuntimeError("Deleted guest still owns a live controller allocation")
    volumes = remote("zfs list -H -t volume -o name", text=True, timeout=90).stdout.splitlines()
    if any(volume.startswith(prefix + "/") for volume in volumes):
        raise RuntimeError("Guest deletion left a root or cloud-init zvol behind")
    check("bootable_guest_verified_delete_releases_all_owned_volumes")


def cloud_readiness(returncode, stdout):
    if not stdout.strip():
        return None
    try:
        report = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError("Guest returned non-JSON cloud-init status") from error
    status = report.get("status")
    extended = str(report.get("extended_status", "")).lower()
    if returncode in (1, 2) or status in ("error", "degraded", "disabled") or "degraded" in extended or report.get("errors"):
        raise RuntimeError(f"Cloud-init failed rather than becoming ready (exit {returncode}): {report}")
    if returncode == 0 and status == "done":
        return report
    if status == "running":
        return None
    raise RuntimeError(f"Unexpected cloud-init readiness response (exit {returncode}): {report}")
