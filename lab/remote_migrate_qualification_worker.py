#!/usr/bin/env python3
"""Additional fixed lab gates. Loaded only after the independent-host attestation."""
import io
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import time
import uuid

import remote_migrate_worker as w


def fixed(marker, role=None):
    if role and marker["role"] != role:
        raise RuntimeError("wrong qualification fixture role")
    if not (w.STATE / "qualification.json").is_file():
        raise RuntimeError("not an expanded qualification fixture")


def enable(marker):
    w.write_private(w.STATE / "qualification.json", json.dumps({"nonce": marker["nonce"]}))
    w.cmd(["systemctl", "disable", "pve-lab-bootstrap.service"])
    octet = 1 if marker["role"] == "a" else 2
    links = json.loads(w.cmd(["ip", "-j", "link"]).stdout)
    interface = next(link["ifname"] for link in links if link.get("address") == f"02:50:77:00:00:0{octet}")
    bridge = "vmbr77" if marker["role"] == "a" else "vmbr78"
    script = ("#!/bin/sh\nset -eu\n"
              f"ip address replace 172.31.77.{octet}/30 dev {interface}\n"
              f"ip link set {interface} up\n"
              f"ip link show {bridge} >/dev/null 2>&1 || ip link add {bridge} type bridge\n"
              f"ip address replace 10.77.0.1/24 dev {bridge}\n"
              f"ip link set {bridge} up\n"
              "sysctl -w net.ipv4.ip_forward=0\n")
    network = w.STATE / "network.sh"
    w.write_private(network, script)
    network.chmod(0o700)
    Path("/etc/systemd/system/migration-fixture-network.service").write_text(
        "[Unit]\nAfter=network.target\nBefore=pve-cluster.service\n"
        "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=" + str(network) +
        "\n[Install]\nWantedBy=multi-user.target\n")
    w.cmd(["systemctl", "daemon-reload"])
    w.cmd(["systemctl", "enable", "migration-fixture-network.service"])
    # storage() initially creates the bridge; start this unit after storage().
    return {"qualification_enabled": True, "bootstrap_identity_reset_disabled": True}


def snippet(marker):
    fixed(marker)
    directory = Path("/var/lib/pve-spike-snippets/snippets")
    directory.mkdir(parents=True)
    content = f"instance-id: migration-guest-{marker['nonce']}\nlocal-hostname: migration-guest\n"
    path = directory / "identity.yaml"
    w.write_private(path, content)
    path.chmod(0o644)
    w.pve("/storage", "create", storage="spike-snippets", type="dir",
          path=str(directory.parent), content="snippets")
    return {"path": "spike-snippets:snippets/identity.yaml", "sha256": w.sha_file(path),
            "instance_id": "migration-guest-" + marker["nonce"], "staged_before_migration": True}


def tunnel_key(marker):
    fixed(marker, "a")
    path = w.STATE / "tunnel-key"
    w.cmd(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(path)])
    return {"public_key": path.with_suffix(".pub").read_text().strip()}


def tunnel_target(marker, payload):
    fixed(marker, "b")
    key = payload["public_key"]
    if not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\n]*)?", key):
        raise RuntimeError("invalid throwaway tunnel public key")
    w.cmd(["useradd", "--create-home", "--shell", "/usr/sbin/nologin", "migration-spike"])
    # '*' is password-unusable, while avoiding the locked-account '!' marker.
    w.cmd(["usermod", "--password", "*", "migration-spike"])
    home = Path("/home/migration-spike/.ssh")
    home.mkdir(mode=0o700)
    w.write_private(home / "authorized_keys",
                    'restrict,port-forwarding,permitopen="127.0.0.1:8006" ' + key + "\n")
    w.cmd(["chown", "-R", "migration-spike:migration-spike", str(home)])
    Path("/etc/ssh/sshd_config.d/95-migration-spike.conf").write_text(
        "Match User migration-spike\n    AllowTcpForwarding local\n    PermitListen none\n"
        "    MaxSessions 0\n    PasswordAuthentication no\n    KbdInteractiveAuthentication no\n"
        "    AllowAgentForwarding no\n    X11Forwarding no\n    PermitTTY no\n"
        "    ForceCommand /usr/sbin/nologin\nMatch all\n")
    w.cmd(["sshd", "-t"])
    w.cmd(["systemctl", "reload", "ssh"])
    Path("/etc/default/pveproxy").write_text("LISTEN_IP=127.0.0.1\n")
    w.cmd(["systemctl", "restart", "pveproxy"])
    listeners = w.cmd(["ss", "-Hlnt"]).stdout.splitlines()
    api = [line for line in listeners if ":8006 " in line]
    if len(api) != 1 or "127.0.0.1:8006" not in api[0]:
        raise RuntimeError("destination API is not exclusively loopback-bound: " + str(api))
    return {"destination_api_listeners": api, "only_forward_target": "127.0.0.1:8006",
            "ssh_account_shell_sessions": 0, "iroh_used": False}


