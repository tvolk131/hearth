"""Offline matrix-lock contract; never starts an emulator."""
import fcntl
import contextlib
import io
import os
import pathlib
import signal
import tempfile
import time
import unittest
from unittest import mock
import subprocess
import sys

import qualification_matrix


class InheritedLock(unittest.TestCase):
    def test_same_description_shares_exclusion_without_releasing_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "lab.lock"
            with path.open("a") as parent, path.open("a") as contender:
                fcntl.flock(parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
                inherited = os.dup(parent.fileno())
                try:
                    qualification_matrix.validate_inherited_lock(inherited, path)
                finally:
                    os.close(inherited)
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(parent, fcntl.LOCK_UN)
                fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_independently_opened_descriptor_cannot_bypass_existing_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "lab.lock"
            with path.open("a") as owner, path.open("a") as other:
                fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with self.assertRaises(BlockingIOError):
                    qualification_matrix.validate_inherited_lock(other.fileno(), path)

    def test_wrong_file_inode_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "lab.lock"
            path.touch()
            with (path.parent / "other").open("a") as other:
                with self.assertRaisesRegex(RuntimeError, "inode"):
                    qualification_matrix.validate_inherited_lock(other.fileno(), path)

    def test_exec_child_validates_inherited_lock_and_cannot_release_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "lab.lock"
            with path.open("a") as parent, path.open("a") as contender:
                fcntl.flock(parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
                script = "import pathlib,sys; sys.path.insert(0,sys.argv[1]); import qualification_matrix as q; q.validate_inherited_lock(int(sys.argv[2]),pathlib.Path(sys.argv[3]))"
                result = subprocess.run([sys.executable, "-c", script, str(pathlib.Path(qualification_matrix.__file__).parent),
                                         str(parent.fileno()), str(path)], pass_fds=(parent.fileno(),), capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)


class AnchoredSessions(unittest.TestCase):
    def setUp(self):
        try:
            qualification_matrix.process_table()
        except PermissionError as error:
            self.skipTest("owned-process inspection is blocked by the sandbox: " + str(error))
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = pathlib.Path(self.temporary.name)
        self.lock_path = self.directory / "lab.lock"
        self.owner = self.lock_path.open("a")
        fcntl.flock(self.owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.sessions, self.logs = [], []

    def tearDown(self):
        qualification_matrix.retire_sessions(self.sessions, grace_seconds=0, retry_seconds=.01)
        for logfile in self.logs:
            logfile.close()
        self.owner.close()
        self.temporary.cleanup()

    def spawn(self, worker):
        name = "test-" + str(len(self.sessions))
        status = self.directory / (name + ".json")
        logfile = (self.directory / (name + ".log")).open("wb")
        self.logs.append(logfile)
        script = ("import pathlib,sys; sys.path.insert(0,sys.argv[1]); import qualification_matrix as q; "
                  "q.session_anchor(int(sys.argv[2]),pathlib.Path(sys.argv[3]),[sys.executable,'-c',sys.argv[4]])")
        argv = [sys.executable, "-c", script, str(pathlib.Path(qualification_matrix.__file__).parent),
                str(self.owner.fileno()), str(status), worker]
        return qualification_matrix.spawn_registered(name, argv, status, self.owner.fileno(), logfile, self.sessions)

    def wait_for(self, predicate):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(.02)
        self.fail("offline worker did not reach the expected state")

    def assert_stopped(self, pid):
        state = qualification_matrix.process_table().get(pid)
        self.assertTrue(state is None or not qualification_matrix.live(state[1]), state)

    def orphan_worker(self):
        receipt = self.directory / "descendant.pid"
        worker = ("import pathlib,subprocess,sys; "
                  "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)']); "
                  f"pathlib.Path({str(receipt)!r}).write_text(str(p.pid))")
        session = self.spawn(worker)
        self.wait_for(lambda: session.status().get("phase") == "complete")
        return session, int(receipt.read_text())

    def test_exited_worker_descendant_is_retired_before_anchor_reap(self):
        session, descendant = self.orphan_worker()
        self.assertTrue(qualification_matrix.live(qualification_matrix.process_table()[descendant][1]))
        real_wait = session.process.wait
        live_members_at_reap = []

        def checked_wait(*args, **kwargs):
            live_members_at_reap.append(any(pgid == session.process.pid and qualification_matrix.live(state)
                                           for pgid, state in qualification_matrix.process_table().values()))
            return real_wait(*args, **kwargs)

        with mock.patch.object(session.process, "poll", side_effect=AssertionError("anchor must remain unreaped")), \
                mock.patch.object(session.process, "wait", side_effect=checked_wait) as waited:
            qualification_matrix.retire_sessions(self.sessions, grace_seconds=0, retry_seconds=.01)
            waited.assert_called_once()
        self.assertTrue(session.retired)
        self.assertTrue(session.forced_cleanup)
        self.assertEqual(live_members_at_reap, [False])
        self.assert_stopped(descendant)

    def test_dead_anchor_does_not_hide_its_live_descendant(self):
        receipt = self.directory / "descendant.pid"
        worker = ("import pathlib,subprocess,sys,time; "
                  "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)']); "
                  f"pathlib.Path({str(receipt)!r}).write_text(str(p.pid)); time.sleep(120)")
        session = self.spawn(worker)
        self.wait_for(receipt.exists)
        self.wait_for(lambda: session.status().get("phase") == "running")
        descendant = int(receipt.read_text())
        os.kill(session.process.pid, signal.SIGKILL)
        self.wait_for(lambda: not qualification_matrix.live(qualification_matrix.process_table()[session.process.pid][1]))
        self.assertTrue(qualification_matrix.live(qualification_matrix.process_table()[descendant][1]))
        self.assertIsNone(session.process.returncode)  # No poll/wait has reaped it.
        qualification_matrix.retire_sessions(self.sessions, grace_seconds=0, retry_seconds=.01)
        self.assertTrue(session.retired)
        self.assert_stopped(descendant)

    def test_anchor_keeps_the_lease_after_worker_exit_and_parent_close(self):
        session = self.spawn("pass")
        self.wait_for(lambda: session.status().get("phase") == "complete")
        self.owner.close()
        with self.lock_path.open("a") as contender:
            with self.assertRaises(BlockingIOError):
                fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            qualification_matrix.retire_sessions(self.sessions, grace_seconds=0, retry_seconds=.01)
            fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.assertFalse(session.forced_cleanup)

    def test_cancellation_runs_worker_finally_before_group_retirement(self):
        ready, cleaned = self.directory / "ready", self.directory / "cleaned"
        worker = f'''import pathlib,signal,time
def stop(signum,frame): raise KeyboardInterrupt
signal.signal(signal.SIGTERM,stop)
pathlib.Path({str(ready)!r}).touch()
try: time.sleep(120)
finally: pathlib.Path({str(cleaned)!r}).touch()
'''
        session = self.spawn(worker)
        self.wait_for(ready.exists)
        qualification_matrix.retire_sessions(self.sessions, grace_seconds=2, retry_seconds=.01)
        self.assertTrue(cleaned.exists())
        self.assertTrue(session.retired)
        self.assertFalse(session.forced_cleanup)

    def test_cancellation_is_deferred_until_spawn_is_registered(self):
        real_popen = subprocess.Popen
        previous = signal.getsignal(signal.SIGTERM)

        def interrupted(signum, frame):
            raise KeyboardInterrupt

        def spawn_then_cancel(*args, **kwargs):
            process = real_popen(*args, **kwargs)
            os.kill(os.getpid(), signal.SIGTERM)
            self.assertEqual(len(self.sessions), 0)
            return process

        signal.signal(signal.SIGTERM, interrupted)
        try:
            with mock.patch.object(qualification_matrix.subprocess, "Popen", side_effect=spawn_then_cancel):
                with self.assertRaises(KeyboardInterrupt):
                    self.spawn("import time; time.sleep(120)")
            self.assertEqual(len(self.sessions), 1)
        finally:
            signal.signal(signal.SIGTERM, previous)
        qualification_matrix.retire_sessions(self.sessions, grace_seconds=0, retry_seconds=.01)
        self.assertTrue(self.sessions[0].retired)

    def test_failed_cleanup_proof_retains_lock_and_unreaped_anchor_until_retry(self):
        session = self.spawn("pass")
        self.wait_for(lambda: session.status().get("phase") == "complete")
        real_table = qualification_matrix.process_table
        calls = 0
        observations = {}

        class RetryLimitExceeded(BaseException):
            pass

        def fail_once_after_kill():
            nonlocal calls
            calls += 1
            if calls > 20:
                raise RetryLimitExceeded("cleanup retry did not recover from the injected failure")
            if calls == 2:
                observations["unreaped"] = session.process.returncode is None
                observations["mask"] = signal.pthread_sigmask(signal.SIG_BLOCK, set())
                with self.lock_path.open("a") as contender:
                    try:
                        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        observations["locked"] = True
                    else:
                        observations["locked"] = False
                raise OSError("injected process snapshot failure")
            return real_table()

        output = io.StringIO()
        with mock.patch.object(qualification_matrix, "process_table", side_effect=fail_once_after_kill), \
                contextlib.redirect_stdout(output):
            qualification_matrix.retire_sessions(self.sessions, grace_seconds=0, retry_seconds=.01)
        self.assertIn("retaining the lab lock", output.getvalue())
        self.assertGreaterEqual(calls, 4)
        self.assertTrue(observations["unreaped"])
        self.assertTrue(observations["locked"])
        self.assertTrue({signal.SIGINT, signal.SIGTERM} <= observations["mask"])
        self.assertTrue(session.retired)

    def test_worker_ignoring_cancellation_is_forcibly_retired(self):
        ready = self.directory / "ready"
        worker = ("import pathlib,signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); "
                  f"pathlib.Path({str(ready)!r}).touch(); time.sleep(120)")
        session = self.spawn(worker)
        self.wait_for(ready.exists)
        qualification_matrix.retire_sessions(self.sessions, grace_seconds=.2, retry_seconds=.01)
        self.assertTrue(session.forced_cleanup)
        self.assertTrue(session.retired)
        self.assert_stopped(session.status()["worker_pid"])


if __name__ == "__main__":
    unittest.main()
