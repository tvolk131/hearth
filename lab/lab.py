#!/usr/bin/env python3
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import pathlib
import re
import secrets
import signal
import shutil
import socket
import subprocess
import sys
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parents[1]
HOME = ROOT / ".lab"
INPUTS = json.loads((ROOT / "lab/inputs.json").read_text())
MIN_FREE = int(os.environ.get("PVE_LAB_MIN_FREE_GIB", "10")) * 1024**3
if MIN_FREE < 3 * 1024**3:
    raise RuntimeError("Lab safety floor cannot be less than 3 GiB")


def run(arguments, **options):
    return subprocess.run(arguments, check=True, **options)


def atomic_json(path, value):
    temporary = path.with_suffix(".tmp")
    with temporary.open("w") as output:
        json.dump(value, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    temporary.replace(path)
    descriptor = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def space():
    free = shutil.disk_usage(HOME).free
    if free < MIN_FREE:
        raise RuntimeError(f"Only {free / 1024**3:.1f} GiB free; lab requires a {MIN_FREE / 1024**3:g} GiB safety floor")


def download():
    destination = HOME / "debian.qcow2"
    if not destination.exists():
        space()
        temporary = HOME / "debian.download"
        run(["curl", "--fail", "--location", "--retry", "3", "--max-time", "1800",
             "--output", str(temporary), INPUTS["debian_url"]])
        verify_image(temporary)
        temporary.replace(destination)
    verify_image(destination)
    return destination


def verify_image(path):
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, "sha512").hexdigest()
    if actual != INPUTS["debian_sha512"]:
        raise RuntimeError(f"Debian image checksum mismatch: {path}")


def identity():
    directory = HOME / "identity"
    directory.mkdir(mode=0o700, exist_ok=True)
    for name in ("controller", "host"):
        if not (directory / name).exists():
            run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(directory / name)])
    return directory


def seed(directory, nonce):
    keys = identity()
    service = """[Unit]
After=network-online.target
Wants=network-online.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/pve-lab-bootstrap
TimeoutStartSec=infinity
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
"""
    config = {
        "hostname": "pve-lab",
        "manage_etc_hosts": False,
        "disable_root": False,
        "ssh_pwauth": False,
        "users": [{"name": "root", "lock_passwd": True,
                   "ssh_authorized_keys": [(keys / "controller.pub").read_text().strip()]}],
        "ssh_keys": {"ed25519_private": (keys / "host").read_text(),
                     "ed25519_public": (keys / "host.pub").read_text().strip()},
        "write_files": [
            {"path": "/etc/pve-dev-lab-id", "content": nonce + "\n", "permissions": "0600"},
            {"path": "/usr/local/sbin/pve-lab-bootstrap", "permissions": "0700",
             "content": (ROOT / "lab/bootstrap.sh").read_text()},
            {"path": "/etc/systemd/system/pve-lab-bootstrap.service", "content": service},
        ],
        "runcmd": [["systemctl", "daemon-reload"],
                    ["systemctl", "enable", "--now", "--no-block", "pve-lab-bootstrap.service"]],
    }
    (directory / "user-data").write_text("#cloud-config\n" + json.dumps(config))
    (directory / "meta-data").write_text(json.dumps({"instance-id": nonce, "local-hostname": "pve-lab"}))
    run(["xorriso", "-as", "mkisofs", "-quiet", "-output", str(directory / "seed.iso"),
         "-volid", "cidata", "-joliet", "-rock", str(directory / "user-data"),
         str(directory / "meta-data")])


def ssh_arguments(directory):
    endpoint = json.loads((directory / "endpoint.json").read_text())
    port = endpoint["port"]
    if not isinstance(port, int) or not 1024 <= port <= 65535:
        raise RuntimeError("invalid lab loopback port")
    return ["ssh", "-F", "/dev/null", "-i", str(identity() / "controller"),
            "-p", str(port), "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no",
            "-o", f"UserKnownHostsFile={directory / 'known_hosts'}",
            "-o", "GlobalKnownHostsFile=/dev/null", "-o", "ConnectTimeout=5",
            "-o", "ConnectionAttempts=1", "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=3", "-o", "ForwardAgent=no",
            "-o", "ClearAllForwardings=yes", "root@127.0.0.1"]


def ssh(directory, command, **options):
    return subprocess.run(ssh_arguments(directory) + [command], **options)


