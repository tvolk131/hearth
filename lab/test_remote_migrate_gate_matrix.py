import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import remote_migrate_gate_matrix as matrix


class IndependentGateContractTests(unittest.TestCase):
    def test_generated_schema_hash_must_reconstruct_from_actual_text(self):
        text = "#cloud-config\nusers:\n  - default\n"
        value = {**matrix.fixture.generated_user_schema(text), "content": text}
        matrix.generated_schema_proof(value)
        value["sha256"] = "0" * 64
        with self.assertRaisesRegex(RuntimeError, "does not reconstruct"):
            matrix.generated_schema_proof(value)

    def test_generated_schema_label_cannot_hide_deprecated_text(self):
        value = {"schema": "users-list/default", "sha256": "0" * 64,
                 "content": "#cloud-config\nuser: dev\nusers:\n  - default\n"}
        with self.assertRaisesRegex(RuntimeError, "deprecated scalar user"):
            matrix.generated_schema_proof(value)

    def test_running_native_task_never_counts_as_terminal_proof(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            (report / "task.json").write_text(json.dumps({"upid": "fixture", "observations": [{"status": "running"}]}))
            with self.assertRaisesRegex(RuntimeError, "not terminal"):
                matrix.terminal(report, "task", successful=False)

    def test_failed_native_task_never_counts_as_successful_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            (report / "task.json").write_text(json.dumps({"upid": "fixture", "observations": [{"status": "stopped", "exitstatus": "migration aborted"}]}))
            with self.assertRaisesRegex(RuntimeError, "unexpected native task"):
                matrix.terminal(report, "task", successful=True)
            self.assertEqual(matrix.terminal(report, "task", successful=False), "fixture")

    def test_missing_native_exit_status_is_ambiguity_not_expected_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            (report / "task.json").write_text(json.dumps({"upid": "fixture", "observations": [{"status": "stopped"}]}))
            with self.assertRaisesRegex(RuntimeError, "exit status is unknown"):
                matrix.terminal(report, "task", successful=False)

    def test_incomplete_guest_run_cannot_qualify_its_image(self):
        with mock.patch.object(matrix, "read", return_value={"complete": False, "mode": "guest"}):
            with self.assertRaisesRegex(RuntimeError, "guest profile is incomplete"):
                matrix.guest_gate(Path("unused"))

    def test_multithread_fixture_cannot_supply_single_thread_reboot_gate(self):
        summary = {"complete": True, "mode": "fence", "outer_tcg_threading": {"a": "multi", "b": "multi"}, "guest_os_boot_verified": False}
        with mock.patch.object(matrix, "read", return_value=summary):
            with self.assertRaisesRegex(RuntimeError, "threading"):
                matrix.fence_gate(Path("unused"))

    def test_failed_token_cleanup_refuses_qualification(self):
        with mock.patch.object(matrix, "read", return_value={"all_spike_tokens_removed": False}):
            with self.assertRaisesRegex(RuntimeError, "token cleanup"):
                matrix.cleanup(Path("unused"), {"qemu_children_stopped": True})

    def test_pristine_cache_requires_digest_readonly_and_preboot_export(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            cache = home / "physical-trial-image"
            cache.mkdir()
            data = b"pristine fixture bytes"
            digest = hashlib.sha256(data).hexdigest()
            image = cache / (digest + ".qcow2")
            image.write_bytes(data)
            image.chmod(0o444)
            manifest = {"image_sha256": digest, "export_sha256_verified": True,
                        "exported_before_guest_import_or_boot": True, "root_zvol_exported": False,
                        "default_user": "dev"}
            image.with_suffix(".json").write_text(json.dumps(manifest))
            with mock.patch.object(matrix.qualification.lab, "HOME", home):
                self.assertEqual(matrix.qualification.verified_cached_image(digest)[0], image)
                manifest["root_zvol_exported"] = True
                image.with_suffix(".json").write_text(json.dumps(manifest))
                with self.assertRaisesRegex(RuntimeError, "provenance"):
                    matrix.qualification.verified_cached_image(digest)


if __name__ == "__main__": unittest.main()