def tunnel_start(marker, payload):
    fixed(marker, "a")
    if (w.STATE / "tunnel.json").exists():
        raise RuntimeError("owned tunnel state already exists; reconcile it before another start")
    with socket.socket() as connection:
        connection.settimeout(3)
        direct = connection.connect_ex(("172.31.77.2", 8006))
    if direct == 0:
        raise RuntimeError("destination API unexpectedly reachable directly")
    key = payload["host_public_key"]
    if not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\n]*)?", key):
        raise RuntimeError("invalid pinned peer host key")
    known = w.STATE / "tunnel-known-hosts"
    if known.exists():
        if known.read_text() != "172.31.77.2 " + key + "\n":
            raise RuntimeError("peer host pin changed")
    else:
        w.write_private(known, "172.31.77.2 " + key + "\n")
    args = ["ssh", "-F", "/dev/null", "-N", "-T", "-i", str(w.STATE / "tunnel-key"),
            "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
            "-o", "ForwardAgent=no", "-o", "ExitOnForwardFailure=yes",
            "-o", "StrictHostKeyChecking=yes", "-o", "GlobalKnownHostsFile=/dev/null",
            "-o", "UserKnownHostsFile=" + str(known), "-o", "ConnectTimeout=10",
            "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2",
            "-L", "127.0.0.1:18006:127.0.0.1:8006", "migration-spike@172.31.77.2"]
    with (w.STATE / "tunnel.log").open("a") as log:
        process = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                   start_new_session=True)
    state = {"pid": process.pid, "argv": args,
             "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}
    path = w.STATE / "tunnel.json"
    w.write_private(path, json.dumps(state))
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("owned SSH tunnel exited: " + (w.STATE / "tunnel.log").read_text()[-2000:])
        try:
            import ssl
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
            context.check_hostname = False
            context.verify_mode = ssl.CERT_NONE
            with socket.create_connection(("127.0.0.1", 18006), timeout=5) as raw:
                with context.wrap_socket(raw, server_hostname="fixture") as tls:
                    actual = w.hashlib.sha256(tls.getpeercert(binary_form=True)).hexdigest()
                    if actual != payload["fingerprint"]:
                        raise RuntimeError("tunnel TLS peer pin mismatch")
            return {"pid": process.pid, "direct_api_refused": True, "tls_pin_verified": True,
                    "transport": "temporary scoped SSH local forward", "iroh_used": False}
        except (OSError, TimeoutError):
            time.sleep(.5)
    raise RuntimeError("owned SSH tunnel did not become ready")


def tunnel_stop(marker):
    fixed(marker, "a")
    path = w.STATE / "tunnel.json"
    if not path.exists():
        return {"already_absent": True}
    state = json.loads(path.read_text())
    if state["boot_id"] != Path("/proc/sys/kernel/random/boot_id").read_text().strip():
        # A PID number can be reused after reboot. The owned process belonged
        # to the previous kernel boot and must never signal a new process.
        path.unlink()
        return {"owned_tunnel_stopped": True, "terminated_by_confirmed_host_reboot": True}
    process = Path(f"/proc/{state['pid']}/cmdline")
    if process.exists() and process.read_bytes().split(b"\0")[:-1] == [a.encode() for a in state["argv"]]:
        os.kill(state["pid"], signal.SIGTERM)
        for _ in range(50):
            if not process.exists():
                break
            time.sleep(.1)
        else:
            # A zombie no longer has a cmdline and cannot listen/forward.
            if process.read_bytes():
                raise RuntimeError("owned tunnel did not terminate")
    elif process.exists() and process.read_bytes():
        raise RuntimeError("owned tunnel PID changed identity; refusing to signal")
    path.unlink()
    return {"owned_tunnel_stopped": True}


def image_prepare(marker, payload):
    fixed(marker, "a")
    import prepare_guest_image as image
    def binding(environment, guid):
        w.attest({"nonce": environment})
        if environment != marker["nonce"] or w.cmd(["zpool", "get", "-Hp", "-o", "value", "guid", "labpool"]).stdout.strip() != guid:
            raise RuntimeError("qualification image pool binding changed")
    image.binding = binding
    guid = w.cmd(["zpool", "get", "-Hp", "-o", "value", "guid", "labpool"]).stdout.strip()
    return image.prepare(marker["nonce"], guid, payload["parent_sha256"])


def generated_user_schema(text):
    if re.search(r"(?m)^user\s*:", text):
        raise RuntimeError("generated cloud-init user-data contains deprecated scalar user; no boot attempted")
    if not re.search(r"(?m)^users:\s*\n[ \t]+-[ \t]+default[ \t]*$", text):
        raise RuntimeError("generated cloud-init user-data lacks the image default users-list contract")
    return {"schema": "users-list/default", "sha256": w.hashlib.sha256(text.encode()).hexdigest()}


def guest_create(marker, payload):
    fixed(marker, "a")
    vmid = payload["vmid"]
    if vmid != 9002 or any(int(v["vmid"]) == vmid for v in w.pve("/cluster/resources", type="vm")):
        raise RuntimeError("bootable fixture requires unused exact VMID 9002")
    digest = payload["image_sha256"]
    if not re.fullmatch("[a-f0-9]{64}", digest):
        raise RuntimeError("invalid pinned boot image")
    image = Path("/var/lib/pve-dev-images") / (digest + ".qcow2")
    if image.stat().st_mode & 0o222 or w.sha_file(image) != digest:
        raise RuntimeError("boot image identity changed")
    key = payload["ssh_public_key"]
    if not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\n]*)?", key):
        raise RuntimeError("invalid disposable guest public key")
    keyfile = w.STATE / "guest-key.pub"
    w.write_private(keyfile, key + "\n")
    w.pve(f"/nodes/{marker['node']}/qemu", "create", vmid=vmid,
          name="bootable-migration-guest", memory=1024, cores=4, kvm=0, cpu="qemu64",
          scsihw="virtio-scsi-pci", onboot=0, protection=1, serial0="socket", vga="serial0",
          net0="virtio=02:50:90:00:00:02,bridge=vmbr77", smbios1="uuid=" + str(uuid.uuid4()))
    w.cmd(["qm", "disk", "import", str(vmid), str(image), "src-alloc", "--format", "raw"], timeout=600)
    w.pve(f"/nodes/{marker['node']}/qemu/{vmid}/config", "set",
          scsi0="src-alloc:vm-9002-disk-0", boot="order=scsi0", ide2="src-alloc:cloudinit",
          citype="nocloud", ciupgrade=0, ipconfig0="ip=10.77.0.12/24",
          nameserver="10.77.0.1", searchdomain="lab.invalid",
          cicustom="meta=spike-snippets:snippets/identity.yaml")
    w.cmd(["qm", "set", str(vmid), "--sshkeys", str(keyfile)])
    w.cmd(["qm", "disk", "resize", str(vmid), "scsi0", "4G"])
    w.cmd(["zfs", "set", "org.pve-dev:spike-owner=" + marker["nonce"], "labpool/alloc/vm-9002-disk-0"])
    w.cmd(["qm", "cloudinit", "update", str(vmid)])
    user_data = w.cmd(["qm", "cloudinit", "dump", str(vmid), "user"]).stdout
    schema = generated_user_schema(user_data)
    # This fixture has only a fresh public SSH key, never a password/private
    # credential. Retaining the exact generated text makes schema/hash proof
    # independently reproducible without relaxing cloud-init readiness.
    return {**w.inspect(marker, vmid), "generated_user_data": {**schema, "content": user_data}}


