#!/usr/bin/env python3
"""Fixed operations for fresh migration-spike VMs, never a general host helper."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import ssl
import subprocess
import sys
import time
import urllib.parse
import uuid

STATE = Path("/root/pve-migration-spike")
MARKER = Path("/etc/pve-dev-migration-spike.json")
SECRETS = []


def sanitize(text):
    for secret in SECRETS:
        text = text.replace(secret, "<REDACTED-LAB-TOKEN>")
    return re.sub(r"(PVEAPIToken=[^\s,]+)=([^\s,]+)", r"\1=<REDACTED>", text)


def cmd(arguments, *, payload=None, allowed=(0,), timeout=180):
    result = subprocess.run(arguments, input=payload, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout)
    if result.returncode not in allowed:
        raise RuntimeError(f"{arguments[0]} failed ({result.returncode}): {sanitize(result.stderr[-2500:])}")
    return result


def pve(route, action="get", **values):
    args = ["pvesh", action, route, "--output-format", "json"]
    for key, value in values.items():
        args += ["--" + key.replace("_", "-"), str(value)]
    text = cmd(args).stdout
    if action == "get":
        return json.loads(text)
    # PVE's synchronous set handler prints human-readable "update VM ..."
    # despite --output-format=json. Never reinterpret success as no effect.
    if action == "set":
        return None
    lines = text.strip().splitlines()
    last = json.loads(lines[-1]) if lines and lines[-1].startswith('"UPID:') else None
    if isinstance(last, str) and last.startswith("UPID:"):
        node = route.split("/")[2]
        if not last.startswith("UPID:" + node + ":"):
            raise RuntimeError("unexpected node in task receipt")
        deadline = time.monotonic() + 180
        while True:
            status = pve(f"/nodes/{node}/tasks/{last}/status")
            if status["status"] == "stopped":
                if status.get("exitstatus") != "OK":
                    raise RuntimeError("fixture task failed: " + str(status.get("exitstatus")))
                return last
            if time.monotonic() >= deadline:
                raise RuntimeError("fixture task remained running")
            time.sleep(.5)
    if re.fullmatch(r"/nodes/[^/]+/qemu(?:/\d+)?", route):
        raise RuntimeError("VM mutation succeeded without a parseable task receipt; investigate")
    return None


def write_private(path, content):
    with open(path, "x", opener=lambda name, flags: os.open(name, flags, 0o600)) as out:
        out.write(content)
        out.flush()
        os.fsync(out.fileno())
    directory = os.open(Path(path).parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def sha_file(path):
    with open(path, "rb") as data:
        return hashlib.file_digest(data, "sha256").hexdigest()


def prepare(role, nonce, base_nonce):
    if role not in ("a", "b") or not all(re.fullmatch("[a-f0-9]{32}", n) for n in (nonce, base_nonce)):
        raise RuntimeError("invalid disposable identity")
    if cmd(["hostname"]).stdout.strip() != "pve-lab" or Path("/etc/pve-dev-lab-id").read_text().strip() != base_nonce:
        raise RuntimeError("not the sealed disposable base")
    if MARKER.exists() or Path("/etc/pve/corosync.conf").exists():
        raise RuntimeError("already configured or clustered; refusing reuse")
    if pve("/cluster/resources", type="vm"):
        raise RuntimeError("base has guests; refusing identity reset")
    controller_keys = [line for line in Path("/root/.ssh/authorized_keys").read_text().splitlines()
                       if line.startswith("ssh-ed25519 ")]
    if len(controller_keys) != 1:
        raise RuntimeError("expected exactly one disposable controller public key")
    node = "pve-mig-" + role
    octet = 1 if role == "a" else 2
    mac = f"02:50:77:00:00:0{octet}"
    links = json.loads(cmd(["ip", "-j", "link"]).stdout)
    interface = next(link["ifname"] for link in links if link.get("address") == mac)
    cmd(["ip", "address", "replace", f"172.31.77.{octet}/30", "dev", interface])
    cmd(["ip", "link", "set", interface, "up"])
    cmd(["sysctl", "-w", "net.ipv4.ip_forward=0"])
    Path("/etc/cloud/cloud-init.disabled").write_text("")
    cmd(["systemctl", "stop", "pveproxy", "pvedaemon", "pvestatd", "pve-cluster"])
    if cmd(["mountpoint", "-q", "/etc/pve"], allowed=(0, 1, 32)).returncode == 0:
        raise RuntimeError("pmxcfs still mounted after service stop")
    # These files belong only to this throwaway overlay. A fresh empty pmxcfs
    # database prevents copied CA/auth/cluster identities from crossing nodes.
    for name in ("config.db", "config.db-wal", "config.db-shm"):
        path = Path("/var/lib/pve-cluster") / name
        if path.exists():
            path.rename(STATE / ("base-" + name))
    Path("/etc/hostname").write_text(node + "\n")
    Path("/etc/hosts").write_text(f"127.0.0.1 localhost\n172.31.77.{octet} {node}.internal {node}\n")
    cmd(["hostname", node])
    Path("/etc/machine-id").write_text(uuid.uuid4().hex + "\n")
    Path("/etc/pve-dev-lab-id").write_text(nonce + "\n")
    key = Path("/etc/ssh/ssh_host_ed25519_key")
    key.rename(STATE / "base-ssh-host-key")
    Path(str(key) + ".pub").rename(STATE / "base-ssh-host-key.pub")
    cmd(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)])
    # Fresh pvecm initialization reads the RSA host public key as well as the
    # pinned Ed25519 key. The original cloud seed deliberately supplied only
    # Ed25519, so generate the otherwise missing host-key types here.
    cmd(["ssh-keygen", "-A"])
    for suffix in ("", ".pub"):
        old = Path("/root/.ssh/id_rsa" + suffix)
        if old.exists():
            old.rename(STATE / ("base-root-rsa" + suffix))
    cmd(["ssh-keygen", "-q", "-t", "rsa", "-b", "2048", "-N", "", "-f", "/root/.ssh/id_rsa"])
    cmd(["systemctl", "start", "pve-cluster"])
    cmd(["pvecm", "updatecerts", "--force"])
    with Path("/etc/pve/priv/authorized_keys").open("a") as keys:
        keys.write("\n" + controller_keys[0] + "\n")
    cmd(["systemctl", "start", "pvedaemon", "pvestatd", "pveproxy"])
    cmd(["systemctl", "reload", "ssh"])
    marker = {"schema": 1, "role": role, "node": node, "nonce": nonce}
    write_private(MARKER, json.dumps(marker))
    Path("/sys/module/zfs/parameters/zfs_arc_max").write_text(str(256 * 1024**2))
    nodes = pve("/cluster/status")
    if len([n for n in nodes if n.get("type") == "node"]) != 1 or Path("/etc/pve/corosync.conf").exists():
        raise RuntimeError("not an independent standalone node")
    certificate = ssl.PEM_cert_to_DER_cert(Path("/etc/pve/local/pve-ssl.pem").read_text())
    keypub = Path(str(key) + ".pub").read_text().strip()
    fingerprint = cmd(["ssh-keygen", "-lf", str(key) + ".pub"]).stdout.split()[1]
    return {"node": node, "machine_id_sha256": sha_file("/etc/machine-id"),
            "certificate_sha256": hashlib.sha256(certificate).hexdigest(),
            "ca_sha256": sha_file("/etc/pve/pve-root-ca.pem"),
            "root_rsa_public_sha256": sha_file("/root/.ssh/id_rsa.pub"),
            "ssh_key_sha256": fingerprint, "ssh_public_key": keypub,
            "one_node_cluster": True, "pve_version": cmd(["pveversion"]).stdout.strip()}


def attest(payload):
    if os.geteuid() != 0 or not MARKER.is_file():
        raise RuntimeError("not a configured migration fixture")
    value = json.loads(MARKER.read_text())
    if payload.get("nonce") != value["nonce"] or cmd(["hostname"]).stdout.strip() != value["node"]:
        raise RuntimeError("fixture identity mismatch")
    if Path("/etc/pve/corosync.conf").exists():
        raise RuntimeError("fixture unexpectedly joined a cluster")
    vmid = payload.get("vmid")
    if vmid is not None and vmid not in (9001, 9002):
        raise RuntimeError("only two toy VM IDs are permitted")
    for path in STATE.glob("*token*.json"):
        secret = json.loads(path.read_text()).get("token")
        if secret:
            SECRETS.extend([secret, secret.rsplit("=", 1)[-1]])
    return value


def storage(marker):
    if cmd(["zpool", "list", "-H", "-o", "name"]).stdout.strip():
        raise RuntimeError("unexpected preexisting pool")
    disks = [f"/dev/disk/by-id/virtio-lab-data-{c}" for c in ("a", "b")]
    for disk in disks:
        signatures = json.loads(cmd(["wipefs", "--no-act", "--json", disk]).stdout)
        if signatures.get("signatures"):
            raise RuntimeError("data disk not blank")
    cmd(["zpool", "create", "-o", "ashift=12", "-O", "mountpoint=none", "-O", "compression=lz4",
         "-O", "org.pve-dev:lab=" + marker["nonce"], "labpool", "mirror", *disks])
    budget = "5G" if (STATE / "qualification.json").exists() else "256M"
    cmd(["zfs", "create", "-o", "quota=" + budget, "-o", "reservation=" + budget, "labpool/alloc"])
    storage_id = "src-alloc" if marker["role"] == "a" else "dst-alloc"
    pve("/storage", "create", storage=storage_id, type="zfspool", pool="labpool/alloc", content="images", sparse=0)
    bridge = "vmbr77" if marker["role"] == "a" else "vmbr78"
    cmd(["ip", "link", "add", bridge, "type", "bridge"])
    cmd(["ip", "link", "set", bridge, "up"])
    budget_bytes = 5 * 1024**3 if budget == "5G" else 256 * 1024**2
    return {"storage": storage_id, "sparse": False, "quota_bytes": budget_bytes,
            "reservation_bytes": budget_bytes, "bridge": bridge,
            "pool_guid": cmd(["zpool", "get", "-Hp", "-o", "value", "guid", "labpool"]).stdout.strip()}


def inspect(marker, vmid):
    config = pve(f"/nodes/{marker['node']}/qemu/{vmid}/config", current=1)
    status = pve(f"/nodes/{marker['node']}/qemu/{vmid}/status/current")
    volume = f"labpool/alloc/vm-{vmid}-disk-0"
    props = cmd(["zfs", "get", "-Hp", "-o", "property,value", "guid,volsize,refreservation,org.pve-dev:spike-owner", volume]).stdout
    values = dict(line.split("\t", 1) for line in props.splitlines())
    seed_volume = f"/dev/zvol/labpool/alloc/vm-{vmid}-cloudinit"
    ci_props = cmd(["zfs", "get", "-Hp", "-o", "property,value", "guid,volsize,refreservation",
                    f"labpool/alloc/vm-{vmid}-cloudinit"]).stdout
    ci_values = dict(line.split("\t", 1) for line in ci_props.splitlines())
    meta = cmd(["qm", "cloudinit", "dump", str(vmid), "meta"]).stdout
    return {"smbios1": config.get("smbios1"), "vmgenid": config.get("vmgenid"), "root_guid": values["guid"],
            "root_size": int(values["volsize"]), "root_refreservation": int(values["refreservation"]),
            "root_sha256": sha_file("/dev/zvol/" + volume), "cloudinit_sha256": sha_file(seed_volume),
            "cloudinit_guid": ci_values["guid"], "cloudinit_size": int(ci_values["volsize"]),
            "cloudinit_refreservation": int(ci_values["refreservation"]),
            "cloudinit_meta_sha256": hashlib.sha256(meta.encode()).hexdigest(),
            "owner_property": values["org.pve-dev:spike-owner"], "scsi0": config.get("scsi0"),
            "ide2": config.get("ide2"), "net0": config.get("net0"), "lock": config.get("lock"),
            "cicustom": config.get("cicustom"),
            "protection": config.get("protection", 0),
            "status": status["status"], "onboot": config.get("onboot", 0)}


def quota_probe(marker):
    """Real ZFS semantics on a fixed scratch namespace, never VM storage."""
    dataset = "labpool/quota-spike"
    cmd(["zfs", "create", "-o", "quota=128M", "-o", "reservation=128M",
         "-o", "org.pve-dev:quota-spike=" + marker["nonce"], dataset])
    def properties(name=dataset):
        output = cmd(["zfs", "get", "-Hp", "-o", "property,value",
                      "guid,quota,reservation,used,usedbychildren,usedbysnapshots,usedbyrefreservation,refreservation,volsize", name]).stdout
        return dict(line.split("\t", 1) for line in output.splitlines())
    initial = properties()
    steps = []
    def change(prop, value):
        result = subprocess.run(["zfs", "set", f"{prop}={value}", dataset], text=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        steps.append({"property": prop, "value": value, "exit": result.returncode,
                      "stderr": result.stderr.strip(), "observed": properties()})
        return result.returncode
    change("quota", "192M")
    change("reservation", "192M")
    cmd(["zfs", "create", "-V", "32M", dataset + "/probe"])
    cmd(["udevadm", "settle"])
    volume_path = "/dev/zvol/" + dataset + "/probe"
    with open(volume_path, "r+b", buffering=0) as volume:
        volume.write(os.urandom(8 * 1024**2))
        os.fsync(volume.fileno())
    cmd(["zfs", "snapshot", dataset + "/probe@held"])
    with open(volume_path, "r+b", buffering=0) as volume:
        volume.write(os.urandom(8 * 1024**2))
        os.fsync(volume.fileno())
    cmd(["zpool", "sync", "labpool"])
    snapshot_accounting = {"parent": properties(), "volume": properties(dataset + "/probe"),
                           "snapshot": cmd(["zfs", "get", "-Hp", "-o", "property,value", "guid,used", dataset + "/probe@held"]).stdout}
    change("reservation", "96M")
    change("quota", "96M")
    below_reservation_exit = change("quota", "64M")
    # Restore the proposed safe bound if the implementation permits the lower
    # quota: observed behavior, not an assumption, is the purpose of the spike.
    change("quota", "96M")
    change("reservation", "8M")
    below_used_exit = change("quota", "8M")
    change("quota", "96M")
    change("reservation", "96M")
    unlimited_probes = []
    for candidate in ("0", "none"):
        before_unlimited = properties()
        try:
            exit_status = change("quota", candidate)
            unlimited_probes.append({"input": candidate, "exit": exit_status,
                                     "stderr": steps[-1]["stderr"], "before": before_unlimited,
                                     "observed": properties()})
        finally:
            if change("quota", "96M") != 0:
                raise RuntimeError("scratch quota restore failed")
    final = properties()
    return {"dataset": dataset, "initial": initial, "steps": steps,
            "snapshot_accounting": snapshot_accounting, "final": final,
            "guid_preserved": initial["guid"] == final["guid"],
            "below_reservation_refused": below_reservation_exit != 0,
            "below_used_refused": below_used_exit != 0,
            "zero_and_none_probes": unlimited_probes,
            "vm_storage_touched": False}


def create(marker, vmid):
    if marker["role"] != "a":
        raise RuntimeError("fixture creation allowed only on source")
    if pve("/cluster/resources", type="vm"):
        raise RuntimeError("source already has a fixture; no implicit overwrite")
    pve(f"/nodes/{marker['node']}/qemu", "create", vmid=vmid, name="spike-" + marker["nonce"][:8],
        memory=128, cores=1, kvm=0, cpu="qemu64", scsihw="virtio-scsi-pci", onboot=0,
        protection=1 if (STATE / "qualification.json").exists() else 0,
        net0="virtio=02:50:90:00:00:01,bridge=vmbr77", smbios1="uuid=" + str(uuid.uuid4()))
    cmd(["pvesm", "alloc", "src-alloc", str(vmid), f"vm-{vmid}-disk-0", "65536", "--format", "raw"])
    cmd(["udevadm", "settle"])
    root = f"/dev/zvol/labpool/alloc/vm-{vmid}-disk-0"
    with open(root, "wb", buffering=0) as disk:
        for _ in range(64):
            disk.write(os.urandom(1024**2))
        os.fsync(disk.fileno())
    cmd(["zfs", "set", "org.pve-dev:spike-owner=" + marker["nonce"], f"labpool/alloc/vm-{vmid}-disk-0"])
    pve(f"/nodes/{marker['node']}/qemu/{vmid}/config", "set",
        scsi0=f"src-alloc:vm-{vmid}-disk-0", boot="order=scsi0", ide2="src-alloc:cloudinit", citype="nocloud")
    cmd(["qm", "cloudinit", "update", str(vmid)])
    return inspect(marker, vmid)


def token(marker, vmid, source=False):
    tokenid = ("source" if source else "target") + str(vmid)
    existing = STATE / (tokenid + "-token.json")
    if existing.exists():
        secret = json.loads(existing.read_text())["token"]
        SECRETS.extend([secret, secret.rsplit("=", 1)[-1]])
        return {"token": secret, "reused_fixture_token": True, "privilege_separation": True}
    result = json.loads(cmd(["pveum", "user", "token", "add", "root@pam", tokenid,
                             "--privsep", "1", "--expire", str(int(time.time()) + 7200), "--output-format", "json"]).stdout)
    value = result["value"]
    full = result.get("full-tokenid", f"root@pam!{tokenid}")
    secret = full + "=" + value
    SECRETS.extend([value, secret])
    cmd(["pveum", "acl", "modify", f"/vms/{vmid}", "--roles", "PVEVMAdmin", "--tokens", full])
    if not source:
        roles = pve("/access/roles")
        if not any(role["roleid"] == "SpikeIncoming" for role in roles):
            cmd(["pveum", "role", "add", "SpikeIncoming", "--privs", "Sys.Incoming Sys.Audit"])
        cmd(["pveum", "acl", "modify", "/", "--roles", "SpikeIncoming", "--tokens", full])
        if not any(role["roleid"] == "SpikeStorage" for role in roles):
            cmd(["pveum", "role", "add", "SpikeStorage", "--privs", "Datastore.Audit Datastore.AllocateSpace"])
        cmd(["pveum", "acl", "modify", "/storage/dst-alloc", "--roles", "SpikeStorage", "--tokens", full])
        if vmid == 9002 and (STATE / "qualification.json").exists():
            # PVE Storage::check_volume_access explicitly requires
            # Datastore.AllocateSpace for cicustom snippet references. This
            # fresh storage contains only the staged nonce-bound lab snippet.
            cmd(["pveum", "acl", "modify", "/storage/spike-snippets", "--roles", "SpikeStorage", "--tokens", full])
        cmd(["pveum", "acl", "modify", "/sdn/zones/localnetwork/vmbr78", "--roles", "PVESDNUser", "--tokens", full])
    write_private(STATE / (tokenid + "-token.json"), json.dumps({"token": secret}))
    return {"token": secret, "privilege_separation": True, "expires_in_seconds": 7200,
            "allowed_vmid": vmid, "datastore": None if source else "dst-alloc"}


def api(host, fingerprint, path, *, token_value=None, body=None):
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    connection = http.client.HTTPSConnection(host, 8006, timeout=180, context=context)
    connection.connect()
    actual = hashlib.sha256(connection.sock.getpeercert(binary_form=True)).hexdigest()
    if actual != fingerprint:
        connection.close()
        raise RuntimeError("peer TLS certificate fingerprint mismatch before authentication")
    headers = {}
    if token_value:
        headers["Authorization"] = "PVEAPIToken=" + token_value
    if body is not None:
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    connection.request("POST" if body is not None else "GET", "/api2/json" + path,
                       body=urllib.parse.urlencode(body) if body is not None else None, headers=headers)
    response = connection.getresponse()
    data = response.read(1024**2)
    connection.close()
    if response.status != 200:
        raise RuntimeError(f"API status {response.status}: {sanitize(data.decode(errors='replace')[:1500])}")
    return json.loads(data)["data"]


def migrate(marker, payload):
    if marker["role"] != "a" or payload.get("target_vmid") != payload["vmid"]:
        raise RuntimeError("invalid fixed source/destination migration")
    if not re.fullmatch(r"root@pam!target" + str(payload["vmid"]) + r"=[a-f0-9-]{36}", payload["token"]):
        raise RuntimeError("not the exact disposable target credential shape")
    uuid.UUID(payload["token"].rsplit("=", 1)[-1])
    if not re.fullmatch("[a-f0-9]{64}", payload["fingerprint"]):
        raise RuntimeError("invalid fixed peer certificate fingerprint")
    source_token = token(marker, payload["vmid"], source=True)["token"]
    target_file = STATE / ("remote-target-token" + str(payload["vmid"]) + ".json")
    if target_file.exists():
        if json.loads(target_file.read_text())["token"] != payload["token"]:
            raise RuntimeError("fixture target token changed")
    else:
        write_private(target_file, json.dumps({"token": payload["token"]}))
    transport = "host=172.31.77.2"
    if payload.get("loopback_tunnel"):
        if not (STATE / "tunnel.json").exists():
            raise RuntimeError("no owned fixed loopback tunnel")
        transport = "host=127.0.0.1,port=18006"
    certificate = ssl.PEM_cert_to_DER_cert(Path("/etc/pve/local/pve-ssl.pem").read_text())
    source_fp = hashlib.sha256(certificate).hexdigest()
    endpoint = transport + ",apitoken=" + payload["token"] + ",fingerprint=" + ":".join(re.findall("..", payload["fingerprint"]))
    params = {"target-vmid": payload["target_vmid"], "target-endpoint": endpoint,
              "target-storage": "src-alloc:dst-alloc", "target-bridge": "vmbr77:vmbr78",
              "online": 0, "delete": 0}
    if payload.get("bwlimit"):
        params["bwlimit"] = payload["bwlimit"]
    upid = api("127.0.0.1", source_fp, f"/nodes/{marker['node']}/qemu/{payload['vmid']}/remote_migrate",
               token_value=source_token, body=params)
    if not isinstance(upid, str) or not upid.startswith("UPID:" + marker["node"] + ":"):
        raise RuntimeError("migration submission did not return a source task receipt")
    return {"upid": upid, "source_api_token_privsep": True, "target_api_token_privsep": True}


def main():
    if len(sys.argv) > 1:
        if len(sys.argv) != 5 or sys.argv[1] != "prepare":
            raise RuntimeError("invalid preparation command")
        return prepare(*sys.argv[2:])
    raw = sys.stdin.read(256 * 1024 + 1)
    if len(raw) > 256 * 1024:
        raise RuntimeError("oversized fixture request")
    payload = json.loads(raw)
    if payload.get("token"):
        SECRETS.extend([payload["token"], payload["token"].rsplit("=", 1)[-1]])
    marker = attest(payload)
    action = payload["action"]
    if action == "storage":
        return storage(marker)
    if action == "peer":
        return {"peer": "172.31.77.2", "login_realms": api("172.31.77.2", payload["fingerprint"], "/access/domains"), "fingerprint_verified": True}
    if action == "create":
        return create(marker, payload["vmid"])
    if action == "create_conflict" and marker["role"] == "b":
        if pve("/cluster/resources", type="vm"):
            raise RuntimeError("unexpected target VM inventory")
        pve(f"/nodes/{marker['node']}/qemu", "create", vmid=payload["vmid"], name="spike-conflict",
            description="spike-conflict:" + marker["nonce"], memory=128, cores=1, kvm=0, onboot=0)
        cmd(["pvesm", "alloc", "dst-alloc", str(payload["vmid"]), f"vm-{payload['vmid']}-disk-0", "32768", "--format", "raw"])
        cmd(["udevadm", "settle"])
        with open(f"/dev/zvol/labpool/alloc/vm-{payload['vmid']}-disk-0", "r+b", buffering=0) as disk:
            disk.write(b"EXISTING CONFLICT VM DATA MUST SURVIVE\n" * 1024)
            os.fsync(disk.fileno())
        pve(f"/nodes/{marker['node']}/qemu/{payload['vmid']}/config", "set",
            scsi0=f"dst-alloc:vm-{payload['vmid']}-disk-0")
        return {"created_owned_conflict_with_disk": payload["vmid"]}
    if action == "inspect_conflict" and marker["role"] == "b":
        vmid = payload["vmid"]
        config = pve(f"/nodes/{marker['node']}/qemu/{vmid}/config", current=1)
        props = cmd(["zfs", "get", "-Hp", "-o", "property,value", "guid,volsize,refreservation",
                     f"labpool/alloc/vm-{vmid}-disk-0"]).stdout
        return {"smbios1": config.get("smbios1"), "vmgenid": config.get("vmgenid"),
                "config_sha256": hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest(),
                "root_properties": dict(line.split("\t", 1) for line in props.splitlines()),
                "root_sha256": sha_file(f"/dev/zvol/labpool/alloc/vm-{vmid}-disk-0"),
                "volumes": cmd(["zfs", "list", "-H", "-t", "volume", "-o", "name", "-r", "labpool/alloc"]).stdout.splitlines()}
    if action == "remove_conflict" and marker["role"] == "b":
        config = pve(f"/nodes/{marker['node']}/qemu/{payload['vmid']}/config", current=1)
        if config.get("description") != "spike-conflict:" + marker["nonce"]:
            raise RuntimeError("not the owned diskless conflict fixture")
        pve(f"/nodes/{marker['node']}/qemu/{payload['vmid']}", "delete")
        if any(int(vm["vmid"]) == payload["vmid"] for vm in pve("/cluster/resources", type="vm")):
            raise RuntimeError("owned conflict VM is not absent after deletion")
        # Proxmox deletion removes the VM's ACL entries too. Re-authorize only
        # this now-empty fixture ID for the subsequent positive experiment.
        cmd(["pveum", "acl", "modify", f"/vms/{payload['vmid']}", "--roles", "PVEVMAdmin",
             "--tokens", "root@pam!target" + str(payload["vmid"])])
        return {"removed_only_owned_conflict": payload["vmid"],
                "restored_exact_vm_acl_after_owned_fixture_deletion": True}
    if action == "inspect":
        return inspect(marker, payload["vmid"])
    if action == "quota_probe":
        return quota_probe(marker)
    if action == "token" and marker["role"] == "b":
        return token(marker, payload["vmid"])
    if action == "migrate":
        return migrate(marker, payload)
    if action == "task":
        upid = payload["upid"]
        if not re.fullmatch(r"UPID:" + marker["node"] + r":[A-Fa-f0-9:]+[^\s/]*", upid):
            raise RuntimeError("invalid own-node task receipt")
        status = pve(f"/nodes/{marker['node']}/tasks/{upid}/status")
        status["log"] = pve(f"/nodes/{marker['node']}/tasks/{upid}/log", limit=500)
        status["private_peer_tcp_connections"] = [line for line in cmd(["ss", "-Hnt"]).stdout.splitlines() if "172.31.77." in line]
        return status
    if action == "start_probe":
        result = subprocess.run(["qm", "start", str(payload["vmid"])], text=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
        status = pve(f"/nodes/{marker['node']}/qemu/{payload['vmid']}/status/current")
        if status["status"] == "running":
            cmd(["qm", "stop", str(payload["vmid"])])
        return {"start_exit": result.returncode, "running_after_start": status["status"] == "running",
                "stderr": sanitize(result.stderr[-1500:]), "probe_vm_stopped_afterward": True,
                "guest_os_boot_verified": False}
    if action == "revoke":
        cmd(["pveum", "user", "token", "remove", "root@pam", "target" + str(payload["vmid"])])
        return {"revoked": True}
    if action == "cleanup_tokens":
        prefix = "source" if marker["role"] == "a" else "target"
        known = {prefix + str(vmid) for vmid in (9001, 9002)}
        tokens = json.loads(cmd(["pveum", "user", "token", "list", "root@pam", "--output-format", "json"]).stdout)
        removed = []
        for entry in tokens:
            if entry["tokenid"] in known:
                cmd(["pveum", "user", "token", "remove", "root@pam", entry["tokenid"]])
                removed.append(entry["tokenid"])
        after = json.loads(cmd(["pveum", "user", "token", "list", "root@pam", "--output-format", "json"]).stdout)
        return {"removed_token_ids": removed,
                "all_spike_tokens_removed": not any(entry["tokenid"] in known for entry in after)}
    raise RuntimeError("unsupported fixed fixture operation")


if __name__ == "__main__":
    try:
        result = main()
        output = json.dumps(result)
        print(output if isinstance(result, dict) and "token" in result else sanitize(output))
    except Exception as error:
        print(sanitize(str(error)), file=sys.stderr)
        raise SystemExit(1)
