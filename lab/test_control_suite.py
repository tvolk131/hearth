import subprocess
import unittest
import copy

from control_suite import controller_capacity_arguments, policy_request, quota_fault_install, verify_quota_refusal, verify_quota_transition
from guest_suite import GUEST_CORES


class ControllerCapacityTests(unittest.TestCase):
    def test_all_owner_budgets_fit_the_selected_suite_capacity(self):
        for bootable in (False, True):
            arguments = controller_capacity_arguments(bootable)
            limits = dict(zip(arguments[::2], map(int, arguments[1::2])))
            disk_required = 3 * 128 + (5120 if bootable else 0)
            memory_required = 3 * 256 + (512 if bootable else 0)
            self.assertGreaterEqual(limits["--disk-capacity-mib"], disk_required)
            self.assertGreaterEqual(limits["--min-memory-capacity-mib"], memory_required)
            self.assertLessEqual(limits["--min-memory-capacity-mib"], 2048)
            self.assertLessEqual(limits["--disk-capacity-mib"], 16384)

    def test_nested_guest_uses_the_four_available_virtual_cpus(self):
        self.assertEqual(GUEST_CORES, 4)


class QuotaEvidenceTests(unittest.TestCase):
    def refusal(self, message="cannot create volume: out of space", returncode=1):
        return subprocess.CompletedProcess(["zfs"], returncode, stdout="", stderr=message)

    def test_generic_zfs_out_of_space_with_quota_and_pool_evidence_is_valid(self):
        verify_quota_refusal(self.refusal(), 128 * 1024**2, 1024**3, [])

    def test_unrelated_errors_and_success_are_not_quota_proof(self):
        for result in (self.refusal("permission denied"), self.refusal(returncode=0)):
            with self.assertRaisesRegex(RuntimeError, "capacity refusal"):
                verify_quota_refusal(result, 128 * 1024**2, 1024**3, [])

    def test_global_pool_exhaustion_or_wrong_quota_is_not_accepted(self):
        for quota, available in ((0, 1024**3), (128 * 1024**2, 16 * 1024**2)):
            with self.assertRaisesRegex(RuntimeError, "owner limit"):
                verify_quota_refusal(self.refusal(), quota, available, [])

    def test_unexpected_volume_is_not_accepted(self):
        with self.assertRaisesRegex(RuntimeError, "unexpected volume"):
            verify_quota_refusal(self.refusal(), 128 * 1024**2, 1024**3, ["labpool/control/frank/over-budget"])


class QuotaWorkflowEvidenceTests(unittest.TestCase):
    def snapshot(self):
        dataset = "labpool/control/frank"
        return {"dataset": dataset, "zfs": {
            dataset: {"guid": "100", "createtxg": "5", "org.pve-dev:owner": "lab:frank",
                      "quota": str(128 * 1024**2), "reservation": str(128 * 1024**2), "used": "50000000"},
            dataset + "/vm-9001-disk-0": {"guid": "101", "createtxg": "6", "org.pve-dev:owner": "lab:frank:9001",
                                         "volsize": "33554432", "refreservation": "33554432", "used": "33554432"}},
                "configs": {"9001": {"memory": 128, "cores": 1, "scsi0": "ctl-frank:vm-9001-disk-0"}},
                "storage": {"pool": dataset, "sparse": 0}}

    def test_request_preserves_full_policy_revision_and_source(self):
        user = {"user_id": "frank", "budget_mib": 128, "min_memory_budget_mib": 256,
                "max_memory_budget_mib": 1024, "max_cores": 8, "max_vms": 4, "policy_revision": 7}
        request = policy_request(user, "change", budget_mib=256)
        self.assertEqual(request["expected_revision"], 7)
        self.assertEqual(request["policy"], {key: (256 if key == "budget_mib" else user[key])
                                             for key in ("budget_mib", "min_memory_budget_mib", "max_memory_budget_mib", "max_cores", "max_vms")})
        self.assertEqual(user["budget_mib"], 128)
        with self.assertRaisesRegex(ValueError, "unknown"):
            policy_request(user, "typo", budget=256)

    def test_valid_transition_requires_exact_properties_and_preserves_identity(self):
        before = self.snapshot()
        after = copy.deepcopy(before)
        for key in ("quota", "reservation"):
            after["zfs"][after["dataset"]][key] = str(96 * 1024**2)
        verify_quota_transition(before, after, 96)
        for key in ("quota", "reservation", "guid", "createtxg", "org.pve-dev:owner"):
            changed = copy.deepcopy(after)
            changed["zfs"][changed["dataset"]][key] = "123"
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                verify_quota_transition(before, changed, 96)

    def test_configuration_storage_inventory_and_volume_replacement_are_rejected(self):
        before = self.snapshot()
        for field in ("configs", "storage", "volume_guid", "inventory"):
            after = copy.deepcopy(before)
            if field == "configs":
                after["configs"]["9001"]["memory"] = 256
            elif field == "storage":
                after["storage"]["sparse"] = 1
            elif field == "volume_guid":
                after["zfs"][after["dataset"] + "/vm-9001-disk-0"]["guid"] = "replacement"
            else:
                after["zfs"][after["dataset"] + "/orphan"] = {}
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                verify_quota_transition(before, after, 128)

    def test_thin_backing_cannot_be_reported_as_preserved(self):
        before = self.snapshot()
        before["zfs"][before["dataset"] + "/vm-9001-disk-0"]["refreservation"] = "0"
        with self.assertRaisesRegex(RuntimeError, "thin"):
            verify_quota_transition(before, copy.deepcopy(before), 128)

    def test_fault_gate_is_bound_to_valid_disposable_nonce_and_exact_effect(self):
        with self.assertRaisesRegex(ValueError, "nonce"):
            quota_fault_install("production-host")
        script = quota_fault_install("a" * 32)
        self.assertIn('socket.gethostname() != "pve-lab"', script)
        self.assertIn("quota=134217728", script)
        self.assertIn("signal.SIGSTOP", script)
        compile(script, "quota-fault-install", "exec")
