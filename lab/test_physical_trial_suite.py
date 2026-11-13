import hashlib
import json
import pathlib
import tempfile
import unittest
from unittest import mock

import physical_trial_suite as suite
from physical_trial_suite import CAPACITY_MIB, PORTABLE_BINARIES, cached_base, freeze_binaries, growth_gate, trial_config


class PhysicalModeFixtureTests(unittest.TestCase):
    def test_cli_success_requires_explicit_complete_result(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for complete in (True, False, "true", 1):
                with self.subTest(complete=complete), mock.patch.object(suite.lab, "HOME", root), mock.patch.object(suite, "run", return_value={"complete": complete}), mock.patch.object(suite.signal, "signal"), mock.patch("sys.argv", ["physical_trial_suite.py", "--linux-bin-dir", str(root)]):
                    if complete is True:
                        suite.main()
                    else:
                        with self.assertRaises(SystemExit) as result:
                            suite.main()
                        self.assertEqual(result.exception.code, 1)

    def test_only_matching_sealed_base_is_allowed_and_nothing_is_rebuilt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            with mock.patch.object(suite.lab, "HOME", root), mock.patch.object(suite.lab, "build") as build, mock.patch.object(suite.lab, "verify_image") as verify:
                with self.assertRaisesRegex(RuntimeError, "sealed"):
                    cached_base()
                base = root / "base"
                base.mkdir()
                for name in ("root.qcow2", "seed.iso"):
                    (base / name).write_bytes(b"fixture")
                digest = hashlib.sha256((suite.lab.ROOT / "lab/inputs.json").read_bytes() + (suite.lab.ROOT / "lab/bootstrap.sh").read_bytes()).hexdigest()
                info = {"schema": 1, "inputs_digest": digest, "debian": suite.lab.INPUTS, "nonce": "a" * 32}
                (base / "build.json").write_text(json.dumps(info))
                self.assertEqual(cached_base(), (base, info))
                verify.assert_called_once_with(root / "debian.qcow2")
                info["inputs_digest"] = "wrong"
                (base / "build.json").write_text(json.dumps(info))
                with self.assertRaisesRegex(RuntimeError, "will not rebuild"):
                    cached_base()
                build.assert_not_called()

    def config(self):
        return trial_config("a" * 32, "111", "222", "b" * 64)

    def test_fixture_uses_exact_bounded_scope_and_requested_parent_cap(self):
        config = self.config()
        self.assertEqual(config["scope"]["capacity_mib"], 12288)
        self.assertEqual(CAPACITY_MIB, 12288)
        self.assertEqual(config["scope"]["owners"], ["trial-admin", "trial-user"])
        self.assertEqual(config["scope"]["vm_ids"], [9001, 9002, 9003, 9004])
        self.assertEqual(config["scope"]["root_dataset"], "labpool/pve-dev-trial-" + "a" * 32)
        for nonce, guid, digest in [("production", "111", "b" * 64), ("a" * 32, "wrong", "b" * 64), ("a" * 32, "111", "short")]:
            with self.assertRaises(ValueError):
                trial_config(nonce, guid, "222", digest)

    def test_effect_gate_is_exact_scoped_growth_and_does_not_spoof_lab_identity(self):
        config = self.config()
        script = growth_gate(config)
        compile(script, "growth-gate-install", "exec")
        self.assertIn("quota=7516192768", script)
        self.assertIn(config["scope"]["root_dataset"] + "/control/trial-user", script)
        self.assertIn('kill -STOP', script)
        self.assertNotIn("/etc/pve-dev-lab-id", script)
        self.assertIn("/etc/machine-id", script)
        config["scope"]["root_guid"] = "foreign"
        with self.assertRaises(ValueError):
            growth_gate(config)

    def fixture_binaries(self, source):
        header = bytearray(32)
        header[:6] = b"\x7fELF\x02\x01"
        header[18:20] = (62).to_bytes(2, "little")
        hashes = {}
        for name in PORTABLE_BINARIES:
            (source / name).write_bytes(header + name.encode())
            hashes[name] = hashlib.sha256((source / name).read_bytes()).hexdigest()
        (source / "build-manifest.json").write_text(json.dumps({"binaries": hashes, "source_manifest_sha256": "f" * 64}))
        return hashes

    def test_all_three_portable_artifacts_and_source_are_pinned(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "source"
            source.mkdir()
            expected = self.fixture_binaries(source)
            hashes, source_hash = freeze_binaries(source, root / "frozen")
            self.assertEqual(hashes, expected)
            self.assertEqual(set(hashes), set(PORTABLE_BINARIES))
            self.assertEqual(source_hash, "f" * 64)
            (source / "pve-devctl").write_bytes(b"native Mac binary")
            with self.assertRaisesRegex(RuntimeError, "ELF"):
                freeze_binaries(source, root / "wrong-architecture")

    def test_manifest_mismatch_or_missing_cli_cannot_start_qualification(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "source"
            source.mkdir()
            self.fixture_binaries(source)
            with (source / "pve-devd").open("ab") as stream:
                stream.write(b"tamper")
            with self.assertRaisesRegex(RuntimeError, "manifest"):
                freeze_binaries(source, root / "tampered")
            (source / "pve-devctl").unlink()
            with self.assertRaises(FileNotFoundError):
                freeze_binaries(source, root / "missing-client")