def disable_host_cloud_init(directory):
    result = ssh(directory, "install -m 0644 /dev/null /etc/cloud/cloud-init.disabled", capture_output=True, text=True, timeout=45)
    result.check_returncode()


def qmp(directory, command):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(10)
        connection.connect(json.loads((directory / "qmp.json").read_text())["path"])
        stream = connection.makefile("rwb")
        json.loads(stream.readline())
        for request in ("qmp_capabilities", command):
            stream.write(json.dumps({"execute": request}).encode() + b"\n")
            stream.flush()
            while True:
                response = json.loads(stream.readline())
                if "error" in response:
                    raise RuntimeError(response)
                if "return" in response:
                    break


@contextlib.contextmanager
def boot(directory, nonce, data_disks=False, control_port=False):
    space()
    socket_directory = tempfile.TemporaryDirectory(prefix="pve-dev-qmp-", dir="/tmp")
    socket_path = pathlib.Path(socket_directory.name) / "qmp.sock"
    atomic_json(directory / "qmp.json", {"path": str(socket_path)})
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    host_key = (identity() / "host.pub").read_text().strip()
    (directory / "known_hosts").write_text(f"[127.0.0.1]:{port} {host_key}\n")
    endpoint = {"schema": 1, "port": port, "nonce": nonce}
    network = f"user,id=net0,restrict=off,hostfwd=tcp:127.0.0.1:{port}-:22"
    if control_port:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as reservation:
            reservation.bind(("127.0.0.1", 0))
            endpoint["control_udp_port"] = reservation.getsockname()[1]
        network += f",hostfwd=udp:127.0.0.1:{endpoint['control_udp_port']}-:7777"
    atomic_json(directory / "endpoint.json", endpoint)
    threading = "multi" if control_port else "single"
    command = ["qemu-system-x86_64", "-name", "pve-dev-disposable-lab", "-machine", "q35",
               "-accel", f"tcg,thread={threading}", "-cpu", "max", "-smp", "4", "-m", "4096",
               "-display", "none", "-monitor", "none", "-serial", f"file:{directory / 'serial.log'}",
               "-qmp", f"unix:{socket_path},server=on,wait=off",
               "-drive", f"file={directory / 'root.qcow2'},format=qcow2,if=none,id=rootdisk,discard=unmap",
               "-device", "virtio-blk-pci,drive=rootdisk,bootindex=1",
               "-drive", f"file={directory / 'seed.iso'},format=raw,media=cdrom,readonly=on",
               "-netdev", network,
               "-device", "virtio-net-pci,netdev=net0"]
    if data_disks:
        for letter in ("a", "b"):
            command += ["-drive", f"file={directory / ('data-' + letter + '.qcow2')},format=qcow2,if=none,id=data-{letter},discard=unmap",
                        "-device", f"virtio-blk-pci,drive=data-{letter},serial=lab-data-{letter}"]
    with (directory / "qemu.log").open("a") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        try:
            yield process
        finally:
            if process.poll() is None:
                try:
                    qmp(directory, "system_powerdown")
                    process.wait(timeout=90)
                except (OSError, ValueError, subprocess.TimeoutExpired, RuntimeError):
                    process.terminate()
                    try:
                        process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            if socket_path.exists():
                socket_path.unlink()
            socket_directory.cleanup()


def check_boot_health(directory, process):
    if process.poll() is not None:
        raise RuntimeError(f"QEMU exited: {(directory / 'qemu.log').read_text()}")
    serial = directory / "serial.log"
    if serial.exists():
        with serial.open("rb") as output:
            output.seek(max(0, serial.stat().st_size - 131072))
            if b"Kernel panic - not syncing:" in output.read():
                raise RuntimeError(f"Lab kernel panicked; inspect {serial}; no automatic reset or retry")


