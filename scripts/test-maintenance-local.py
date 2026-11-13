"""Real local-process paired backup drill; simulated external world only."""
import argparse
import contextlib
import json
from pathlib import Path
import secrets
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parent.parent / "target/debug")
    args = parser.parse_args()
    binary = args.bin_dir.resolve()
    temp_parent = Path("/private/tmp") if Path("/private/tmp").is_dir() else Path("/tmp").resolve()
    with tempfile.TemporaryDirectory(prefix="pved-maintenance-", dir=temp_parent) as temporary, contextlib.ExitStack() as cleanup:
        root = Path(temporary).resolve()
        children = []

        def start(name, *parts):
            log = cleanup.enter_context((root / f"{name}.log").open("w"))
            child = subprocess.Popen([str(p) for p in parts], stdout=log, stderr=log)
            children.append(child)
            return child

        def stop(child):
            if child.poll() is None:
                child.send_signal(signal.SIGCONT)
                child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)

        def close():
            for child in reversed(children):
                stop(child)

        cleanup.callback(close)

        def command(*parts):
            return subprocess.run([str(p) for p in parts], check=True, capture_output=True, text=True, timeout=30)

        def wait(predicate):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, subprocess.SubprocessError):
                    pass
                time.sleep(0.1)
            raise TimeoutError("local fixture failed to become ready")

        client = binary / "pve-devctl"
        daemon_bin = binary / "pve-devd"
        helper_bin = binary / "pve-dev-host"
        ops = binary / "pve-dev-ops"
        controller = root / "controller"
        helper_state = root / "helper"
        world = root / "world.json"
        socket = root / "host.sock"
        environment = secrets.token_hex(16)
        identity = root / "admin.key"
        admin = json.loads(command(client, "identity", "new", "--path", identity).stdout)["endpoint_id"]
        helper = start("helper", helper_bin, "serve", "--state-dir", helper_state, "--socket", socket,
                       "--simulate", "--environment", environment, "--world", world)
        wait(lambda: socket.exists())
        command(daemon_bin, "init", "--data-dir", controller, "--admin", admin, "--environment", environment,
                "--host-socket", socket, "--backend", "simulated")
        daemon = start("daemon", daemon_bin, "serve", "--data-dir", controller)
        descriptor = controller / "server.json"
        wait(lambda: descriptor.exists())
        server_id = json.loads(descriptor.read_text())["endpoint"]["id"]
        profile = root / "profile.json"
        command(client, "profile", "--identity", identity, "--server", descriptor, "--expected-server", server_id, "--output", profile)

        def invoke(request):
            result = subprocess.run([str(client), "--profile", str(profile), "rpc"], input=json.dumps(request),
                                    check=True, capture_output=True, text=True, timeout=30)
            response = json.loads(result.stdout)
            assert response["ok"], response
            return response["result"]

        wait(lambda: invoke({"action": "whoami"})["ready"])
        helper.send_signal(signal.SIGSTOP)
        accepted = invoke({"action": "create_vm", "request_id": "offline-backup-durable-create", "name": "restore-me",
                           "disk_mib": 32, "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None})
        assert accepted["status"] == "queued"
        daemon.kill()
        daemon.wait(timeout=10)
        # The simulated helper has not executed any external effect. A real
        # deployment must separately inspect outliving Proxmox tasks.
        helper.kill()
        helper.wait(timeout=10)
        pool_guid = json.loads(world.read_text())["pool_guid"]
        pins = ["--expected-environment", environment, "--expected-server", server_id, "--expected-pool-guid", pool_guid]
        receipt = json.loads(command(ops, "backup", "--controller-dir", controller, "--helper-dir", helper_state,
                                     "--simulated-world", world, "--output", root / "backup", "--external-effects-quiesced", *pins).stdout)
        command(ops, "verify", "--bundle", root / "backup", "--expected-bundle", receipt["payload_sha256"], *pins)
        restored = json.loads(command(ops, "restore", "--bundle", root / "backup", "--output", root / "restore",
                                      "--expected-bundle", receipt["payload_sha256"], *pins).stdout)
        assert restored["quarantined"] and not restored["activates_workers"]
        bundle = json.loads((root / "backup/bundle.json").read_text())
        saved = bundle["payload"]["catalog"]["operations"][accepted["operation_id"]]
        assert saved["public"]["status"] == "queued" and saved["public"]["vm_id"] == accepted["vm_id"]
        for parts in [
            [daemon_bin, "serve", "--data-dir", root / "restore/controller"],
            [helper_bin, "serve", "--state-dir", root / "restore/helper", "--socket", root / "restore.sock",
             "--simulate", "--environment", environment, "--world", root / "restore/simulated-world.json"],
        ]:
            result = subprocess.run([str(p) for p in parts], capture_output=True, text=True, timeout=10)
            assert result.returncode and "quarantin" in result.stderr, result.stderr
        print(json.dumps({"complete": True, "backend": "simulated", "production_contacted": False,
                          "checks": ["sigkill_after_acceptance", "paired_offline_export", "pinned_bundle_verify",
                                     "accepted_operation_identity_preserved", "restored_daemon_and_helper_refuse_activation"]}, indent=2))


if __name__ == "__main__":
    main()
