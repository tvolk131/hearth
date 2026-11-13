import hashlib
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import time


LAB_ID = pathlib.Path("/etc/pve-dev-lab-id")
IMAGE_ROOT = pathlib.Path("/var/lib/pve-dev-images")
DROP_IN = "/etc/systemd/system.conf.d/90-pve-dev-lab.conf"
TIMEOUT_CONFIG = b"[Manager]\nDefaultTimeoutStartSec=600s\nDefaultDeviceTimeoutSec=600s\n"
CLOUD_INIT_DROP_IN = "/etc/cloud/cloud.cfg.d/90-pve-dev-lab.cfg"
CLOUD_INIT_CONFIG = b"system_info:\n  default_user:\n    name: dev\n"
RECIPE = "pve-dev-lab-boot-policy/2"


def run(*arguments):
    result = subprocess.run(arguments, check=True, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=180,
                            env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C"})
    return result.stdout


def binding(environment, pool_guid):
    if not re.fullmatch(r"[0-9a-f]{32}", environment):
        raise ValueError("invalid disposable environment nonce")
    if not re.fullmatch(r"[1-9][0-9]{0,19}", pool_guid) or int(pool_guid) >= 2**64:
        raise ValueError("invalid lab pool GUID")
    if socket.gethostname() != "pve-lab" or LAB_ID.read_text().strip() != environment:
        raise ValueError("image preparation requires the exact disposable lab identity")
    if run("zpool", "get", "-Hp", "-o", "value", "guid", "labpool").strip() != pool_guid:
        raise ValueError("lab pool identity changed")


def image_info(info):
    specific = info.get("format-specific", {}).get("data", {})
    if (info.get("format") != "qcow2" or info.get("virtual-size") != 3 * 1024**3
            or info.get("backing-filename") or info.get("full-backing-filename")
            or info.get("encrypted") or "encrypt" in specific or specific.get("corrupt")
            or specific.get("data-file") or specific.get("data-file-raw")):
        raise ValueError("the pinned parent must be a self-contained 3 GiB qcow2")


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def prepare(environment, pool_guid, parent_sha256):
    binding(environment, pool_guid)
    if not re.fullmatch(r"[0-9a-f]{64}", parent_sha256):
        raise ValueError("invalid pinned parent digest")
    parent = IMAGE_ROOT / f"{parent_sha256}.qcow2"
    metadata = parent.stat()
    if (parent.resolve() != parent or metadata.st_uid != 0 or metadata.st_nlink != 1
            or metadata.st_mode & 0o222 or metadata.st_size > 1024**3
            or digest(parent) != parent_sha256):
        raise ValueError("parent image identity or hash is not trusted")
    image_info(json.loads(run("qemu-img", "info", "-f", "qcow2", "--output=json", str(parent))))
    staging = IMAGE_ROOT / f"staging-{environment}.qcow2"
    descriptor = os.open(staging, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as output, parent.open("rb") as source:
        shutil.copyfileobj(source, output)
        output.flush()
        os.fsync(output.fileno())
    configurations = []
    for suffix, target, content in (("systemd", DROP_IN, TIMEOUT_CONFIG),
                                    ("cloud-init", CLOUD_INIT_DROP_IN, CLOUD_INIT_CONFIG)):
        config = pathlib.Path(f"/run/pve-dev-image-{environment}-{suffix}.conf")
        descriptor = os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        configurations.append((config, target, content))
    run("modprobe", "nbd", "max_part=16")
    if (pathlib.Path("/sys/block/nbd0/pid").exists()
            or pathlib.Path("/sys/block/nbd0/size").read_text().strip() != "0"):
        raise ValueError("refusing to use an occupied NBD device")
    connected = False
    try:
        run("qemu-nbd", "--connect=/dev/nbd0", "--format=qcow2", str(staging))
        connected = True
        partition = pathlib.Path("/dev/nbd0p1")
        deadline = time.monotonic() + 90
        while not partition.exists():
            if time.monotonic() >= deadline:
                raise TimeoutError("the private image partition did not appear")
            time.sleep(1)
        if run("blkid", "-p", "-s", "TYPE", "-o", "value", str(partition)).strip() != "ext4":
            raise ValueError("the parent root partition is not ext4")
        run("e2fsck", "-f", "-n", str(partition))
        for config, target, content in configurations:
            directory = str(pathlib.PurePosixPath(target).parent)
            existing = run("debugfs", "-R", f"stat {directory}", str(partition))
            if "Inode:" not in existing:
                run("debugfs", "-w", "-R", f"mkdir {directory}", str(partition))
                existing = run("debugfs", "-R", f"stat {directory}", str(partition))
            if "Type: directory" not in existing:
                raise ValueError("lab drop-in directory was not created")
            if "Inode:" in run("debugfs", "-R", f"stat {target}", str(partition)):
                raise ValueError("the lab drop-in already exists in the pinned parent")
            run("debugfs", "-w", "-R", f"write {config} {target}", str(partition))
            run("debugfs", "-w", "-R", f"set_inode_field {target} mode 0100644", str(partition))
            if run("debugfs", "-R", f"cat {target}", str(partition)).encode() != content:
                raise ValueError("prepared image does not contain the exact boot policy")
        run("e2fsck", "-f", "-n", str(partition))
        run("blockdev", "--flushbufs", "/dev/nbd0")
    finally:
        if connected:
            run("qemu-nbd", "--disconnect", "/dev/nbd0")
    for config, _, _ in configurations:
        config.unlink()
    run("qemu-img", "check", "-f", "qcow2", str(staging))
    image_info(json.loads(run("qemu-img", "info", "-f", "qcow2", "--output=json", str(staging))))
    if staging.stat().st_size > 1024**3 or digest(parent) != parent_sha256:
        raise ValueError("derived image is oversized or the pinned parent changed")
    image_sha256 = digest(staging)
    final = IMAGE_ROOT / f"{image_sha256}.qcow2"
    os.chmod(staging, 0o444)
    with staging.open("rb") as source:
        os.fsync(source.fileno())
    os.link(staging, final)
    staging.unlink()
    directory_fd = os.open(IMAGE_ROOT, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
    return {"schema": "pve-dev-lab-image/1", "recipe": RECIPE,
            "environment": environment, "parent_sha256": parent_sha256,
            "image_sha256": image_sha256, "path": str(final),
            "manager_drop_in": DROP_IN,
            "manager_drop_in_sha256": hashlib.sha256(TIMEOUT_CONFIG).hexdigest(),
            "cloud_init_drop_in": CLOUD_INIT_DROP_IN,
            "cloud_init_drop_in_sha256": hashlib.sha256(CLOUD_INIT_CONFIG).hexdigest(),
            "default_user": "dev",
            "timeout_seconds": 600, "parent_image_unchanged": True}


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit("expected disposable nonce, lab pool GUID and pinned parent SHA-256")
    print(json.dumps(prepare(*sys.argv[1:])))
