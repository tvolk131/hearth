import copy
import pathlib
import tempfile
import unittest
from unittest import mock

import prepare_guest_image as prepare


class PreparedGuestImageTests(unittest.TestCase):
    def test_recipe_extends_timeouts_without_skipping_health(self):
        self.assertEqual(prepare.TIMEOUT_CONFIG,
                         b"[Manager]\nDefaultTimeoutStartSec=600s\nDefaultDeviceTimeoutSec=600s\n")
        self.assertNotIn(b"nofail", prepare.TIMEOUT_CONFIG)
        self.assertNotIn(b"disable", prepare.TIMEOUT_CONFIG)
        self.assertTrue(prepare.DROP_IN.startswith("/etc/systemd/system.conf.d/"))

    def test_default_user_uses_supported_cloud_init_mapping_without_suppressing_errors(self):
        self.assertEqual(prepare.CLOUD_INIT_CONFIG,
                         b"system_info:\n  default_user:\n    name: dev\n")
        self.assertEqual(prepare.CLOUD_INIT_DROP_IN,
                         "/etc/cloud/cloud.cfg.d/90-pve-dev-lab.cfg")
        self.assertEqual(prepare.RECIPE, "pve-dev-lab-boot-policy/2")

    def test_non_lab_identity_and_changed_pool_are_refused_before_effects(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "lab-id"
            path.write_text("a" * 32)
            with mock.patch.object(prepare, "LAB_ID", path), \
                    mock.patch.object(prepare.socket, "gethostname", return_value="pve1"), \
                    mock.patch.object(prepare, "run") as execute:
                with self.assertRaisesRegex(ValueError, "disposable lab identity"):
                    prepare.binding("a" * 32, "42")
                execute.assert_not_called()
            with mock.patch.object(prepare, "LAB_ID", path), \
                    mock.patch.object(prepare.socket, "gethostname", return_value="pve-lab"), \
                    mock.patch.object(prepare, "run", return_value="99\n") as execute:
                with self.assertRaisesRegex(ValueError, "pool identity"):
                    prepare.binding("a" * 32, "42")
                execute.assert_called_once_with("zpool", "get", "-Hp", "-o", "value", "guid", "labpool")

    def test_bad_bindings_never_execute_commands(self):
        with mock.patch.object(prepare, "run") as execute:
            for nonce, guid in (("invalid", "42"), ("a" * 32, "0"),
                                ("a" * 32, str(2**64)), ("a" * 32, "42;evil")):
                with self.assertRaises(ValueError):
                    prepare.binding(nonce, guid)
            execute.assert_not_called()

    def test_parent_must_be_self_contained_and_exact_size(self):
        good = {"format": "qcow2", "virtual-size": 3 * 1024**3,
                "format-specific": {"data": {"corrupt": False}}}
        prepare.image_info(good)
        for key, value in (("format", "raw"), ("virtual-size", 8 * 1024**3),
                           ("backing-filename", "/etc/shadow"), ("encrypted", True)):
            bad = copy.deepcopy(good)
            bad[key] = value
            with self.assertRaises(ValueError):
                prepare.image_info(bad)
        for key, value in (("encrypt", {}), ("corrupt", True),
                           ("data-file", "/dev/sda"), ("data-file-raw", True)):
            bad = copy.deepcopy(good)
            bad["format-specific"]["data"][key] = value
            with self.assertRaises(ValueError):
                prepare.image_info(bad)
