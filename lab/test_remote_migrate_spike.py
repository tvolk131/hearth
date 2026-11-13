import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import remote_migrate_spike as spike
import remote_migrate_worker as worker


class MigrationSpikeSafetyTests(unittest.TestCase):
    def test_accepted_task_is_polled_through_asynchronous_failure(self):
        running = {"status": "running"}
        failed = {"status": "stopped", "exitstatus": "target VM already exists"}
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(spike, "call", side_effect=[running, failed]) as calls, mock.patch.object(spike.lab, "space"), mock.patch.object(spike.time, "sleep"):
                result = spike.await_task(Path(directory), "nonce", "UPID:fixture", Path(directory), "task", 60)
            self.assertEqual(result, failed)
            self.assertEqual(calls.call_count, 2)
            self.assertTrue((Path(directory) / "task.json").is_file())

    def test_pvesh_set_prose_is_success_not_json_or_absence(self):
        result = mock.Mock(stdout="update VM 9001: -scsi0 src-alloc:vm-9001-disk-0\n")
        with mock.patch.object(worker, "cmd", return_value=result):
            self.assertIsNone(worker.pve("/nodes/pve-mig-a/qemu/9001/config", "set", scsi0="fixture"))

    def test_directory_storage_path_property_is_distinct_from_api_route(self):
        with mock.patch.object(worker, "cmd", return_value=mock.Mock(stdout="")) as command:
            worker.pve("/storage", "create", storage="spike-snippets", type="dir", path="/var/lib/pve-spike-snippets", content="snippets")
        self.assertIn("/storage", command.call_args.args[0])
        self.assertIn("--path", command.call_args.args[0])
        self.assertIn("/var/lib/pve-spike-snippets", command.call_args.args[0])

    def test_pvesh_create_waits_for_exact_node_task_receipt(self):
        upid = "UPID:pve-mig-a:00000001:00000002:00000003:qmcreate:9001:root@pam:"
        create = mock.Mock(stdout='fixture log\n"' + upid + '"\n')
        done = mock.Mock(stdout='{"status":"stopped","exitstatus":"OK"}')
        with mock.patch.object(worker, "cmd", side_effect=[create, done]):
            self.assertEqual(worker.pve("/nodes/pve-mig-a/qemu", "create", vmid=9001), upid)

    def test_private_state_files_are_exclusive_and_0600(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            spike.private_json(path, {"test": True})
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                spike.private_json(path, {"overwrite": True})
            secret = Path(directory) / "token.json"
            worker.write_private(secret, "fixture-secret")
            self.assertEqual(secret.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                worker.write_private(secret, "other")

    def test_only_fresh_local_lab_pair_roles_are_accepted(self):
        for role in ("source-production", "pve1", ""):
            with self.assertRaises(ValueError):
                spike.qemu_command(spike.lab.HOME / "migration-test" / "a", role, 10001, 10002, 10003, Path("/tmp/test.sock"))
        with self.assertRaises(ValueError):
            spike.qemu_command(Path("/tmp/arbitrary"), "a", 10001, 10002, 10003, Path("/tmp/test.sock"))

    def test_management_and_peer_transport_bind_loopback_without_host_devices(self):
        for role in ("a", "b"):
            command = spike.qemu_command(spike.lab.HOME / "migration-test" / role, role, 10001, 10002, 10003, Path("/tmp/test.sock"))
            forwards = [arg for arg in command if "hostfwd" in arg]
            self.assertEqual(len(forwards), 1)
            self.assertIn("hostfwd=tcp:127.0.0.1:10002-:22", forwards[0])
            self.assertIn("hostfwd=tcp:127.0.0.1:10003-:8006", forwards[0])
            link = next(arg for arg in command if arg.startswith("socket,"))
            self.assertIn("127.0.0.1:10001", link)
            self.assertFalse(any("/dev/disk" in arg or "tap," in arg or "bridge," in arg for arg in command))
            self.assertEqual(sum("format=qcow2,if=none,id=data-" in arg for arg in command), 2)
            self.assertIn("virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56,addr=0x2", command)
            peer = next(arg for arg in command if arg.startswith("virtio-net-pci,netdev=migration,"))
            self.assertIn("02:50:77:00:00:01" if role == "a" else "02:50:77:00:00:02", peer)

    def test_token_redaction_covers_value_and_full_header(self):
        with mock.patch.object(worker, "SECRETS", ["root@pam!job=abc-secret", "abc-secret"]):
            text = worker.sanitize("apitoken=root@pam!job=abc-secret bare=abc-secret")
            self.assertNotIn("abc-secret", text)
        self.assertNotIn("token-secret", worker.sanitize("PVEAPIToken=root@pam!job=token-secret"))

    def test_peer_fingerprint_mismatch_sends_no_authentication(self):
        connection = mock.Mock()
        connection.sock.getpeercert.return_value = b"foreign certificate"
        with mock.patch.object(worker.http.client, "HTTPSConnection", return_value=connection):
            with self.assertRaisesRegex(RuntimeError, "fingerprint mismatch before authentication"):
                worker.api("172.31.77.2", "0" * 64, "/version", token_value="private-token")
        connection.request.assert_not_called()
        connection.close.assert_called_once()

    def test_worker_refuses_unattested_or_wrong_nonce_host(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "marker.json"
            marker.write_text('{"nonce":"expected","node":"pve-mig-a","role":"a"}')
            with mock.patch.object(worker, "MARKER", marker), mock.patch.object(worker.os, "geteuid", return_value=0):
                with self.assertRaisesRegex(RuntimeError, "fixture identity mismatch"):
                    worker.attest({"nonce": "foreign"})

    def test_worker_refuses_real_or_unlisted_vm_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "marker.json"
            marker.write_text('{"nonce":"expected","node":"pve-mig-a","role":"a"}')
            result = mock.Mock(stdout="pve-mig-a\n")
            with mock.patch.object(worker, "MARKER", marker), mock.patch.object(worker.os, "geteuid", return_value=0), mock.patch.object(worker, "cmd", return_value=result):
                for vmid in (101, 102, 103, 9000, 9999):
                    with self.assertRaisesRegex(RuntimeError, "toy VM IDs"):
                        worker.attest({"nonce": "expected", "vmid": vmid})


if __name__ == "__main__":
    unittest.main()