def ready(directory, process, timeout, installing=False):
    started = time.monotonic()
    next_report = 0
    repaired_bootstrap = False
    while time.monotonic() - started < timeout:
        space()
        check_boot_health(directory, process)
        try:
            result = ssh(directory, "if test -f /var/lib/pve-dev-lab/failed; then cat /var/lib/pve-dev-lab/failed; exit 40; fi; test -f /var/lib/pve-dev-lab/ready && pvesh get /version --output-format json", stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=45)
        except subprocess.TimeoutExpired:
            check_boot_health(directory, process)
            print("Read-only boot readiness query timed out; continuing within the overall boot deadline", flush=True)
            time.sleep(5)
            continue
        if result.returncode == 40:
            if installing and not repaired_bootstrap:
                installed = ssh(directory, "sha256sum /usr/local/sbin/pve-lab-bootstrap", capture_output=True, text=True, timeout=45)
                expected = hashlib.sha256((ROOT / "lab/bootstrap.sh").read_bytes()).hexdigest()
                if installed.returncode == 0 and installed.stdout.split()[0] != expected:
                    repair = ssh(directory, "cat >/usr/local/sbin/pve-lab-bootstrap && chmod 700 /usr/local/sbin/pve-lab-bootstrap && rm -f /var/lib/pve-dev-lab/failed && systemctl restart --no-block pve-lab-bootstrap.service", input=(ROOT / "lab/bootstrap.sh").read_text(), text=True, timeout=45)
                    repair.check_returncode()
                    repaired_bootstrap = True
                    continue
            log = ssh(directory, "tail -100 /var/log/pve-lab-bootstrap.log", capture_output=True, text=True, timeout=45)
            raise RuntimeError(log.stdout)
        if result.returncode == 0:
            print(f"Real Proxmox API ready after {time.monotonic() - started:.0f}s: {result.stdout.strip()}", flush=True)
            return json.loads(result.stdout)
        elapsed = time.monotonic() - started
        if elapsed >= next_report:
            print(f"Waiting for {'Proxmox installation' if installing else 'Proxmox boot'} ({elapsed:.0f}s); logs: {directory}", flush=True)
            next_report = elapsed + 60
        time.sleep(5)
    raise TimeoutError(f"Proxmox not ready after {timeout}s; see {directory}")


def build(timeout):
    directory = HOME / "base"
    directory.mkdir(mode=0o700, exist_ok=True)
    directory.chmod(0o700)
    inputs = hashlib.sha256((ROOT / "lab/inputs.json").read_bytes() + (ROOT / "lab/bootstrap.sh").read_bytes()).hexdigest()
    stamp = directory / "build.json"
    if stamp.exists():
        if json.loads(stamp.read_text())["inputs_digest"] != inputs:
            raise RuntimeError("Base inputs changed; preserve or remove .lab/base explicitly before rebuilding")
        verify_image(HOME / "debian.qcow2")
        print(f"Using existing locally built base: {directory}", flush=True)
        return
    nonce_file = directory / "nonce"
    if not nonce_file.exists():
        nonce_file.write_text(secrets.token_hex(16))
    nonce = nonce_file.read_text().strip()
    source = download()
    root = directory / "root.qcow2"
    if not root.exists():
        run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(source), str(root), "16G"])
    seed(directory, nonce)
    started = time.monotonic()
    with boot(directory, nonce) as process:
        version = ready(directory, process, timeout, installing=True)
        result = ssh(directory, "pveversion --verbose; dpkg-query -W", capture_output=True, text=True, timeout=120)
        result.check_returncode()
        (directory / "packages.txt").write_text(result.stdout)
    if process.returncode != 0:
        raise RuntimeError("Base did not shut down cleanly; refusing to seal it")
    run(["qemu-img", "check", str(root)])
    atomic_json(stamp, {"schema": 1, "inputs_digest": inputs, "proxmox": version,
                        "build_seconds": round(time.monotonic() - started, 1),
                        "debian": INPUTS, "nonce": nonce})
    print(f"Base built and shut down: {stamp}", flush=True)


def main():
    def interrupted(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description="Disposable, loopback-only real Proxmox lab; never uses production SSH config")
    parser.add_argument("command", choices=["build", "control-test", "guest-test"])
    parser.add_argument("--timeout", type=int, default=5400)
    parser.add_argument("--linux-bin-dir", type=pathlib.Path, help="Directory containing Linux pve-devd and pve-dev-host binaries for control-test")
    arguments = parser.parse_args()
    HOME.mkdir(mode=0o700, exist_ok=True)
    HOME.chmod(0o700)
    with (HOME / "lab.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if arguments.command == "build":
            build(arguments.timeout)
        else:
            import control_suite
            control_suite.run(sys.modules[__name__], arguments.timeout, arguments.linux_bin_dir,
                              bootable=arguments.command == "guest-test")


if __name__ == "__main__":
    main()
