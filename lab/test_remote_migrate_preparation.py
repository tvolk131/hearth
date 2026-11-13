import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import remote_migrate_preparation as preparation


class PhysicalPreparationContractTests(unittest.TestCase):
    def write_timeout(self, report, *, volume="vm-9002-cloudinit", reason="failed to read readiness from disk import child: got timeout"):
        upid = "UPID:pve-mig-a:fixture"
        command = {"cmd": "disk-import", "volname": volume, "storage": "dst-alloc"}
        state = {"status": "stopped", "exitstatus": "migration aborted", "log": [
            {"t": "tunnel command '" + json.dumps(command) + "' failed - " + reason}]}
        values = {"bootable-transfer-task": {"upid": upid, "observations": [state]},
                  "lost-ack-recovered-receipt": {"upid": upid}, "lost-ack-before-cleanup": state}
        for name, value in values.items(): (report / (name + ".json")).write_text(json.dumps(value))

    def test_exact_terminal_first_ci_readiness_timeout_is_identified(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            self.write_timeout(report)
            self.assertEqual(preparation.first_ci_import_timeout(report), "UPID:pve-mig-a:fixture")

    def test_root_volume_failure_is_not_mislabelled_first_ci_startup(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            self.write_timeout(report, volume="vm-9002-disk-0")
            with self.assertRaisesRegex(RuntimeError, "first cloud-init"):
                preparation.first_ci_import_timeout(report)

    def test_other_import_failures_cannot_qualify_as_readiness_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            self.write_timeout(report, reason="permission denied")
            with self.assertRaisesRegex(RuntimeError, "child readiness timeout"):
                preparation.first_ci_import_timeout(report)

    def test_degraded_source_is_not_healthy_preparation(self):
        summary = {"complete": False, "mode": "guest", "failure": "fixture failure",
                   "outer_tcg_threading": {"a": "multi", "b": "multi"}}
        identity = {"cloud_exit": 2, "cloud": {"status": "done", "extended_status": "degraded done"}}
        with mock.patch.object(preparation, "read", side_effect=[summary, identity]):
            with self.assertRaisesRegex(RuntimeError, "strict healthy source"):
                preparation.source_boot_gate(Path("unused"))

    def test_preparation_never_claims_complete_bootable_transfer(self):
        base = {"complete": False, "production_contacted": False, "iroh_used": False,
                "guest_image_sha256": "a" * 64}
        paths = [preparation.ROOT / "evidence" / ("remote-migrate-qualification-unit-" + x) for x in ("interruption", "fence", "source")]
        with mock.patch.object(preparation.matrix, "interruption_gate", return_value=(base, {})), \
             mock.patch.object(preparation.matrix, "fence_gate", return_value=({**base, "complete": True}, {})), \
             mock.patch.object(preparation, "source_boot_gate", return_value=(base, {})), \
             mock.patch.object(preparation, "private_api_gate"), \
             mock.patch.object(preparation.matrix, "descriptor", return_value={"checked": True}):
            result = preparation.compose(*paths)
        self.assertEqual(result["schema"], "pve-native-physical-preparation/1")
        self.assertFalse(result["complete"])
        self.assertFalse(result["bootable_transfer_qualified"])
        self.assertTrue(result["preparation_complete"])
        self.assertFalse(result["all_gates_same_vm_and_run"])
        self.assertEqual(set(result["reports"]), {"interruption", "fence", "source_boot"})


if __name__ == "__main__": unittest.main()