def inventory(marker):
    fixed(marker)
    vms = w.pve("/cluster/resources", type="vm")
    return {"vms": vms,
            "configs": {str(vm["vmid"]): w.pve(f"/nodes/{marker['node']}/qemu/{vm['vmid']}/config", current=1) for vm in vms},
            "volumes": w.cmd(["zfs", "list", "-Hp", "-t", "volume,snapshot", "-o", "name,guid,used,referenced,logicalreferenced", "-r", "labpool/alloc"]).stdout.splitlines()}


def submit_receipt(marker, payload):
    fixed(marker, "a")
    job = payload["receipt_id"]
    if not re.fullmatch("[a-f0-9]{32}", job): raise RuntimeError("invalid fixed receipt identity")
    path = w.STATE / ("receipt-" + job + ".json")
    if path.exists(): raise RuntimeError("fixture refuses native migration resubmission for existing receipt")
    pending = w.STATE / ("receipt-" + job + ".pending")
    try:
        w.write_private(pending, "native acceptance may be uncertain until receipt is persisted\n")
    except FileExistsError:
        raise RuntimeError("fixture refuses native migration resubmission after uncertain acceptance")
    result = w.migrate(marker, payload)
    w.write_private(path, json.dumps(result))
    return result


def main():
    raw = sys.stdin.read(256 * 1024 + 1)
    if len(raw) > 256 * 1024: raise RuntimeError("oversized fixed fixture request")
    payload = json.loads(raw)
    if payload.get("token"):
        w.SECRETS.extend([payload["token"], payload["token"].rsplit("=", 1)[-1]])
    marker = w.attest(payload)
    action = payload["action"]
    if action == "qualification_enable": return enable(marker)
    if action == "snippet_stage": return snippet(marker)
    if action == "tunnel_key": return tunnel_key(marker)
    if action == "tunnel_target": return tunnel_target(marker, payload)
    if action == "tunnel_start": return tunnel_start(marker, payload)
    if action == "tunnel_stop": return tunnel_stop(marker)
    if action == "tunnel_account_remove":
        fixed(marker, "b")
        w.cmd(["userdel", "--remove", "migration-spike"])
        Path("/etc/ssh/sshd_config.d/95-migration-spike.conf").unlink()
        w.cmd(["systemctl", "reload", "ssh"])
        return {"temporary_ssh_account_removed": w.cmd(["getent", "passwd", "migration-spike"], allowed=(0, 2)).returncode == 2}
    if action == "image_prepare": return image_prepare(marker, payload)
    if action == "guest_create": return guest_create(marker, payload)
    if action == "inventory": return inventory(marker)
    if action == "copy_progress":
        fixed(marker, "a")
        tcp = w.cmd(["ss", "-Hnti", "dst", "172.31.77.2"]).stdout
        acked = [int(value) for value in re.findall(r"bytes_acked:(\d+)", tcp)]
        return {"private_ssh_tcp": tcp, "largest_bytes_acked": max(acked, default=0)}
    if action == "network_start":
        fixed(marker)
        w.cmd(["systemctl", "start", "migration-fixture-network.service"])
        return {"persistent_private_link_and_guest_bridge": True}
    if action == "guest_start":
        fixed(marker)
        w.cmd(["qm", "start", str(payload["vmid"])], timeout=180)
        return {"qemu_started": True, "guest_os_ready": False}
    if action == "guest_stop":
        fixed(marker)
        w.cmd(["qm", "shutdown", str(payload["vmid"]), "--timeout", "180"], timeout=210)
        status = w.pve(f"/nodes/{marker['node']}/qemu/{payload['vmid']}/status/current")
        if status["status"] != "stopped": raise RuntimeError("guest did not shut down cleanly")
        return {"clean_shutdown": True}
    if action == "qemu_stop":
        fixed(marker, "b")
        w.cmd(["qm", "stop", str(payload["vmid"])], timeout=120)
        return {"qemu_stopped": True, "guest_os_boot_verified": False}
    if action == "migrate" and payload.get("receipt_id"):
        return submit_receipt(marker, payload)
    if action == "receipt":
        fixed(marker, "a")
        job = payload["receipt_id"]
        if not re.fullmatch("[a-f0-9]{32}", job): raise RuntimeError("invalid receipt identity")
        path = w.STATE / ("receipt-" + job + ".json")
        if not path.exists() and path.with_suffix(".pending").exists():
            return {"acceptance": "unknown", "submission_must_not_be_repeated": True}
        return json.loads(path.read_text())
    if action == "reboot_source":
        fixed(marker, "a")
        for vm in w.pve("/cluster/resources", type="vm"):
            if vm["status"] != "stopped": raise RuntimeError("source has a runnable guest before reboot fence test")
        w.cmd(["systemctl", "reboot", "--no-block"])
        return {"reboot_requested": True}
    if action == "boot_identity":
        fixed(marker)
        return {"boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "machine_id_sha256": w.sha_file("/etc/machine-id"), "node": marker["node"],
                "inventory": inventory(marker)}
    sys.stdin = io.StringIO(raw)
    return w.main()


if __name__ == "__main__":
    try:
        result = main()
        text = json.dumps(result)
        print(text if isinstance(result, dict) and "token" in result else w.sanitize(text))
    except Exception as error:
        print(w.sanitize(str(error)), file=sys.stderr)
        raise SystemExit(1)
