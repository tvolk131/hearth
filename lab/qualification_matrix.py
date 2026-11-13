"""Run independent guest and scope fixtures under one inherited lab lock.

The default lab commands remain exclusive. This supervisor runs either or both
of two fixed fixtures, allowing at most three outer virtual machines after
explicit capacity checks. It never
contacts physical hosts, builds a missing base, or installs software globally.
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import pathlib
import platform
import re
import shutil
import signal
import stat
import subprocess
import sys
import time

import lab


@contextlib.contextmanager
def defer_cancellation():
    """Do not lose ownership between fork/exec and registration, or in cleanup."""
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
    try:
        yield
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def process_table():
    output = subprocess.check_output(["ps", "-axo", "pid=,pgid=,stat="], text=True, timeout=10)
    table = {}
    for line in output.splitlines():
        fields = line.split()
        if len(fields) != 3 or not fields[0].isdigit() or not fields[1].isdigit():
            raise RuntimeError("could not establish the owned process groups")
        table[int(fields[0])] = (int(fields[1]), fields[2])
    if not table:
        raise RuntimeError("empty process ownership snapshot")
    return table


def live(state):
    # Zombies have relinquished their descriptors and memory and cannot fork.
    return not state.startswith(("Z", "X"))


class Session:
    def __init__(self, name, process, status_path):
        self.name, self.process, self.status_path = name, process, status_path
        self.stop_requested = self.retired = self.forced_cleanup = False

    def status(self):
        try:
            value = json.loads(self.status_path.read_text())
        except FileNotFoundError:
            return {}
        if value.get("phase") not in ("running", "complete") or not isinstance(value.get("worker_pid"), int):
            raise RuntimeError("invalid session worker status")
        if value["phase"] == "complete" and type(value.get("exit_code")) is not int:
            raise RuntimeError("invalid completed worker status")
        return value

    def finished(self, table):
        if self.status().get("phase") == "complete":
            return True
        anchor = table.get(self.process.pid)
        if anchor is None:
            raise RuntimeError("unreaped session anchor is missing from the process snapshot")
        return not live(anchor[1])

    def request_stop(self):
        # The running receipt is written only after the anchor installs its
        # handler. Signal the anchor, never a potentially reaped worker PID.
        if not self.stop_requested and self.status().get("phase") == "running":
            try:
                os.kill(self.process.pid, signal.SIGUSR1)
            except ProcessLookupError:
                pass
            self.stop_requested = True


def spawn_registered(name, argv, status_path, descriptor, logfile, sessions):
    with defer_cancellation():
        process = subprocess.Popen(argv, stdout=logfile, stderr=subprocess.STDOUT,
                                   pass_fds=(descriptor,), start_new_session=True)
        session = Session(name, process, status_path)
        sessions.append(session)
    return session


def session_anchor(descriptor, status_path, argv):
    """Keep the PGID and inherited lease pinned after the fixture worker exits.

    Only the parent reaps this anchor, after retiring the group. The fixed lab
    workers and their QEMUs inherit this group; none daemonize or call setsid.
    """
    stop_requested = False

    def request_stop(signum, frame):
        nonlocal stop_requested
        stop_requested = True

    signal.signal(signal.SIGUSR1, request_stop)
    signal.signal(signal.SIGINT, lambda signum, frame: None)
    signal.signal(signal.SIGTERM, lambda signum, frame: None)
    # The parent masks cancellation through Popen and registration. An exec
    # inherits that mask, so restore these signals after installing handlers.
    signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGINT, signal.SIGTERM})
    worker = subprocess.Popen(argv, pass_fds=(descriptor,))
    lab.atomic_json(status_path, {"phase": "running", "worker_pid": worker.pid})
    while worker.poll() is None:
        if stop_requested:
            worker.terminate()
            stop_requested = False
        time.sleep(.1)
    lab.atomic_json(status_path, {"phase": "complete", "worker_pid": worker.pid, "exit_code": worker.returncode})
    while True:
        signal.pause()


def retire_sessions(sessions, grace_seconds=60, retry_seconds=.2):
    """Hold the lease until every owned group is proved unable to run guests.

    Never poll/wait an anchor before this proof: even a dead, unreaped anchor
    reserves its PID, so its PGID cannot be reused while we signal the group.
    If inspection or retirement fails, stay alive holding the lease and retry.
    """
    with defer_cancellation():
        deadline = time.monotonic() + grace_seconds
        try:
            while time.monotonic() < deadline:
                table = process_table()
                if all(session.finished(table) for session in sessions):
                    break
                for session in sessions:
                    session.request_stop()
                time.sleep(retry_seconds)
        except Exception as error:
            print("Session grace period failed; retiring owned groups:", error, flush=True)
        next_report = 0
        while any(not session.retired for session in sessions):
            try:
                before = process_table()
                for session in sessions:
                    if session.retired:
                        continue
                    session.forced_cleanup |= any(pgid == session.process.pid and pid != session.process.pid and live(state)
                                                  for pid, (pgid, state) in before.items())
                    # On macOS killpg can return EPERM for a group containing
                    # only its unreaped zombie anchor. Such a group needs proof
                    # and reaping, rather than another signal.
                    if not any(pgid == session.process.pid and live(state) for pgid, state in before.values()):
                        continue
                    try:
                        os.killpg(session.process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                after = process_table()
                for session in sessions:
                    if session.retired:
                        continue
                    if any(pgid == session.process.pid and live(state) for pgid, state in after.values()):
                        continue
                    session.process.wait(timeout=2)
                    session.retired = True
                if all(session.retired for session in sessions):
                    return
            except Exception as error:
                if time.monotonic() >= next_report:
                    print("Cleanup not proved; retaining the lab lock and retrying:", error, flush=True)
                    next_report = time.monotonic() + 5
            time.sleep(retry_seconds)


def validate_inherited_lock(descriptor, path):
    if descriptor < 3:
        raise RuntimeError("an inherited nonstandard lock descriptor is required")
    actual, expected = os.fstat(descriptor), path.stat()
    if not stat.S_ISREG(actual.st_mode) or (actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino):
        raise RuntimeError("inherited descriptor is not this lab's lock inode")
    # The same inherited open file description already owns the parent's lock.
    # An unrelated independently opened descriptor cannot bypass another owner.
    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)


def capacity():
    if platform.system() == "Darwin":
        total = int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True, timeout=10))
        cpus = int(subprocess.check_output(["sysctl", "-n", "hw.logicalcpu"], text=True, timeout=10))
        pressure = subprocess.check_output(["memory_pressure", "-Q"], text=True, timeout=10)
        match = re.search(r"System-wide memory free percentage: (\d+)%", pressure)
        if match is None:
            raise RuntimeError("memory availability could not be established")
        available = total * int(match[1]) // 100
    elif platform.system() == "Linux":
        values = {line.split(":")[0]: int(line.split()[1]) * 1024 for line in pathlib.Path("/proc/meminfo").read_text().splitlines()}
        total, available, cpus = values["MemTotal"], values["MemAvailable"], os.cpu_count() or 0
    else:
        raise RuntimeError("matrix capacity checks support only macOS/Linux")
    free = shutil.disk_usage(lab.HOME).free
    if cpus < 12 or total < 40 * 1024**3 or available < 18 * 1024**3 or free < 24 * 1024**3:
        raise RuntimeError("use sequential fixtures: matrix requires12CPU/40GiB RAM/18GiB available/24GiB disk")
    return {"logical_cpus": cpus, "memory_bytes": total, "available_memory_bytes": available,
            "disk_free_bytes": free, "maximum_outer_guests": 3, "outer_memory_limit_bytes": 12 * 1024**3}


def cached_inputs():
    base = lab.HOME / "base/build.json"
    if not base.is_file() or any(not (lab.HOME / "identity" / name).is_file() for name in ("controller", "host")):
        raise RuntimeError("matrix requires an already sealed base and existing identities")
    expected = hashlib.sha256((lab.ROOT / "lab/inputs.json").read_bytes() + (lab.ROOT / "lab/bootstrap.sh").read_bytes()).hexdigest()
    if json.loads(base.read_text())["inputs_digest"] != expected:
        raise RuntimeError("sealed base inputs changed")
    lab.verify_image(lab.HOME / "debian.qcow2")


def child(component, descriptor, binaries, image_sha):
    validate_inherited_lock(descriptor, lab.HOME / "lab.lock")
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        if component == "guest":
            import remote_migrate_qualification
            result = remote_migrate_qualification.run(5400, "guest", image_sha)
        elif component == "scope":
            import physical_trial_suite
            result = physical_trial_suite.run(binaries, 1800)
        else:
            raise ValueError("matrix permits only the two declared components")
        return 0 if result["complete"] else 1
    finally:
        # Close our reference; do not explicitly unlock the shared description.
        os.close(descriptor)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-bin-dir", type=pathlib.Path, required=True)
    parser.add_argument("--image-sha256", required=True)
    parser.add_argument("--only", choices=("guest", "scope"), help="Run just one declared fixture under the same session/lock ownership checks")
    parser.add_argument("--child-component", choices=("guest", "scope"), help=argparse.SUPPRESS)
    parser.add_argument("--lock-fd", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--session-anchor", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--status-path", type=pathlib.Path, help=argparse.SUPPRESS)
    arguments = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{64}", arguments.image_sha256):
        parser.error("invalid pristine image digest")
    if arguments.child_component:
        if arguments.only is not None:
            parser.error("fixture selection belongs to the supervising parent")
        if arguments.lock_fd is None:
            parser.error("component requires the inherited lab lock")
        if arguments.session_anchor:
            if arguments.status_path is None:
                parser.error("session anchor requires its private status path")
            validate_inherited_lock(arguments.lock_fd, lab.HOME / "lab.lock")
            argv = [sys.executable, str(pathlib.Path(__file__).resolve()), "--child-component", arguments.child_component,
                    "--lock-fd", str(arguments.lock_fd), "--linux-bin-dir", str(arguments.linux_bin_dir.resolve()),
                    "--image-sha256", arguments.image_sha256]
            session_anchor(arguments.lock_fd, arguments.status_path, argv)
        if arguments.status_path is not None:
            parser.error("status path is internal to a session anchor")
        raise SystemExit(child(arguments.child_component, arguments.lock_fd, arguments.linux_bin_dir.resolve(), arguments.image_sha256))
    if arguments.lock_fd is not None or arguments.session_anchor or arguments.status_path is not None:
        parser.error("session options are internal to a declared component")
    # Keep one global exclusion lease throughout both isolated child lifetimes.
    with (lab.HOME / "lab.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        resources = capacity()
        cached_inputs()
        # Establish inspection permission before starting any owned session.
        process_table()
        directory = lab.HOME / ("qualification-matrix-" + time.strftime("%Y%m%dT%H%M%S"))
        directory.mkdir(mode=0o700)
        summary = {"schema": 1, "complete": False, "production_contacted": False, "resources": resources,
                   "single_global_lock_inode": os.fstat(lock.fileno()).st_ino, "components": {}}
        components = (arguments.only,) if arguments.only else ("guest", "scope")
        summary["selected_components"] = list(components)
        lab.atomic_json(directory / "summary.json", summary)
        print("Private matrix directory:", directory, flush=True)
        sessions, logs = [], []
        try:
            for name in components:
                logfile = (directory / (name + ".log")).open("wb")
                logs.append(logfile)
                argv = [sys.executable, str(pathlib.Path(__file__).resolve()), "--child-component", name,
                        "--session-anchor", "--status-path", str(directory / (name + "-status.json")),
                        "--lock-fd", str(lock.fileno()), "--linux-bin-dir", str(arguments.linux_bin_dir.resolve()),
                        "--image-sha256", arguments.image_sha256]
                session = spawn_registered(name, argv, directory / (name + "-status.json"), lock.fileno(), logfile, sessions)
                summary["components"][name] = {"pid": session.process.pid, "log": str(directory / (name + ".log"))}
                lab.atomic_json(directory / "summary.json", summary)
            while True:
                table = process_table()
                if all(session.finished(table) for session in sessions):
                    break
                time.sleep(1)
            for session in sessions:
                summary["components"][session.name]["exit_code"] = session.status().get("exit_code", 1)
            summary["complete"] = all(session.status().get("exit_code") == 0 for session in sessions)
        finally:
            with defer_cancellation():
                retire_sessions(sessions)
                for session in sessions:
                    details = summary["components"].setdefault(session.name, {"pid": session.process.pid})
                    details.update({"owned_group_retired": session.retired, "forced_descendant_cleanup": session.forced_cleanup})
                summary["complete"] &= all(session.retired and not session.forced_cleanup for session in sessions)
                for output in logs:
                    output.close()
                lab.atomic_json(directory / "summary.json", summary)
        print(json.dumps(summary, indent=2), flush=True)
        raise SystemExit(0 if summary["complete"] else 1)


if __name__ == "__main__":
    def stop(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    main()
