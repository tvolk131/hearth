import hashlib
import contextlib
import io
import json
import pathlib
import tempfile
import unittest
from unittest import mock

import lab


class LabSafetyTests(unittest.TestCase):
    def test_checksum_failure_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            image = pathlib.Path(temporary) / "image.qcow2"
            image.write_bytes(b"not the pinned image")
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                lab.verify_image(image)

    def test_atomic_state_is_replaced_without_a_partial_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "state.json"
            lab.atomic_json(path, {"state": "intent"})
            lab.atomic_json(path, {"state": "verified"})
            self.assertEqual(path.read_text(), '{\n  "state": "verified"\n}\n')
            self.assertFalse(path.with_suffix(".tmp").exists())

    def test_space_floor_refuses_to_start(self):
        with mock.patch.object(lab.shutil, "disk_usage") as usage:
            usage.return_value.free = lab.MIN_FREE - 1
            with self.assertRaisesRegex(RuntimeError, "safety floor"):
                lab.space()

    def test_invalid_port_never_reaches_ssh(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            lab.atomic_json(directory / "endpoint.json", {"port": 22})
            with self.assertRaisesRegex(RuntimeError, "loopback port"):
                lab.ssh_arguments(directory)

    def test_host_cloud_init_is_disabled_before_guest_seed_volumes_exist(self):
        directory = pathlib.Path("disposable-run")
        with mock.patch.object(lab, "ssh") as execute:
            lab.disable_host_cloud_init(directory)
            execute.assert_called_once_with(directory, "install -m 0644 /dev/null /etc/cloud/cloud-init.disabled",
                                            capture_output=True, text=True, timeout=45)
            execute.return_value.check_returncode.assert_called_once_with()

    def test_failure_to_disable_host_cloud_init_aborts_setup(self):
        with mock.patch.object(lab, "ssh") as execute:
            execute.return_value.check_returncode.side_effect = RuntimeError("cannot disable cloud-init")
            with self.assertRaisesRegex(RuntimeError, "cannot disable cloud-init"):
                lab.disable_host_cloud_init(pathlib.Path("disposable-run"))

    def test_pinned_input_is_a_versioned_https_image(self):
        self.assertTrue(lab.INPUTS["debian_url"].startswith("https://cloud.debian.org/"))
        self.assertNotIn("/latest/", lab.INPUTS["debian_url"])
        self.assertEqual(len(bytes.fromhex(lab.INPUTS["debian_sha512"])), hashlib.sha512().digest_size)

    def test_kernel_panic_fails_without_waiting_for_ssh(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            (directory / "serial.log").write_text("booting\nKernel panic - not syncing: Fatal exception\n")
            process = mock.Mock()
            process.poll.return_value = None
            with mock.patch.object(lab, "space"), mock.patch.object(lab, "ssh") as execute:
                with self.assertRaisesRegex(RuntimeError, "no automatic reset or retry"):
                    lab.ready(directory, process, 60)
                execute.assert_not_called()

    def test_exited_qemu_fails_with_its_diagnostic_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            (directory / "qemu.log").write_text("emulator exited")
            process = mock.Mock()
            process.poll.return_value = 1
            with self.assertRaisesRegex(RuntimeError, "QEMU exited: emulator exited"):
                lab.check_boot_health(directory, process)

    def test_readonly_boot_query_timeout_is_retried_without_reboot_or_mutation(self):
        response = mock.Mock(returncode=0, stdout='{"version":"test"}')
        expired = lab.subprocess.TimeoutExpired("read-only readiness", 45)
        with mock.patch.object(lab, "space"), mock.patch.object(lab, "check_boot_health"), \
                mock.patch.object(lab.time, "sleep"), mock.patch.object(lab, "qmp") as monitor, \
                mock.patch.object(lab, "ssh", side_effect=[expired, response]) as execute:
            self.assertEqual(lab.ready(pathlib.Path("disposable-run"), mock.Mock(), 60), {"version": "test"})
            self.assertEqual(execute.call_count, 2)
            self.assertEqual(execute.call_args_list[0], execute.call_args_list[1])
            self.assertIn("pvesh get /version", execute.call_args.args[1])
            monitor.assert_not_called()

    def test_boot_uses_device_serials_and_cleans_up_after_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            (directory / "host.pub").write_text("ssh-ed25519 disposable-test-key")
            with mock.patch.object(lab, "space"), mock.patch.object(lab, "identity", return_value=directory), \
                    mock.patch.object(lab.socket, "socket") as sockets, \
                    mock.patch.object(lab.subprocess, "Popen") as popen, \
                    mock.patch.object(lab, "qmp") as monitor:
                sockets.return_value.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 23456)
                popen.return_value.poll.return_value = None
                with self.assertRaisesRegex(RuntimeError, "test interruption"):
                    with lab.boot(directory, "a" * 32, data_disks=True):
                        raise RuntimeError("test interruption")
                command = popen.call_args.args[0]
                self.assertIn("virtio-blk-pci,drive=data-a,serial=lab-data-a", command)
                self.assertIn("virtio-blk-pci,drive=rootdisk,bootindex=1", command)
                self.assertIn("tcg,thread=single", command)
                self.assertTrue(any("hostfwd=tcp:127.0.0.1:23456-:22" in argument for argument in command))
                self.assertFalse(any("format=qcow2,if=virtio,serial=" in argument for argument in command))
                monitor.assert_called_once_with(directory, "system_powerdown")
                popen.return_value.wait.assert_called_once_with(timeout=90)

    def test_controller_forwarding_is_explicit_and_loopback_udp_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            (directory / "host.pub").write_text("ssh-ed25519 disposable-test-key")
            with mock.patch.object(lab, "space"), mock.patch.object(lab, "identity", return_value=directory), \
                    mock.patch.object(lab.socket, "socket") as sockets, \
                    mock.patch.object(lab.subprocess, "Popen") as popen, \
                    mock.patch.object(lab, "qmp"):
                sockets.return_value.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 23456)
                popen.return_value.poll.return_value = None
                with lab.boot(directory, "a" * 32, data_disks=True, control_port=True):
                    command = popen.call_args.args[0]
                    network = command[command.index("-netdev") + 1]
                    self.assertIn("hostfwd=udp:127.0.0.1:23456-:7777", network)
                    self.assertEqual(json.loads((directory / "endpoint.json").read_text())["control_udp_port"], 23456)
                    self.assertNotIn("hostfwd=udp:0.0.0.0", network)
                    self.assertIn("tcg,thread=multi", command)
                    self.assertFalse(any("/dev/disk" in argument for argument in command))

if __name__ == "__main__":
    unittest.main()
