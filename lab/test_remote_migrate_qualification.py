import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import remote_migrate_qualification_worker as worker


class QualificationSafetyTests(unittest.TestCase):
    def test_generated_user_schema_rejects_deprecated_override_even_with_users_list(self):
        with self.assertRaisesRegex(RuntimeError, "deprecated scalar user"):
            worker.generated_user_schema("#cloud-config\nuser: dev\nusers:\n  - default\n")
        with self.assertRaisesRegex(RuntimeError, "lacks.*users-list"):
            worker.generated_user_schema("#cloud-config\nssh_authorized_keys:\n  - fixture\n")
        self.assertEqual(worker.generated_user_schema("#cloud-config\nusers:\n  - default\n")["schema"], "users-list/default")

    def test_lost_ack_witness_refuses_native_resubmission(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            (state / "qualification.json").write_text("{}")
            payload = {"receipt_id": "a" * 32}
            receipt = {"upid": "UPID:pve-mig-a:fixture"}
            with mock.patch.object(worker.w, "STATE", state), mock.patch.object(worker.w, "migrate", return_value=receipt) as migrate:
                self.assertEqual(worker.submit_receipt({"role": "a"}, payload), receipt)
                with self.assertRaisesRegex(RuntimeError, "resubmission"):
                    worker.submit_receipt({"role": "a"}, payload)
                migrate.assert_called_once()
            path = state / ("receipt-" + "a" * 32 + ".json")
            self.assertEqual(json.loads(path.read_text()), receipt)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_receipt_path_never_accepts_arbitrary_paths(self):
        with mock.patch.object(worker, "fixed"):
            for job in ("../other", "a" * 31, "A" * 32, "a" * 33):
                with self.assertRaisesRegex(RuntimeError, "receipt identity"):
                    worker.submit_receipt({"role": "a"}, {"receipt_id": job})

    def test_uncertain_acceptance_is_not_resubmitted_when_receipt_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            (state / "qualification.json").write_text("{}")
            payload = {"receipt_id": "b" * 32}
            with mock.patch.object(worker.w, "STATE", state), mock.patch.object(worker.w, "migrate", side_effect=RuntimeError("connection lost")) as migrate:
                with self.assertRaisesRegex(RuntimeError, "connection lost"):
                    worker.submit_receipt({"role": "a"}, payload)
                with self.assertRaisesRegex(RuntimeError, "uncertain acceptance"):
                    worker.submit_receipt({"role": "a"}, payload)
                migrate.assert_called_once()

    def test_loopback_mode_requires_owned_tunnel_before_api_submission(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(worker.w, "STATE", Path(directory)), mock.patch.object(worker.w, "token", return_value={"token": "source-fixture"}), mock.patch.object(worker.w, "api") as api:
                with self.assertRaisesRegex(RuntimeError, "owned fixed loopback tunnel"):
                    worker.w.migrate({"role": "a", "node": "pve-mig-a"}, {"vmid": 9001, "target_vmid": 9001,
                       "token": "root@pam!target9001=00000000-0000-4000-8000-000000000000",
                       "fingerprint": "a" * 64, "loopback_tunnel": True})
                api.assert_not_called()

    def test_remote_endpoint_fields_cannot_override_fixed_private_host(self):
        for token in ("root@pam!target9001=fixture,host=arbitrary", "root@pam!foreign=00000000-0000-4000-8000-000000000000"):
            with mock.patch.object(worker.w, "token") as source_token, mock.patch.object(worker.w, "api") as api:
                with self.assertRaisesRegex(RuntimeError, "disposable target credential shape"):
                    worker.w.migrate({"role": "a"}, {"vmid": 9001, "target_vmid": 9001,
                       "token": token, "fingerprint": "a" * 64})
                source_token.assert_not_called()
                api.assert_not_called()

    def test_qualification_refuses_unenabled_or_wrong_role(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(worker.w, "STATE", Path(directory)):
                with self.assertRaisesRegex(RuntimeError, "not an expanded"):
                    worker.fixed({"role": "a"})
            with self.assertRaisesRegex(RuntimeError, "wrong qualification fixture role"):
                worker.fixed({"role": "b"}, "a")

    def test_tunnel_cleanup_never_signals_a_reused_pid_after_host_reboot(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            (state / "qualification.json").write_text("{}")
            (state / "tunnel.json").write_text(json.dumps({"pid": 12345, "argv": ["ssh", "fixture"], "boot_id": "old-boot"}))
            original = Path.read_text
            def read(path, *args, **kwargs):
                return "new-boot" if str(path) == "/proc/sys/kernel/random/boot_id" else original(path, *args, **kwargs)
            with mock.patch.object(worker.w, "STATE", state), mock.patch.object(Path, "read_text", autospec=True, side_effect=read), mock.patch.object(worker.os, "kill") as kill:
                result = worker.tunnel_stop({"role": "a"})
                self.assertTrue(result["terminated_by_confirmed_host_reboot"])
                kill.assert_not_called()
            self.assertFalse((state / "tunnel.json").exists())


if __name__ == "__main__": unittest.main()
