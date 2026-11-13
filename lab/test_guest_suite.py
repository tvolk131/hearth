import pathlib
import shlex
import tempfile
import unittest
from unittest import mock

import guest_suite


class GuestSshTests(unittest.TestCase):
    def setUp(self):
        self.lab = mock.Mock()
        self.lab.ssh_arguments.return_value = ["ssh", "-F", "/dev/null", "-o", "StrictHostKeyChecking=yes",
                                                "-o", "ForwardAgent=no", "root@127.0.0.1"]
        self.directory = pathlib.Path("/tmp/disposable lab")

    def test_inner_ssh_keeps_outer_host_pinning_and_uses_only_lab_subnet(self):
        arguments = guest_suite.guest_ssh(self.lab, self.directory, self.directory / "key", "10.77.0.11", "id")
        proxy = next(value for value in arguments if value.startswith("ProxyCommand="))
        self.assertEqual(shlex.split(proxy.removeprefix("ProxyCommand=")),
                         self.lab.ssh_arguments.return_value[:-1] + ["-W", "10.77.0.11:22", "root@127.0.0.1"])
        for required in ("StrictHostKeyChecking=yes", "ForwardAgent=no", "IdentityAgent=none", "BatchMode=yes", "IdentitiesOnly=yes"):
            self.assertIn(required, arguments)
        self.assertEqual(arguments[-2:], ["dev@10.77.0.11", "id"])
        self.assertIn("ConnectTimeout=30", arguments)
        self.assertEqual(guest_suite.GUEST_SSH_TIMEOUT, 180)

    def test_tofu_is_explicit_and_never_replaces_changed_keys(self):
        arguments = guest_suite.guest_ssh(self.lab, self.directory, self.directory / "key", "10.77.0.109", "id", first_contact=True)
        self.assertIn("StrictHostKeyChecking=accept-new", arguments)
        self.assertNotIn("StrictHostKeyChecking=no", arguments)

    def test_nonlab_addresses_are_refused_before_ssh(self):
        for address in ("203.0.113.7", "127.0.0.1", "10.77.0.1", "10.77.0.110", "10.77.0.11;evil"):
            with self.assertRaises(ValueError):
                guest_suite.guest_ssh(self.lab, self.directory, self.directory / "key", address, "id")
        self.lab.ssh_arguments.assert_not_called()

    def test_clean_cloud_init_completion_is_required(self):
        self.assertEqual(guest_suite.cloud_readiness(0, '{"status":"done"}'), {"status":"done"})
        self.assertIsNone(guest_suite.cloud_readiness(0, '{"status":"running"}'))
        self.assertIsNone(guest_suite.cloud_readiness(255, ""))
        for code, output in [(1, '{"status":"error"}'), (2, '{"status":"done","extended_status":"degraded done"}'),
                             (0, '{"status":"done","errors":["bad"]}'), (0, 'not JSON'), (0, '{"status":"disabled"}')]:
            with self.assertRaises(RuntimeError):
                guest_suite.cloud_readiness(code, output)

    def test_serial_capture_is_bounded_read_only_and_lab_only(self):
        script = guest_suite.serial_capture_script(9001, 600)
        compile(script, "serial-collector", "exec")
        self.assertIn("/var/run/qemu-server/9001.serial0", script)
        self.assertIn("connection.recv", script)
        self.assertIn("2097152", script)
        self.assertNotIn("send", script)
        for vm_id in (101, 9000, 9100, True, "9001;evil"):
            with self.assertRaises(ValueError):
                guest_suite.serial_capture_script(vm_id, 600)
        for timeout in (0, 7201, True, "600"):
            with self.assertRaises(ValueError):
                guest_suite.serial_capture_script(9001, timeout)

    def test_console_boot_errors_and_collector_failure_fail_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            evidence = pathlib.Path(temporary)
            process = mock.Mock()
            process.poll.return_value = None
            for text in ("Kernel panic - not syncing: bad", "(initramfs)", "Gave up waiting for root file system",
                         "You are in emergency mode."):
                (evidence / "guest-serial.log").write_text(text)
                with self.assertRaisesRegex(RuntimeError, "no automatic reboot or retry"):
                    guest_suite.check_console(evidence, process)
            (evidence / "guest-serial.log").write_text("normal boot progress")
            guest_suite.check_console(evidence, process)
            process.poll.return_value = 1
            with self.assertRaisesRegex(RuntimeError, "collector exited"):
                guest_suite.check_console(evidence, process)

    def test_serial_capture_cleans_up_its_own_child_after_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            evidence = pathlib.Path(temporary)
            with mock.patch.object(guest_suite.subprocess, "Popen") as popen:
                process = popen.return_value
                process.poll.return_value = None
                with self.assertRaisesRegex(RuntimeError, "deliberate failure"):
                    with guest_suite.serial_capture(self.lab, self.directory, evidence, 9001, 600):
                        raise RuntimeError("deliberate failure")
                self.assertEqual(popen.call_args.args[0], self.lab.ssh_arguments.return_value + ["python3 -I -"])
                process.stdin.close.assert_called_once()
                process.terminate.assert_called_once()
                process.wait.assert_called_once_with(timeout=10)
                self.assertTrue(popen.call_args.kwargs["stdout"].closed)
                self.assertTrue(popen.call_args.kwargs["stderr"].closed)


if __name__ == "__main__":
    unittest.main()
