import json
import pathlib
import subprocess
import sys


nonce = sys.argv[1]
if pathlib.Path("/etc/pve-dev-lab-id").read_text().strip() != nonce:
    raise RuntimeError("not the expected disposable lab")
if subprocess.check_output(["hostname"], text=True).strip() != "pve-lab":
    raise RuntimeError("unexpected hostname")
disks = [f"/dev/disk/by-id/virtio-lab-data-{letter}" for letter in ("a", "b")]
for disk in disks:
    if not pathlib.Path(disk).is_block_device():
        raise RuntimeError(f"missing dedicated virtual disk: {disk}")
    signatures = json.loads(subprocess.check_output(["wipefs", "--no-act", "--json", disk], text=True))
    if signatures.get("signatures"):
        raise RuntimeError(f"virtual disk is not blank; refusing overwrite: {disk}")
subprocess.run(["zpool", "create", "-o", "ashift=12", "-o", "cachefile=/etc/zfs/zpool.cache", "-O", "compression=lz4", "-O", "mountpoint=none",
                "-O", f"org.pve-dev:lab={nonce}", "labpool", "mirror", *disks], check=True)
subprocess.run(["zfs", "create", "labpool/people"], check=True)
print(subprocess.check_output(["zpool", "status", "labpool"], text=True))
