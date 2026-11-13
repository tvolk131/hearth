import argparse
import contextlib
import json
import os
import pathlib
import secrets
import signal
import socket
import subprocess
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description="Exercise real controller processes with a simulated host; never contacts Proxmox")
    parser.add_argument("--bin-dir", type=pathlib.Path, default=ROOT / "target/debug")
    parser.add_argument("--swift", action="store_true")
    parser.add_argument("--quota", action="store_true", help="Also exercise policy editing over real local iroh processes; simulated storage only")
    parser.add_argument("--user-deletion", action="store_true", help="Exercise explicit VM cleanup followed by empty-member deletion and revoked-device refusal")
    parser.add_argument("--serve", action="store_true", help="Keep the simulated demo services alive after testing, for manual GUI use")
    arguments = parser.parse_args()
    binaries = arguments.bin_dir.resolve()
    environment = secrets.token_hex(16)
    checks = []
    with tempfile.TemporaryDirectory(prefix="pved-", dir="/tmp") as temporary, contextlib.ExitStack() as cleanup:
        directory = pathlib.Path(temporary)
        children = []

        def start(name, command):
            logfile = cleanup.enter_context((directory / f"{name}-{len(children)}.log").open("w+"))
            process = subprocess.Popen([str(part) for part in command], stdout=logfile, stderr=subprocess.STDOUT)
            children.append((process, logfile))
            return process

        def stop(process):
            if process.poll() is None:
                process.send_signal(signal.SIGCONT)
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)

        def stop_all():
            for process, _ in reversed(children):
                stop(process)

        cleanup.callback(stop_all)

        def command(*parts):
            return subprocess.run([str(part) for part in parts], check=True, capture_output=True, text=True, timeout=45)

        def wait_for(predicate, description):
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                try:
                    result = predicate()
                    if result:
                        return result
                except (OSError, RuntimeError, json.JSONDecodeError):
                    pass
                time.sleep(0.2)
            for process, logfile in children:
                logfile.flush()
                logfile.seek(0)
                print(f"Process {process.pid}, exit {process.poll()}:\n{logfile.read()}")
            raise TimeoutError(description)

        identities = {}
        profiles = {}
        client = binaries / "pve-devctl"
        for name in ("erin", "frank", "unknown"):
            identity = directory / f"{name}.key"
            identities[name] = json.loads(command(client, "identity", "new", "--path", identity).stdout)["endpoint_id"]
            profiles[name] = directory / f"{name}.json"

        host_command = [binaries / "pve-dev-host", "serve", "--state-dir", directory / "host", "--socket", directory / "host.sock", "--simulate", "--environment", environment]
        helper = start("helper", host_command)
        wait_for(lambda: (directory / "host.sock").exists(), "helper socket")
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        state = directory / "controller"
        command(binaries / "pve-devd", "init", "--data-dir", state, "--admin", identities["erin"], "--environment", environment,
                "--host-socket", directory / "host.sock", "--backend", "simulated", "--port", port)
        daemon_command = [binaries / "pve-devd", "serve", "--data-dir", state]
        daemon = start("daemon", daemon_command)
        descriptor = state / "server.json"
        wait_for(lambda: descriptor.exists(), "controller descriptor")
        server_id = json.loads(descriptor.read_text())["endpoint"]["id"]
        for name in identities:
            command(client, "profile", "--identity", directory / f"{name}.key", "--server", descriptor,
                    "--expected-server", server_id, "--output", profiles[name])

        def invoke(name, request, expected=None):
            process = subprocess.run([str(client), "--profile", str(profiles[name]), "rpc"], input=json.dumps(request), capture_output=True, text=True, timeout=35)
            if not process.stdout:
                raise RuntimeError(process.stderr)
            response = json.loads(process.stdout)
            if expected:
                assert process.returncode == 2 and response["error"]["code"] == expected, response
                return response
            if process.returncode or not response["ok"]:
                raise RuntimeError(str(response))
            return response["result"]

        def verified(name, operation):
            def observe():
                record = invoke(name, {"action": "get_operation", "operation_id": operation["operation_id"]})
                if record["status"] == "blocked":
                    raise AssertionError(record)
                return record if record["status"] == "verified" else None
            return wait_for(observe, "operation verification")

        wait_for(lambda: invoke("erin", {"action": "whoami"})["ready"], "bootstrap reservation")
        invoke("unknown", {"action": "dashboard"}, "forbidden")
        verified("erin", invoke("erin", {"action": "create_user", "request_id": "person", "user_id": "frank", "budget_mib": 128,
                                        "min_memory_budget_mib": 256, "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4}))
        invoke("erin", {"action": "enroll_device", "user_id": "frank", "endpoint_id": identities["frank"], "label": "Test device"})
        checks.append("real_process_authentication_and_async_bootstrap")
        request = {"action": "create_vm", "request_id": "durable-create", "name": "dev-frank", "disk_mib": 32,
                   "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None}
        helper.send_signal(signal.SIGSTOP)
        try:
            original = invoke("frank", request)
            assert original["status"] == "queued"
            daemon.kill()
            daemon.wait(timeout=10)
        finally:
            helper.send_signal(signal.SIGCONT)
        daemon = start("daemon-restarted", daemon_command)
        resumed = wait_for(lambda: invoke("frank", request), "restarted daemon")
        assert resumed["operation_id"] == original["operation_id"]
        assert resumed["vm_id"] == original["vm_id"]
        verified("frank", resumed)
        checks.append("sigkill_after_durable_acceptance_replays_same_intent")
        helper.kill()
        helper.wait(timeout=10)
        helper = start("helper-restarted", host_command)
        verified("frank", invoke("frank", {"action": "vm_action", "request_id": "after-helper-restart", "vm_id": original["vm_id"], "kind": "start"}))
        checks.append("helper_journal_and_backend_survive_process_restart")
        invoke("frank", {**request, "request_id": "spoof", "owner": "erin"}, "forbidden")
        if arguments.quota:
            policy = {"budget_mib": 256, "min_memory_budget_mib": 256,
                      "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4}
            edit = {"action": "update_user_policy", "request_id": "durable-policy", "user_id": "frank",
                    "expected_revision": 0, "policy": policy}
            invoke("frank", edit, "forbidden")
            world_file = directory / "host/simulated-world.json"
            original_config = json.loads(world_file.read_text())["vms"][str(original["vm_id"])]["config"]
            helper.send_signal(signal.SIGSTOP)
            try:
                # Exercise the typed CLI flags, then retry through raw JSON to
                # establish that both interfaces identify the same request.
                accepted = json.loads(command(client, "--profile", profiles["erin"], "update-user-policy",
                    "--request-id", "durable-policy", "--user-id", "frank", "--expected-revision", "0",
                    "--budget-mib", "256", "--min-memory-budget-mib", "256", "--max-memory-budget-mib", "1024",
                    "--max-cores", "8", "--max-vms", "4").stdout)["result"]
                current = invoke("frank", {"action": "whoami"})
                assert not current["ready"] and current["budget_mib"] == 128
                invoke("frank", {**request, "name": "during-policy", "request_id": "during-policy"}, "conflict")
                daemon.kill()
                daemon.wait(timeout=10)
            finally:
                helper.send_signal(signal.SIGCONT)
            # The daemon attests helper health before opening its endpoint.
            # Resume the helper before restarting, as in the create drill.
            daemon = start("daemon-policy-restarted", daemon_command)
            replay = wait_for(lambda: invoke("erin", edit), "restarted policy acceptance")
            assert replay["operation_id"] == accepted["operation_id"]
            verified("erin", replay)
            current = invoke("frank", {"action": "whoami"})
            assert current["ready"] and current["budget_mib"] == 256 and current["policy_revision"] == 1
            checks.append("quota_typed_cli_authorization_and_lost_ack_restart")

            shrink = {"action": "update_user_policy", "request_id": "contract-unused", "user_id": "frank",
                      "expected_revision": 1, "policy": {**policy, "budget_mib": 64}}
            invoke("erin", {**shrink, "request_id": "stale-policy", "expected_revision": 0}, "conflict")
            invoke("erin", {**shrink, "request_id": "zero-policy", "policy": {**policy, "budget_mib": 0}}, "invalid")
            second = invoke("frank", {**request, "request_id": "second-quota-vm", "name": "second-quota-vm"})
            verified("frank", second)
            invoke("erin", {**shrink, "request_id": "below-commitments"}, "conflict")
            verified("frank", invoke("frank", {"action": "vm_action", "request_id": "remove-second-quota-vm",
                     "vm_id": second["vm_id"], "kind": "delete"}))
            verified("erin", invoke("erin", shrink))
            current = invoke("frank", {"action": "whoami"})
            assert current["ready"] and current["budget_mib"] == 64 and current["policy_revision"] == 2
            assert json.loads(world_file.read_text())["vms"][str(original["vm_id"])]["config"] == original_config
            checks.append("quota_contraction_refuses_in_use_and_preserves_original_vm")

            helper.kill()
            helper.wait(timeout=10)
            helper = start("helper-policy-restarted", host_command)
            def policy_helper_ready():
                probe = subprocess.run([str(binaries / "pve-dev-host"), "health", "--socket", str(directory / "host.sock")],
                                       capture_output=True, text=True, timeout=8)
                return probe.returncode == 0 and json.loads(probe.stdout)["environment"] == environment
            wait_for(policy_helper_ready, "policy helper restart")
            # A completed operation remains historical after policy revision2;
            # retry cannot reapply its old revision0 resource policy.
            historical = invoke("frank", {"action": "vm_action", "request_id": "after-helper-restart",
                                "vm_id": original["vm_id"], "kind": "start"})
            assert historical["status"] == "verified"
            verified("frank", invoke("frank", {"action": "vm_action", "request_id": "after-policy-stop",
                     "vm_id": original["vm_id"], "kind": "stop"}))
            current = invoke("frank", {"action": "whoami"})
            assert current["budget_mib"] == 64 and current["policy_revision"] == 2
            checks.append("quota_new_policy_and_historical_retry_survive_helper_restart")
            before_properties = json.loads(world_file.read_text())["datasets"]
            verified("erin", invoke("erin", {"action": "update_user_policy", "request_id": "cores-only",
                     "user_id": "frank", "expected_revision": 2,
                     "policy": {**policy, "budget_mib": 64, "max_cores": 9}}))
            current = invoke("frank", {"action": "whoami"})
            assert current["policy_revision"] == 3 and current["max_cores"] == 9
            assert current["budget_mib"] == 64 and json.loads(world_file.read_text())["datasets"] == before_properties
            checks.append("quota_nonstorage_edit_preserves_all_dataset_properties")
        if arguments.user_deletion:
            account = {"action": "create_user", "request_id": "empty-member-fixture", "user_id": "trial-user",
                       "budget_mib": 64, "min_memory_budget_mib": 128, "max_memory_budget_mib": 256,
                       "max_cores": 1, "max_vms": 1}
            created_account = verified("erin", invoke("erin", account))
            invoke("erin", {"action": "enroll_device", "user_id": "trial-user", "endpoint_id": identities["unknown"], "label": "Disposable member"})
            vm = verified("unknown", invoke("unknown", {"action": "create_vm", "request_id": "empty-member-vm", "name": "empty-member-vm",
                          "disk_mib": 16, "memory_mib": 128, "min_memory_mib": 128, "cores": 1, "owner": None}))
            deletion = {"action": "delete_user", "request_id": "empty-member-retire", "user_id": "trial-user", "expected_revision": 0}
            invoke("unknown", deletion, "forbidden")
            invoke("erin", deletion, "conflict")
            assert invoke("unknown", {"action": "whoami"})["ready"]
            verified("unknown", invoke("unknown", {"action": "vm_action", "request_id": "explicit-member-vm-delete", "vm_id": vm["vm_id"], "kind": "delete"}))
            helper.send_signal(signal.SIGSTOP)
            cli = command(client, "--profile", profiles["erin"], "delete-user", "--request-id", "empty-member-retire",
                          "--user-id", "trial-user", "--expected-revision", "0")
            accepted = json.loads(cli.stdout)["result"]
            retired = next(user for user in invoke("erin", {"action": "dashboard"})["users"] if user["user_id"] == "trial-user")
            assert retired["state"] == "deleting" and not retired["ready"]
            invoke("unknown", {"action": "dashboard"}, "forbidden")
            daemon.kill(); daemon.wait(timeout=10)
            helper.send_signal(signal.SIGCONT)
            daemon = start("daemon-empty-account-restarted", daemon_command)
            wait_for(lambda: invoke("erin", {"action": "whoami"}), "daemon restart during account retirement")
            replay = invoke("erin", deletion)
            assert replay["operation_id"] == accepted["operation_id"]
            verified("erin", replay)
            retired = next(user for user in invoke("erin", {"action": "dashboard"})["users"] if user["user_id"] == "trial-user")
            assert retired["state"] == "deleted" and not retired["ready"]
            world = json.loads((directory / "host/simulated-world.json").read_text())
            assert not any(name == "labpool/control/trial-user" or name.startswith("labpool/control/trial-user/") for name in world["datasets"])
            assert "ctl-trial-user" not in world["storages"]
            invoke("unknown", {"action": "dashboard"}, "forbidden")
            invoke("erin", {**account, "request_id": "cannot-reuse-retired-member"}, "conflict")
            historical = invoke("erin", account)
            assert historical["operation_id"] == created_account["operation_id"] and historical["status"] == "verified"
            checks.append("explicit_vm_cleanup_empty_account_deletion_restart_and_revocation")
        if arguments.swift:
            swift_environment = dict(os.environ, PVE_DEV_TEST_CLI=str(client), PVE_DEV_TEST_PROFILE=str(profiles["erin"]))
            subprocess.run(["bash", str(ROOT / "scripts/check-swift.sh")], env=swift_environment, check=True, timeout=180)
            checks.append("swift_client_uses_real_rust_rpc_and_idempotent_mutation")
        invoke("erin", {"action": "revoke_device", "endpoint_id": identities["frank"]})
        invoke("frank", {"action": "dashboard"}, "forbidden")
        checks.append("live_revocation")
        print(json.dumps({"complete": True, "backend": "simulated", "checks": checks}, indent=2), flush=True)
        if arguments.serve:
            print(f"SIMULATED host only. CLI executable: {client}\nGUI profile JSON: {profiles['erin']}\nPress Ctrl-C to stop and remove this temporary demo.", flush=True)
            try:
                while True:
                    signal.pause()
            except KeyboardInterrupt:
                pass


if __name__ == "__main__":
    main()
