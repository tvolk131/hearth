import copy
from dataclasses import replace
import random
import unittest

from migration_handoff_model import HandoffModel, Plan, Refused


class MigrationHandoffSpecificationTests(unittest.TestCase):
    def ready_to_commit(self):
        model = HandoffModel()
        for event in ("reserve", "copy", "ready", "deliver_ready", "fence_intent",
                      "install_fence", "stop", "record_stopped"):
            getattr(model, event)()
            model.check()
        return model

    def complete(self, model):
        for event in ("commit", "send_proof", "deliver_proof", "start", "record_running"):
            getattr(model, event)()
            model.check()

    def test_disconnected_gui_and_lost_handoff_leave_source_fenced(self):
        model = self.ready_to_commit()
        model.commit()
        model.send_proof()
        model.drop_messages()
        model.reboot_source()
        model.reboot_target()
        model.expire()
        self.assertFalse(model.effects["source_running"])
        self.assertFalse(model.effects["target_running"])
        with self.assertRaises(Refused):
            model.abort_source()
        with self.assertRaises(Refused):
            model.send_proof()
        model.renew()
        model.send_proof()
        model.deliver_proof()
        model.start()
        model.check()

    def test_every_happy_path_boundary_survives_crash_and_duplicate_messages(self):
        sequence = ("reserve", "copy", "ready", "deliver_ready", "fence_intent",
                    "install_fence", "stop", "record_stopped", "commit", "send_proof",
                    "deliver_proof", "start", "record_running")
        for boundary in range(len(sequence) + 1):
            model = HandoffModel()
            for event in sequence[:boundary]:
                getattr(model, event)()
            model.crash_source()
            model.crash_target()
            for event in sequence[boundary:]:
                getattr(model, event)()
                model.check()
            for _ in range(3):
                model.send_proof()
                model.deliver_proof()
                model.start()
                model.reboot_source()
                model.reboot_target()
                model.check()

    def test_expiry_never_reverses_accepted_ownership(self):
        model = self.ready_to_commit()
        model.commit()
        model.send_proof()
        model.deliver_proof()
        model.expire()
        model.start()
        model.reboot_source()
        model.reboot_target()
        with self.assertRaises(Refused):
            model.abort_source()
        with self.assertRaises(Refused):
            model.abort_target()
        model.check()

    def test_stale_copy_requires_final_stopped_data_to_be_transferred(self):
        model = HandoffModel()
        for event in ("reserve", "copy", "ready", "deliver_ready", "source_write",
                      "fence_intent", "install_fence", "stop", "record_stopped"):
            getattr(model, event)()
        with self.assertRaisesRegex(Refused, "final source data"):
            model.commit()
        for event in ("copy", "ready", "deliver_ready"):
            getattr(model, event)()
        self.complete(model)

    def test_changed_job_peer_or_resource_cannot_reuse_handoff(self):
        model = self.ready_to_commit()
        model.commit()
        model.send_proof()
        proof = copy.deepcopy(model.channel["proof"])
        for plan in (replace(Plan(), job="other-job"), replace(Plan(), target="other-peer"),
                     replace(Plan(), target_disk_guid="replacement-disk"),
                     replace(Plan(), vm_uuid="replacement-vm")):
            target = HandoffModel(plan)
            target.reserve()
            target.copy()
            target.ready()
            target.channel["proof"] = proof
            with self.assertRaises(Refused):
                target.deliver_proof()
            self.assertFalse(target.effects["target_running"])

    def test_ready_staging_never_autostarts_or_releases_its_claim(self):
        model = self.ready_to_commit()
        model.reboot_target()
        self.assertFalse(model.effects["target_running"])
        self.assertEqual(model.target["claim"], model.plan.disk_charge)
        with self.assertRaises(Refused):
            model.start()
        model.abort_target()
        self.assertTrue(model.effects["target_exists"])
        self.assertEqual(model.target["claim"], model.plan.disk_charge)
        with self.assertRaisesRegex(Refused, "absence"):
            model.release_target()
        model.crash_target()
        model.remove_target()
        model.crash_target()
        self.assertEqual(model.target["claim"], model.plan.disk_charge)
        model.release_target()
        self.assertFalse(model.effects["target_exists"])
        self.assertEqual(model.target["claim"], 0)
        with self.assertRaises(Refused):
            model.reserve()

    def test_abort_before_commit_cannot_be_followed_by_a_late_commit(self):
        model = self.ready_to_commit()
        model.abort_source()
        with self.assertRaises(Refused):
            model.commit()
        model.reboot_target()
        model.check()
        self.assertTrue(model.effects["source_running"])
        self.assertFalse(model.effects["target_running"])

    def test_bounded_reordering_crashes_expiry_and_retries_preserve_single_owner(self):
        # An executable specification, not exhaustive verification of the real
        # controller. Seed independent fixtures at varied progress boundaries so
        # an early abort cannot make the whole campaign an absorbing-state test.
        prefix = ("reserve", "copy", "ready", "deliver_ready", "fence_intent",
                  "install_fence", "stop", "record_stopped", "commit",
                  "send_proof", "deliver_proof", "start", "record_running")
        for seed in range(32):
            randomizer = random.Random(seed)
            for case in range(8):
                model = HandoffModel(Plan(job=f"fixture-{seed}-{case}"))
                for event in prefix[:randomizer.randrange(len(prefix) + 1)]:
                    getattr(model, event)()
                for _ in range(64):
                    event = randomizer.choice(model.ACTIONS)
                    before = model.state_key()
                    try:
                        getattr(model, event)()
                    except Refused:
                        self.assertEqual(model.state_key(), before, event)
                    model.check()


if __name__ == "__main__":
    unittest.main()
