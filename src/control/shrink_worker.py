"""Fixed helper adapter: disposable lab only, no caller-supplied paths/commands.

Rust journals intent before invoking each phase. An interrupted phase is NEVER
replayed automatically. Stage changes only a fresh replacement's filesystem;
swap retains the original disk; finish alone authorizes its exact destruction.
"""
import ctypes
from fractions import Fraction
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import struct
import subprocess
import sys
import time
import zlib

MIB = 1024**2
PROPS = ("type", "guid", "createtxg", "volsize", "refreservation", "origin",
         "org.pve-dev:owner", "org.pve-dev:image", "org.pve-dev:shrink", "readonly")
child = None
ROOT_TYPES = {"4f68bce3-e8cd-4db1-96e7-fbcaf984b709"}
DATA_TYPE = "0fc63daf-8483-4772-8e79-3d69d8477de4"
EFI_TYPE = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
BIOS_TYPE = "21686148-6449-6e6f-744e-656564454649"
RECEIPT_ROOT = Path("/var/lib/pve-dev-resize")
RECEIPT_LIMIT = 1024**2
DISK_TOOL_TIMEOUT = 1200


def disk_tool_timeout(old_mib, target_mib):
    require(type(old_mib) is int and type(target_mib) is int and 0 < min(old_mib,target_mib)
            and max(old_mib,target_mib)<=16*1024**2,"invalid admitted timeout size")
    if max(old_mib,target_mib)<=8192:return 1200
    return min(72*3600,max(1200,(2*(old_mib+target_mib)+63)//64+1800))


def require(condition, reason):
    if not condition:
        raise RuntimeError(reason)


def stop_child(signum, _frame):
    if child is not None and child.poll() is None:
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
    raise RuntimeError(f"shrink adapter interrupted by signal {signum}; inspect retained state")


def run(*args, data=None, allowed=(0,), timeout=None):
    global child
    if timeout is None:
        disk_tool=args[0] in ("dd","cmp","e2fsck","resize2fs") or args[:2]==("zpool","sync") \
            or (len(args)>1 and args[0]=="qemu-img" and args[1] in ("convert","compare"))
        timeout=DISK_TOOL_TIMEOUT if disk_tool else 1200
    child = subprocess.Popen(args, stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                             start_new_session=True, env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C"})
    try:
        stdout, stderr = child.communicate(data, timeout=timeout)
    except subprocess.TimeoutExpired:
        stop_child(signal.SIGALRM, None)
    require(child.returncode in allowed, f"{args[0]} failed ({child.returncode}): {stderr[-1500:]}; {stdout[-1000:]}")
    child = None
    return stdout.strip()


def api(method, path, **args):
    argv = ["pvesh", method, path]
    for key, value in args.items():
        argv += ["--" + key.replace("_", "-"), str(value)]
    result = run(*argv, "--output-format", "json")
    if method == "set":
        # PVE can print a successful human-readable "update VM ..." line.
        # Setter exit status is authoritative; subsequent reads prove effects.
        return None
    return json.loads(result) if result else None


def attachment_size(attachment):
    fields = [part.removeprefix("size=") for part in attachment.split(",") if part.startswith("size=")]
    require(len(fields) == 1 and len(fields[0]) <= 32, "root attachment must report exactly one bounded size")
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([KMGT]?)", fields[0])
    require(match is not None, "invalid root attachment size")
    return Fraction(match[1]) * 1024**("KMGT".index(match[2])+1 if match[2] else 0)


def props(name):
    rows = run("zfs", "get", "-Hp", "-o", "property,value", ",".join(PROPS), name)
    return dict(line.split("\t", 1) for line in rows.splitlines())


def same_volume(name, expected, size):
    current = props(name)
    require(current == expected, f"{name} identity or properties changed")
    require(current["type"] == "volume" and int(current["volsize"]) == size
            and current["origin"] == "-" and int(current["refreservation"]) >= size,
            "volume is not independent and thickly backed at the expected size")
    return current


def names(name):
    return run("zfs", "list", "-H", "-r", "-t", "filesystem,volume,snapshot,bookmark", "-o", "name", name).splitlines()


def device(name):
    run("udevadm", "settle")
    path = Path("/dev/zvol") / name
    for _ in range(100):
        if path.exists():
            resolved = path.resolve(strict=True)
            require(re.fullmatch(r"zd[0-9]+", resolved.name) and stat.S_ISBLK(resolved.stat().st_mode), "non-zvol device refused")
            return str(resolved)
        time.sleep(.1)
    raise RuntimeError("expected zvol device did not appear")


def unused(disk):
    def visit(item):
        require(not any(item.get("mountpoints") or []), "disk or partition is mounted")
        require(not list((Path("/sys/class/block") / Path(item["name"]).name / "holders").iterdir()), "disk has an active holder")
        for sub in item.get("children", []):
            visit(sub)
    for item in json.loads(run("lsblk", "-J", "-p", "-o", "NAME,MOUNTPOINTS", disk))["blockdevices"]:
        visit(item)


def sha(disk):
    unused(disk)
    digest = hashlib.sha256()
    with open(disk, "rb", buffering=0) as stream:
        for block in iter(lambda: stream.read(4*MIB), b""):
            digest.update(block)
    return digest.hexdigest()


def layout(disk, root_partition_uuid=None):
    unused(disk)
    value = json.loads(run("sfdisk", "--json", disk))["partitiontable"]
    require(value["label"] == "gpt" and value["sectorsize"] == 512, "only GPT with 512-byte sectors is supported")
    partitions = value["partitions"]
    require(1 <= len(partitions) <= 4, "unsupported partition count")
    allowed = ROOT_TYPES | {DATA_TYPE, EFI_TYPE, BIOS_TYPE}
    unsupported = sorted({p["type"].lower() for p in partitions} - allowed)
    require(not unsupported, "unsupported GPT partition type(s): " + ", ".join(unsupported))
    ext4 = []
    filesystems = []
    normalized = []
    uuids = set()
    for p in partitions:
        partition_number(disk, p["node"])
        require(isinstance(p.get("uuid"), str) and 1 <= len(p["uuid"]) <= 64 and p["uuid"].lower() not in uuids,
                "missing or duplicate partition identity")
        uuids.add(p["uuid"].lower())
        require(type(p["start"]) is int and type(p["size"]) is int and p["start"] >= 34 and p["size"] > 0,
                "invalid partition extent")
        fs = run("blkid", "-p", "-s", "TYPE", "-o", "value", p["node"], allowed=(0, 2))
        require(fs in ("", "vfat", "ext4"), "unsupported filesystem")
        require((p["type"].lower() in ROOT_TYPES | {DATA_TYPE} and fs == "ext4")
                or (p["type"].lower() == EFI_TYPE and fs == "vfat")
                or (p["type"].lower() == BIOS_TYPE and fs == ""), "partition type/filesystem mismatch")
        item = {k: v for k, v in p.items() if k != "node"}
        normalized.append(item)
        if fs == "ext4":
            ext4.append(p)
        if fs:
            fs_uuid = run("blkid", "-s", "UUID", "-o", "value", p["node"])
            require(bool(fs_uuid) and len(fs_uuid) <= 64, "filesystem lacks a bounded identity")
            filesystems.append({"partition_uuid":p["uuid"], "type":fs, "uuid":fs_uuid})
    require(1 <= len(ext4) <= 2, "requires ext4 root and at most one separate ext4 boot partition")
    typed = [p for p in ext4 if p["type"].lower() in ROOT_TYPES]
    require(len(typed) <= 1, "multiple Linux-root partition types are ambiguous")
    if root_partition_uuid is not None:
        require(isinstance(root_partition_uuid, str) and 1 <= len(root_partition_uuid) <= 64,
                "invalid attested root partition selector")
        selected = [p for p in ext4 if p["uuid"].lower() == root_partition_uuid.lower()]
        require(len(selected) == 1 and (not typed or selected[0] == typed[0]),
                "attested root selector is missing or contradicts Linux-root type")
        root = selected[0]
    else:
        require(len(ext4) == 1 or len(typed) == 1,
                "multiple generic ext4 partitions require an explicit attested root UUID")
        root = ext4[0] if len(ext4) == 1 else typed[0]
    require(root["start"] + root["size"] == max(p["start"] + p["size"] for p in partitions), "ext4 root must be the last partition by extent")
    extents = sorted((p["start"], p["start"] + p["size"]) for p in partitions)
    require(all(a[1] <= b[0] for a, b in zip(extents, extents[1:])), "overlapping partition extents")
    details = run("dumpe2fs", "-h", root["node"])
    def number(label):
        match = re.search(r"^" + label + r":\s+(\d+)$", details, re.M)
        require(match is not None, "missing ext4 size information")
        return int(match[1])
    uuid = next(fs["uuid"] for fs in filesystems if fs["partition_uuid"] == root["uuid"])
    require(number("Block size") == 4096, "first version supports ext4 with 4 KiB blocks only")
    cluster = re.search(r"^Cluster size:\s+(\d+)$", details, re.M)
    require(cluster is None or int(cluster[1]) == 4096, "bigalloc ext4 clusters are unsupported")
    blocks, free = number("Block count"), number("Free blocks")
    require(blocks > 0 and 0 <= free <= blocks and blocks*4096 <= root["size"]*512,
            "ext4 counters exceed root partition bounds")
    return {"disk_guid": value["id"], "partitions": normalized, "root_partition_uuid": root["uuid"],
            "root_start": root["start"], "root_size": root["size"], "root_fs_uuid": uuid,
            "filesystem_bytes": blocks * 4096,
            "used_bytes": (blocks - free) * 4096,
            "other_filesystems":[fs for fs in filesystems if fs["partition_uuid"] != root["uuid"]]}, root["node"]


def preserve(before, after, target):
    require(before["disk_guid"] == after["disk_guid"] and before["root_fs_uuid"] == after["root_fs_uuid"]
            and before["root_partition_uuid"] == after["root_partition_uuid"]
            and before["root_start"] == after["root_start"], "guest disk identity changed")
    require(before.get("other_filesystems", []) == after.get("other_filesystems", []), "another filesystem identity changed")
    old = before["partitions"]
    new = after["partitions"]
    require(len(old) == len(new), "partition count changed")
    for a, b in zip(old, new):
        if a["uuid"] == before["root_partition_uuid"]:
            require({k:v for k,v in a.items() if k != "size"} == {k:v for k,v in b.items() if k != "size"}, "root partition metadata changed")
        else:
            require(a == b, "another partition changed")
    require(after["filesystem_bytes"] <= after["root_size"] * 512
            and (after["root_start"] + after["root_size"]) * 512 <= target - 33*512,
            "filesystem/partition extends beyond the smaller disk")


def partition_number(disk, partition):
    # Zvol partition names preserve the actual GPT slot (e.g. p1, p14, p15),
    # which need not equal the row's position in sfdisk's populated-entry list.
    match = re.fullmatch(re.escape(disk) + r"p([1-9][0-9]{0,2})", partition)
    require(match is not None and 1 <= int(match[1]) <= 128, "unexpected zvol partition node")
    return int(match[1])


def verify_gpt(disk, size):
    require(size % 512 == 0 and size >= 68*512, "invalid GPT disk size")
    last = size // 512 - 1
    arrays = []
    identities = []
    with open(disk, "rb", buffering=0) as stream:
        for own, alternate in ((1, last), (last, 1)):
            stream.seek(own*512)
            header = bytearray(stream.read(512))
            require(len(header) == 512 and header[:8] == b"EFI PART", "missing GPT header")
            length, crc = struct.unpack_from("<II", header, 12)
            require(92 <= length <= 512, "invalid GPT header length")
            struct.pack_into("<I", header, 16, 0)
            require(zlib.crc32(header[:length]) & 0xffffffff == crc, "GPT header CRC mismatch")
            current, backup, first, end = struct.unpack_from("<4Q", header, 24)
            require((current, backup) == (own, alternate), "GPT header cross-reference changed")
            require(34 <= first <= end <= last-33, "invalid GPT usable bounds")
            identities.append((first, end, bytes(header[56:72])))
            lba, count, stride, checksum = struct.unpack_from("<QIII", header, 72)
            require(count == 128 and stride == 128, "unsupported GPT entry layout")
            sectors = (count*stride + 511)//512
            # GPT arrays may occupy different locations within reserved space;
            # sgdisk can preserve a gap between the backup array and header.
            require((2 <= lba and lba+sectors <= first) if own == 1
                    else (end < lba and lba+sectors <= last), "GPT entry array exceeds reserved bounds")
            stream.seek(lba*512); entries = stream.read(count*stride)
            require(len(entries) == count*stride and zlib.crc32(entries) & 0xffffffff == checksum, "GPT entry CRC mismatch")
            arrays.append(entries)
    require(arrays[0] == arrays[1], "primary and backup GPT tables differ")
    require(identities[0] == identities[1], "primary and backup GPT identities/bounds differ")
    first, end, _ = identities[0]
    extents = []
    guids = set()
    for offset in range(0, len(arrays[0]), 128):
        entry = arrays[0][offset:offset+128]
        if entry[:16] == bytes(16):
            continue
        guid = entry[16:32]
        start, stop = struct.unpack_from("<2Q", entry, 32)
        require(guid != bytes(16) and guid not in guids, "invalid or duplicate partition GUID")
        require(first <= start <= stop <= end, "partition exceeds GPT usable bounds")
        guids.add(guid)
        extents.append((start, stop))
    extents.sort()
    require(all(a[1] < b[0] for a, b in zip(extents, extents[1:])), "overlapping GPT partitions")


def partition_hashes(disk, value):
    """Read exact unchanged partition extents, including a separate /boot."""
    unused(disk)
    result = {}
    with open(disk, "rb", buffering=0) as stream:
        for partition in value["partitions"]:
            if partition["uuid"] == value["root_partition_uuid"]:
                continue
            stream.seek(partition["start"] * 512)
            remaining = partition["size"] * 512
            digest = hashlib.sha256()
            while remaining:
                block = stream.read(min(4*MIB, remaining))
                require(block, "short read of unchanged partition")
                digest.update(block)
                remaining -= len(block)
            result[partition["uuid"]] = digest.hexdigest()
    return result


def configure_scope(p, *, operation_key="preparation_id", growing=False):
    """Derive all paths from independently attested immutable scope metadata."""
    global DISK_TOOL_TIMEOUT
    require(os.geteuid() == 0 and re.fullmatch(r"[0-9a-f]{32}", p["environment"]), "invalid root resize execution")
    require(re.fullmatch(r"[0-9a-f]{64}", p[operation_key]), "invalid operation identity")
    require(p.get("operation_id") is None or re.fullmatch(r"[0-9a-f]{64}", p["operation_id"]), "invalid phase operation identity")
    require(re.fullmatch(r"[a-z][a-z0-9-]{0,47}", p["owner"]), "invalid owner")
    require(type(p["vm_id"]) is int and 9001 <= p["vm_id"] <= 9099, "non-test VM refused")
    require(type(p["old_mib"]) is int and type(p["target_mib"]) is int, "invalid disk sizes")
    require(type(p.get("quick", False)) is bool, "invalid inspection mode")
    trial = p.get("physical_trial")
    if trial is None:
        pool, root, node, limit = "labpool", "labpool", "pve-lab", 8192
        require(Path("/etc/pve-dev-lab-id").read_text().strip() == p["environment"]
                and run("hostname", "-s") == node
                and run("zfs", "get", "-H", "-o", "value", "org.pve-dev:lab", pool) == p["environment"],
                "lab identity changed")
    else:
        require(set(trial) == {"pool_guid", "scope"} and trial["pool_guid"] == p["pool_guid"], "invalid trial binding")
        scope = trial["scope"]
        required = {"node", "pool", "root_dataset", "root_guid", "machine_id_sha256", "storage_prefix", "owners", "vm_ids", "capacity_mib"}
        require(required <= set(scope) <= required | {"features", "limits"},
                "unexpected physical scope fields")
        features = scope.get("features", [])
        require(isinstance(features, list) and len(set(features)) == len(features)
                and set(features) <= {"resize", "recovery", "enrollment", "migration"} and "resize" in features,
                "physical scope does not authorize resizing")
        pool, root, node = scope["pool"], scope["root_dataset"], scope["node"]
        require(re.fullmatch(r"[a-z][a-z0-9-]{0,31}", pool)
                and re.fullmatch(r"[a-z][a-z0-9-]{0,62}", node)
                and root == pool + "/pve-dev-trial-" + p["environment"]
                and scope["storage_prefix"] == "trial-" + p["environment"][:8] + "-", "physical namespace refused")
        require(scope["owners"] == ["trial-admin", "trial-user"]
                and p["owner"] in scope["owners"] and 1 <= len(scope["vm_ids"]) <= 4
                and len(set(scope["vm_ids"])) == len(scope["vm_ids"])
                and all(type(vm) is int and 9001 <= vm <= 9099 for vm in scope["vm_ids"])
                and p["vm_id"] in scope["vm_ids"], "operation escapes scoped test resources")
        require(type(scope["capacity_mib"]) is int and 8192 <= scope["capacity_mib"] <= 16*1024**2,
                "physical scope capacity outside safety bound")
        limit = min(scope["capacity_mib"], 8192)
        limits = scope.get("limits")
        if limits is not None:
            require(set(limits) == {"disk_mib", "budget_mib", "min_memory_mib", "memory_mib", "cores"}
                    and all(type(value) is int for value in limits.values())
                    and 16 <= limits["disk_mib"] <= limits["budget_mib"] <= scope["capacity_mib"], "invalid physical resize limits")
            limit = min(scope["capacity_mib"], limits["disk_mib"])
        require(run("hostname", "-s") == node
                and hashlib.sha256(Path("/etc/machine-id").read_bytes()).hexdigest() == scope["machine_id_sha256"],
                "physical machine identity changed")
        actual = dict(line.split("\t", 1) for line in run("zfs", "get", "-Hp", "-o", "property,value",
                      "type,guid,org.pve-dev:trial,quota,reservation,mountpoint", root).splitlines())
        expected = {"type":"filesystem", "guid":scope["root_guid"], "org.pve-dev:trial":p["environment"],
                    "quota":str(scope["capacity_mib"]*MIB), "reservation":str(scope["capacity_mib"]*MIB), "mountpoint":"none"}
        require(actual == expected, "physical test root identity or capacity changed")
    require(run("zpool", "get", "-Hp", "-o", "value", "guid", pool) == p["pool_guid"]
            and run("zpool", "get", "-H", "-o", "value", "health", pool) == "ONLINE", "pool identity/health changed")
    require(2048 <= min(p["target_mib"], p["old_mib"]) < max(p["target_mib"], p["old_mib"]) <= limit
            and ((p["target_mib"] > p["old_mib"]) if growing else (p["target_mib"] < p["old_mib"])),
            "disk size/direction escapes admitted capacity")
    DISK_TOOL_TIMEOUT=disk_tool_timeout(p["old_mib"],p["target_mib"])
    if not growing:
        require(p["workspace_mib"] == p["old_mib"] + (p["old_mib"]+19)//20 + p["target_mib"] + (p["target_mib"]+19)//20 + 64,
                "workspace contract changed")
    p["pool"], p["scope_root"], p["node"] = pool, root, node
    p["source_name"] = f"{root}/control/{p['owner']}/vm-{p['vm_id']}-disk-0"
    p["vm_path"] = f"/nodes/{node}/qemu/{p['vm_id']}"
    prefix = root + ("/pve-dev-shrink-" if trial is None else "/shrink-")
    p["workspace"] = prefix + p[operation_key]


def _private_directory(path, create=False):
    if create:
        try:
            path.mkdir(mode=0o700)
        except FileExistsError:
            pass
    require(path.is_absolute() and path.resolve(strict=True) == path, "noncanonical receipt path")
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o077, "unsafe receipt directory")
    for ancestor in path.parents:
        info = ancestor.lstat()
        require(stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o022,
                "unsafe receipt ancestry")


def receipt_directory(p, create=False):
    _private_directory(RECEIPT_ROOT, create=create)
    parent = RECEIPT_ROOT / p["environment"]
    _private_directory(parent, create=create)
    operation = p.get("preparation_id", p.get("operation_id"))
    require(isinstance(operation, str) and re.fullmatch(r"[0-9a-f]{64}", operation), "invalid receipt operation")
    directory = parent / operation
    if create:
        # Existing receipt state is never silently adopted by a new preparation.
        directory.mkdir(mode=0o700)
        descriptor = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try: os.fsync(descriptor)
        finally: os.close(descriptor)
    _private_directory(directory)
    return directory


def receipt_binding(p):
    return {key:p.get(key) for key in ("environment", "pool_guid", "physical_trial", "vm_id", "owner", "old_mib", "target_mib",
                                     "root_partition_uuid", "source_name", "workspace")}


def write_receipt(p, name, value, create=False):
    require(re.fullmatch(r"[a-z][a-z-]{0,31}", name), "invalid receipt name")
    directory = receipt_directory(p, create=create)
    payload = {"schema":1, "binding":receipt_binding(p), "phase_operation_id":p.get("operation_id", p.get("preparation_id")), "value":value}
    data = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    require(len(data) <= RECEIPT_LIMIT, "resize receipt exceeds bound")
    temporary = directory / (".pending-" + os.urandom(16).hex())
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data); output.flush(); os.fsync(output.fileno())
        # link creates the final name atomically and refuses an existing file.
        os.link(temporary, directory / (name + ".json"), follow_symlinks=False)
        descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try: os.fsync(descriptor)
        finally: os.close(descriptor)
    finally:
        temporary.unlink(missing_ok=True)
        descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try: os.fsync(descriptor)
        finally: os.close(descriptor)
    return {"path":str(directory / (name + ".json")), "sha256":hashlib.sha256(data).hexdigest()}


def read_receipt(p, name, optional=False, with_meta=False):
    require(re.fullmatch(r"[a-z][a-z-]{0,31}", name), "invalid receipt name")
    path = receipt_directory(p) / (name + ".json")
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        if optional: return None
        raise
    with os.fdopen(descriptor, "rb") as source:
        info = os.fstat(source.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_uid == 0 and info.st_nlink == 1
                and not info.st_mode & 0o077 and info.st_size <= RECEIPT_LIMIT, "unsafe resize receipt")
        data = source.read(RECEIPT_LIMIT + 1)
    value = json.loads(data)
    preparation_files = {"baseline", "source-fenced", "workspace-created", "replacement-created", "stage", "swap", "unlock"}
    expected_operation = p.get("preparation_id") if name in preparation_files else p.get("operation_id", p.get("preparation_id"))
    require(set(value) == {"schema", "binding", "phase_operation_id", "value"} and value["schema"] == 1
            and value["binding"] == receipt_binding(p) and value["phase_operation_id"] == expected_operation,
            "resize receipt binding or phase intent changed")
    return {"value":value["value"], "receipt":{"path":str(path), "sha256":hashlib.sha256(data).hexdigest()}} if with_meta else value["value"]


def resize_lock(p):
    path = "/run/lock/pve-dev-resize-" + p["environment"]
    descriptor = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    info = os.fstat(descriptor)
    require(stat.S_ISREG(info.st_mode) and info.st_uid == 0 and info.st_nlink == 1 and not info.st_mode & 0o077,
            "unsafe adapter lock")
    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    return descriptor


def foreign_references(p):
    needle = f"vm-{p['vm_id']}-disk-0"
    for resource in api("get", "/cluster/resources", type="vm"):
        require(resource["type"] == "qemu", "unsupported guest type during reference scan")
        path = f"/nodes/{resource['node']}/qemu/{resource['vmid']}"
        for suffix in ("/config", "/pending"):
            value = api("get", path + suffix)
            require(p["workspace"] not in json.dumps(value), "a guest refers to recovery workspace")
            if resource["vmid"] != p["vm_id"]:
                require(needle not in json.dumps(value), "another guest refers to the root disk")


def config(path, expected, locked):
    value = api("get", path + "/config")
    require(api("get", path + "/status/current")["status"] == "stopped", "VM must remain stopped")
    require(api("get", path + "/pending") == [] or all(not row.get("pending") and not row.get("delete") for row in api("get", path + "/pending")), "pending configuration refused")
    a = {k:v for k,v in value.items() if k != "digest"}
    b = {k:v for k,v in expected.items() if k != "digest"}
    if locked:
        b["lock"] = "migrate"
    require(a == b, "complete VM configuration changed")
    require(str(value.get("protection")) == "1", "VM protection must remain enabled")
    require(api("get", path + "/snapshot") == [] or all(x.get("name") == "current" for x in api("get", path + "/snapshot")), "VM snapshots refused")
    return value


def workspace(name, expected, marker, size):
    value = props(name)
    require(value == expected and value["type"] == "filesystem" and value["org.pve-dev:shrink"] == marker, "workspace identity changed")
    for key in ("quota", "reservation"):
        require(int(run("zfs", "get", "-Hp", "-o", "value", key, name)) == size, "workspace capacity changed")
    require(run("zfs", "get", "-H", "-o", "value", "mountpoint", name) == "none", "workspace mount policy changed")


def empty_filesystem(name):
    # This proves contents, not merely childlessness or a guessed byte threshold.
    import tempfile
    probe = tempfile.mkdtemp(prefix="pve-dev-shrink-", dir="/run")
    try:
        run("mount", "-t", "zfs", "-o", "ro,zfsutil,nodev,nosuid,noexec", name, probe)
    except BaseException:
        # A failed mount response is not proof that no mount happened. rmdir
        # cannot recursively traverse an uncertain mounted filesystem.
        try:
            os.rmdir(probe)
        except OSError:
            pass
        raise
    try:
        mounts = json.loads(run("findmnt", "-J", "--mountpoint", probe, "-o", "SOURCE,TARGET,FSTYPE,OPTIONS"))["filesystems"]
        require(len(mounts) == 1 and mounts[0]["source"] == name and mounts[0]["target"] == probe
                and mounts[0]["fstype"] == "zfs" and "ro" in mounts[0]["options"].split(","), "read-only workspace mount attestation failed")
        require(all(p.name == ".zfs" for p in Path(probe).iterdir()), "workspace filesystem contains data")
    finally:
        # On failure, retain the path and mount for investigation. Never use a
        # TemporaryDirectory/rmtree cleanup around a potentially live mount.
        run("umount", probe)
        os.rmdir(probe)


def stage(p):
    source, work, replacement = p["source_name"], p["workspace"], p["workspace"] + "/replacement"
    size, target = p["old_mib"] * MIB, p["target_mib"] * MIB
    cfg = config(p["vm_path"], p["config"], False)
    require(names(source) == [source], "source snapshots, bookmarks or children refused")
    same_volume(source, p["source"], size)
    foreign_references(p)
    disk = device(source)
    before, root = layout(disk, p.get("root_partition_uuid"))
    require(before["used_bytes"] + max(256*MIB, before["used_bytes"] // 5) < target - before["root_start"]*512 - 2*MIB, "target lacks conservative filesystem headroom")
    run("e2fsck", "-f", "-n", root)
    require(work not in names(p["scope_root"]), "workspace already exists; no adoption")
    require(int(run("zfs", "get", "-Hp", "-o", "value", "available", p["scope_root"])) >= (p["workspace_mib"] + 512)*MIB,
            "insufficient actual scoped workspace plus safety floor")
    source_hash = sha(disk)
    unchanged = partition_hashes(disk, before)
    baseline = {"source":p["source"], "source_sha256":source_hash, "before":before,
                "original_config":p["config"], "other_partition_sha256":unchanged}
    write_receipt(p, "baseline", baseline, create=True)
    api("set", p["vm_path"] + "/config", digest=cfg["digest"], lock="migrate")
    config(p["vm_path"], p["config"], True)
    run("zfs", "set", "readonly=on", source)
    run("zfs", "set", "org.pve-dev:shrink=" + p["preparation_id"], source)
    original = props(source)
    require(sha(disk) == source_hash, "original bytes changed while fencing")
    write_receipt(p, "source-fenced", original)
    run("zfs", "create", "-o", "mountpoint=none", "-o", "canmount=noauto", "-o", f"quota={p['workspace_mib']*MIB}",
        "-o", f"reservation={p['workspace_mib']*MIB}", "-o", "org.pve-dev:shrink=" + p["preparation_id"], work)
    write_receipt(p, "workspace-created", props(work))
    # Plain -V is thick by default. OpenZFS 2.4.4 resolves the "auto"
    # reservation sentinel for set/clone, but not for an explicit create -o.
    # Check actual backing before writing; never fall back to a sparse zvol.
    run("zfs", "create", "-V", str(size), "-b", "16K", "-o", "readonly=off",
        "-o", "org.pve-dev:owner=" + original["org.pve-dev:owner"], "-o", "org.pve-dev:image=" + original["org.pve-dev:image"],
        "-o", "org.pve-dev:shrink=" + p["preparation_id"], replacement)
    created = props(replacement)
    require(created["readonly"] == "off" and created["org.pve-dev:owner"] == original["org.pve-dev:owner"]
            and created["org.pve-dev:image"] == original["org.pve-dev:image"]
            and created["org.pve-dev:shrink"] == p["preparation_id"], "new replacement properties differ")
    same_volume(replacement, created, size)
    write_receipt(p, "replacement-created", created)
    dest = device(replacement)
    unused(dest)
    run("qemu-img", "convert", "-n", "-f", "raw", "-O", "raw", disk, dest)
    run("blockdev", "--flushbufs", dest)
    run("qemu-img", "compare", "-f", "raw", "-F", "raw", disk, dest)
    copied, root = layout(dest, before["root_partition_uuid"])
    require(copied == before, "copy layout differs before resizing")
    run("e2fsck", "-f", "-p", root, allowed=(0, 1))
    end = target // 512 - 2048
    sectors = end - before["root_start"]
    fs_kib = (sectors*512 - 8*MIB) // 4096 * 4
    require(fs_kib > 0, "partition cannot fit")
    run("resize2fs", root, str(fs_kib) + "K")
    run("e2fsck", "-f", "-n", root)
    root_index = partition_number(dest, root)
    run("sfdisk", "--lock=yes", "--wipe=never", "--wipe-partitions=never", "-N", str(root_index), dest, data=f"size={sectors}\n")
    shrunk, _ = layout(dest, before["root_partition_uuid"])
    require(shrunk["filesystem_bytes"] <= sectors*512, "filesystem did not shrink enough")
    run("zfs", "set", "volmode=none", replacement)
    run("udevadm", "settle")
    require(not (Path("/dev/zvol") / replacement).exists(), "replacement remains exposed")
    run("zfs", "set", "volsize=" + str(target), replacement)
    run("zfs", "set", "refreservation=auto", replacement)
    run("zfs", "set", "volmode=full", replacement)
    dest = device(replacement)
    run("sgdisk", "--move-second-header", dest)
    run("sgdisk", "--verify", dest)
    verify_gpt(dest, target)
    after, root = layout(dest, before["root_partition_uuid"])
    preserve(before, after, target)
    run("resize2fs", root)
    run("e2fsck", "-f", "-n", root)
    after, _ = layout(dest, before["root_partition_uuid"])
    preserve(before, after, target)
    require(partition_hashes(dest, after) == unchanged, "unchanged boot/EFI/BIOS partition bytes changed")
    require(sha(disk) == source_hash, "original disk changed during preparation")
    with open(disk, "rb") as original_stream, open(dest, "rb") as replacement_stream:
        require(original_stream.read(440) == replacement_stream.read(440), "BIOS bootstrap bytes changed")
    # Hashes are before any guest boot; later guest writes are expected.
    proof = {"source": original, "workspace": props(work), "replacement": props(replacement),
             "source_sha256": source_hash, "replacement_sha256": sha(dest), "before": before, "after": after,
             "other_partition_sha256":unchanged, "original_config":p["config"]}
    config(p["vm_path"], p["config"], True)
    require(names(work) == [work, replacement], "unexpected workspace children")
    run("zpool", "sync", p["pool"])
    proof["receipt"] = write_receipt(p, "stage", proof)
    return proof


def validate_prepared(p, swapped):
    s = p["stage"]
    require(s == _receipt_proof(p, "stage"), "helper preparation differs from immutable stage receipt")
    work = p["workspace"]
    workspace(work, s["workspace"], p["preparation_id"], p["workspace_mib"]*MIB)
    foreign_references(p)
    old = work + "/original" if swapped else p["source_name"]
    new = p["source_name"] if swapped else work + "/replacement"
    same_volume(old, s["source"], p["old_mib"]*MIB)
    same_volume(new, s["replacement"], p["target_mib"]*MIB)
    require(sha(device(old)) == s["source_sha256"], "retained original bytes changed")
    require(names(work) == [work, old if swapped else new], "workspace descendants changed")
    return old, new


def swap(p):
    old, new = validate_prepared(p, False)
    require(sha(device(new)) == p["stage"]["replacement_sha256"], "prepared disk bytes changed")
    cfg = config(p["vm_path"], p["config"], True)
    run("zfs", "rename", old, p["workspace"] + "/original")
    run("zfs", "rename", new, p["source_name"])
    attachment = re.sub(r",size=[^,]+", "", cfg["scsi0"]) + f",size={p['target_mib']}M"
    api("set", p["vm_path"] + "/config", digest=cfg["digest"], skiplock=1, scsi0=attachment)
    actual = api("get", p["vm_path"] + "/config")
    expected = dict(p["config"], scsi0=attachment)
    # PVE canonicalizes the redundant size suffix (e.g. 3072M -> 3G).
    require(actual["scsi0"].split(",")[0] == attachment.split(",")[0], "root reference changed")
    require([x for x in actual["scsi0"].split(",") if not x.startswith("size=")]
            == [x for x in attachment.split(",") if not x.startswith("size=")], "root options changed")
    require(attachment_size(actual["scsi0"]) == p["target_mib"]*MIB, "PVE root size annotation differs from the replacement")
    expected["scsi0"] = actual["scsi0"]
    config(p["vm_path"], expected, True)
    validate_prepared(p, True)
    run("zpool", "sync", p["pool"])
    proof = {"config":expected}
    proof["receipt"] = write_receipt(p, "swap", proof)
    return proof


def unlock(p):
    validate_prepared(p, True)
    require(sha(device(p["source_name"])) == p["stage"]["replacement_sha256"], "replacement changed before first boot")
    cfg = config(p["vm_path"], p["swap"]["config"], True)
    api("set", p["vm_path"] + "/config", digest=cfg["digest"], skiplock=1, delete="lock")
    config(p["vm_path"], p["swap"]["config"], False)
    proof = {"prepared":True}
    proof["receipt"] = write_receipt(p, "unlock", proof)
    return proof


def finish(p):
    old, new = validate_prepared(p, True)
    config(p["vm_path"], p["swap"]["config"], False)
    current, root = layout(device(new), p["stage"]["before"]["root_partition_uuid"])
    preserve(p["stage"]["before"], current, p["target_mib"]*MIB)
    run("e2fsck", "-f", "-n", root)
    verify_gpt(device(new), p["target_mib"]*MIB)
    foreign_references(p)
    empty_filesystem(p["workspace"])
    # Recheck immediately before the only recovery-copy deletion.
    validate_prepared(p, True)
    config(p["vm_path"], p["swap"]["config"], False)
    run("zfs", "destroy", old)
    require(names(p["workspace"]) == [p["workspace"]], "workspace not empty after original removal")
    empty_filesystem(p["workspace"])
    workspace(p["workspace"], p["stage"]["workspace"], p["preparation_id"], p["workspace_mib"]*MIB)
    run("zfs", "destroy", p["workspace"])
    require(p["workspace"] not in names(p["scope_root"]), "workspace remains after deletion")
    run("zpool", "sync", p["pool"])
    proof = {"recovery_removed":True}
    proof["receipt"] = write_receipt(p, "finish", proof)
    return proof


def _receipt_proof(p, name):
    saved = read_receipt(p, name, optional=True, with_meta=True)
    return None if saved is None else dict(saved["value"], receipt=saved["receipt"])


def _baseline_original(p, baseline, check_bytes=True):
    current = props(p["source_name"])
    stable = set(PROPS) - {"readonly", "org.pve-dev:shrink"}
    require({key:current[key] for key in stable} == {key:baseline["source"][key] for key in stable},
            "original identity or allocation changed")
    require(names(p["source_name"]) == [p["source_name"]], "original has snapshots or descendants")
    if check_bytes:
        require(sha(device(p["source_name"])) == baseline["source_sha256"], "original bytes changed")
    return current


def _abort_children(p, probe=True):
    work = p["workspace"]
    if work not in names(p["scope_root"]):
        return []
    expected = read_receipt(p, "workspace-created", optional=True)
    require(expected is not None, "workspace creation has no durable GUID receipt")
    workspace(work, expected, p["preparation_id"], p["workspace_mib"]*MIB)
    children = names(work)
    require(children in ([work], [work, work + "/replacement"]), "post-swap or foreign workspace resources refuse abort")
    if len(children) == 2:
        expected = read_receipt(p, "replacement-created", optional=True)
        require(expected is not None, "replacement creation has no durable GUID receipt")
        current = props(children[1])
        stable = set(PROPS) - {"volsize", "refreservation"}
        require({key:current[key] for key in stable} == {key:expected[key] for key in stable}
                and int(current["volsize"]) in (p["old_mib"]*MIB, p["target_mib"]*MIB)
                and int(current["refreservation"]) >= int(current["volsize"]), "replacement identity/backing changed")
        require(names(children[1]) == [children[1]], "replacement snapshots or descendants refuse abort")
        if probe: unused(device(children[1]))
    if probe: empty_filesystem(work)
    return children[1:]


def quick_inspect(p):
    result = {"classification":"full_verification_required", "state":"full_verification_required",
              "candidate_phases":{}, "verified_phases":{}, "abort_candidate":False, "abort_safe":False, "bytes_verified":False}
    try:
        baseline = read_receipt(p, "baseline")
        stage_proof, swap_proof = _receipt_proof(p, "stage"), _receipt_proof(p, "swap")
        exists = names(p["scope_root"])
        source = props(p["source_name"])
        result["observed"] = {"source_guid":source["guid"], "volsize":source["volsize"], "workspace_exists":p["workspace"] in exists}
        if source["guid"] == baseline["source"]["guid"]:
            source = _baseline_original(p, baseline, check_bytes=False)
            if source == baseline["source"] and p["workspace"] not in exists:
                config(p["vm_path"], baseline["original_config"], False)
                result["abort_candidate"] = True
                return result
            config(p["vm_path"], baseline["original_config"], True)
            require(source["readonly"] == "on" and source["org.pve-dev:shrink"] == p["preparation_id"], "partial or foreign source fence")
            _abort_children(p, probe=False)
            result["abort_candidate"] = True
            if stage_proof:
                workspace(p["workspace"], stage_proof["workspace"], p["preparation_id"], p["workspace_mib"]*MIB)
                same_volume(p["workspace"] + "/replacement", stage_proof["replacement"], p["target_mib"]*MIB)
                result["candidate_phases"]["shrink_stage"] = stage_proof
            return result
        require(stage_proof is not None and swap_proof is not None, "replacement lacks phase receipts")
        same_volume(p["source_name"], stage_proof["replacement"], p["target_mib"]*MIB)
        if p["workspace"] not in exists:
            finishing = _receipt_proof(p, "finish")
            require(finishing is not None, "missing recovery workspace has no finish receipt")
            config(p["vm_path"], swap_proof["config"], False)
            result["candidate_phases"]["shrink_finish"] = finishing
            return result
        workspace(p["workspace"], stage_proof["workspace"], p["preparation_id"], p["workspace_mib"]*MIB)
        require(names(p["workspace"]) == [p["workspace"], p["workspace"] + "/original"], "workspace descendants changed")
        same_volume(p["workspace"] + "/original", stage_proof["source"], p["old_mib"]*MIB)
        unlocked = _receipt_proof(p, "unlock")
        config(p["vm_path"], swap_proof["config"], unlocked is None)
        result["candidate_phases"]["shrink_unlock" if unlocked else "shrink_swap"] = unlocked or swap_proof
        return result
    except (OSError, RuntimeError, ValueError, KeyError, TypeError) as error:
        result.update(candidate_phases={}, abort_candidate=False, error=str(error)[:1500])
        return result


def inspect(p):
    """Return observations only. Rust decides whether its matching intent may commit."""
    if p.get("quick", False): return quick_inspect(p)
    result = {"state":"ambiguous", "verified_phases":{}, "abort_safe":False}
    try:
        baseline = read_receipt(p, "baseline")
        stage_proof, swap_proof = _receipt_proof(p, "stage"), _receipt_proof(p, "swap")
        finish_proof, abort_proof = _receipt_proof(p, "finish"), _receipt_proof(p, "abort")
        unlock_proof = _receipt_proof(p, "unlock")
        result["phase_receipts"]={"shrink_"+name:proof for name,proof in (("stage",stage_proof),("swap",swap_proof),("unlock",unlock_proof),("finish",finish_proof)) if proof is not None}
        foreign_references(p)
        exists = names(p["scope_root"])
        if finish_proof is not None:
            require(stage_proof is not None and swap_proof is not None and p["workspace"] not in exists,
                    "finish receipt/effect mismatch")
            same_volume(p["source_name"], stage_proof["replacement"], p["target_mib"]*MIB)
            config(p["vm_path"], swap_proof["config"], False)
            actual, _ = layout(device(p["source_name"]), baseline["before"]["root_partition_uuid"])
            preserve(baseline["before"], actual, p["target_mib"]*MIB)
            verify_gpt(device(p["source_name"]), p["target_mib"]*MIB)
            result.update(state="finished", verified_phases={"shrink_finish":finish_proof})
            return result
        original = props(p["source_name"])
        if original["guid"] == baseline["source"]["guid"]:
            original = _baseline_original(p, baseline)
            untouched = original == baseline["source"] and p["workspace"] not in exists
            if untouched:
                config(p["vm_path"], baseline["original_config"], False)
                result.update(state="aborted" if abort_proof else "untouched", abort_safe=True,
                              source_unchanged=True, observed_abort_complete=True)
                if abort_proof: result["verified_phases"]["shrink_abort"] = abort_proof
                return result
            config(p["vm_path"], baseline["original_config"], True)
            require(original["readonly"] == "on" and original["org.pve-dev:shrink"] == p["preparation_id"],
                    "partial or foreign original fence refuses recovery")
            _abort_children(p)
            result.update(state="fenced", abort_safe=True, source_unchanged=True)
            if stage_proof:
                q = dict(p, stage=stage_proof)
                validate_prepared(q, False)
                require(sha(device(p["workspace"] + "/replacement")) == stage_proof["replacement_sha256"],
                        "prepared replacement bytes differ from receipt")
                result.update(state="prepared", verified_phases={"shrink_stage":stage_proof})
            return result
        require(stage_proof is not None and original["guid"] == stage_proof["replacement"]["guid"],
                "canonical root has unknown identity")
        q = dict(p, stage=stage_proof)
        validate_prepared(q, True)
        require(swap_proof is not None, "swap effect lacks durable completion receipt")
        require(sha(device(p["source_name"])) == stage_proof["replacement_sha256"],
                "replacement changed before ambiguous operation was acknowledged")
        unlock_proof = _receipt_proof(p, "unlock")
        config(p["vm_path"], swap_proof["config"], unlock_proof is None)
        result.update(state="unlocked" if unlock_proof else "swapped",
                      verified_phases={"shrink_stage":stage_proof, "shrink_swap":swap_proof})
        if unlock_proof: result["verified_phases"]["shrink_unlock"] = unlock_proof
        return result
    except (OSError, RuntimeError, ValueError, KeyError, TypeError) as error:
        result.update(state="ambiguous", verified_phases={}, abort_safe=False, error=str(error)[:1500])
        return result


def abort(p):
    """Explicit pre-swap abort only; the retained source must remain byte-exact."""
    observed = inspect(p)
    require(observed["abort_safe"] and observed["state"] in ("untouched", "fenced", "prepared"),
            "abort requires independently proved pre-swap original and owned descendants")
    require(read_receipt(p, "abort", optional=True) is None, "abort already has a durable receipt; inspect instead")
    baseline = read_receipt(p, "baseline")
    original = _baseline_original(p, baseline)
    foreign_references(p)
    children = _abort_children(p)
    if original != baseline["source"]:
        config(p["vm_path"], baseline["original_config"], True)
    for child_name in children:
        _baseline_original(p, baseline)
        _abort_children(p)
        foreign_references(p)
        run("zfs", "destroy", child_name)
    if p["workspace"] in names(p["scope_root"]):
        _abort_children(p)
        require(names(p["workspace"]) == [p["workspace"]], "workspace not childless")
        run("zfs", "destroy", p["workspace"])
    _baseline_original(p, baseline)
    if original != baseline["source"]:
        values = ["readonly=" + baseline["source"]["readonly"]]
        if baseline["source"]["org.pve-dev:shrink"] == "-":
            run("zfs", "inherit", "org.pve-dev:shrink", p["source_name"])
        else:
            values.append("org.pve-dev:shrink=" + baseline["source"]["org.pve-dev:shrink"])
        run("zfs", "set", *values, p["source_name"])
        same_volume(p["source_name"], baseline["source"], p["old_mib"]*MIB)
        cfg = config(p["vm_path"], baseline["original_config"], True)
        api("set", p["vm_path"] + "/config", digest=cfg["digest"], skiplock=1, delete="lock")
    same_volume(p["source_name"], baseline["source"], p["old_mib"]*MIB)
    config(p["vm_path"], baseline["original_config"], False)
    require(p["workspace"] not in names(p["scope_root"]), "aborted workspace remains")
    run("zpool", "sync", p["pool"])
    proof = {"aborted":True, "source_sha256":baseline["source_sha256"], "source":baseline["source"]}
    proof["receipt"] = write_receipt(p, "abort", proof)
    return proof


def main(p):
    configure_scope(p)
    descriptor = resize_lock(p)
    try:
        return {"stage":stage, "swap":swap, "unlock":unlock, "finish":finish, "inspect":inspect, "abort":abort}[p["phase"]](p)
    finally:
        os.close(descriptor)


if __name__ == "__main__":
    parameters = json.loads(sys.stdin.read())
    parent = os.getppid()
    require(parent == parameters["helper_pid"] and parent > 1, "unexpected helper parent")
    signal.signal(signal.SIGTERM, stop_child)
    signal.signal(signal.SIGINT, stop_child)
    require(ctypes.CDLL(None).prctl(1, signal.SIGTERM, 0, 0, 0) == 0 and os.getppid() == parent, "cannot bind worker lifetime to helper")
    print(json.dumps(main(parameters), sort_keys=True))
