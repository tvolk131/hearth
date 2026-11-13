"""Executable protocol specification ONLY: no RPC, hypervisor, or storage adapter.

Local journal commits and observed physical effects are distinct transitions.
Authentication/signature verification is an abstract premise, not implemented
cryptography. Physical fencing is a required adapter contract, not something
this model implements or proves about Proxmox.
"""
from dataclasses import asdict, dataclass
import copy
import hashlib
import json


class Refused(Exception):
    pass


def require(condition, reason):
    if not condition:
        raise Refused(reason)


@dataclass(frozen=True)
class Plan:
    job: str = "migration-1"
    source: str = "source-controller/environment-1"
    target: str = "target-controller/environment-2"
    vm_uuid: str = "original-vm-uuid"
    source_disk_guid: str = "source-disk-guid"
    target_disk_guid: str = "reserved-target-disk-guid"
    owner: str = "alice"
    disk_charge: int = 8

    @property
    def binding(self):
        return hashlib.sha256(json.dumps(asdict(self), sort_keys=True).encode()).hexdigest()


class HandoffModel:
    """Two independently durable journals, an unreliable channel, and effects."""

    ACTIONS = (
        "reserve", "copy", "ready", "deliver_ready", "fence_intent",
        "install_fence", "stop", "record_stopped", "commit", "send_proof",
        "deliver_proof", "start", "record_running", "source_write",
        "expire", "renew", "drop_messages", "crash_source", "crash_target",
        "reboot_source", "reboot_target", "abort_source", "abort_target",
        "remove_target", "release_target",
    )

    def __init__(self, plan=None):
        self.plan = plan or Plan()
        self.source = {"phase": "running", "ready": None, "proof": None}
        self.target = {"phase": "empty", "claim": 0, "proof": None}
        self.effects = {
            "source_running": True, "source_fence": False, "source_data": "v1",
            "target_exists": False, "target_running": False, "target_data": None,
        }
        self.channel = {"ready": None, "proof": None}
        self.authorized = True

    def _grant(self):
        require(self.authorized, "job-scoped peer authorization expired")

    def _receipt(self):
        return {
            "binding": self.plan.binding, "issuer": self.plan.target,
            "target_disk_guid": self.plan.target_disk_guid,
            "data": self.effects["target_data"], "authenticated": True,
        }

    def reserve(self):
        self._grant()
        require(self.target["phase"] not in ("aborted", "aborting"), "job tombstone cannot be reused")
        if self.target["phase"] == "empty":
            self.target.update(phase="reserved", claim=self.plan.disk_charge)

    def copy(self):
        self._grant()
        require(self.target["phase"] in ("reserved", "copied", "ready"), "target cannot receive")
        self.effects.update(target_exists=True, target_data=self.effects["source_data"])
        self.target["phase"] = "copied"

    def ready(self):
        self._grant()
        require(self.target["phase"] in ("copied", "ready"), "copy missing")
        require(self.effects["target_exists"] and not self.effects["target_running"], "staging must stay inactive")
        self.target["phase"] = "ready"
        self.channel["ready"] = self._receipt()

    def deliver_ready(self):
        self._grant()
        receipt = self.channel["ready"]
        require(receipt is not None, "ready message lost")
        require(receipt.get("authenticated") and receipt.get("issuer") == self.plan.target,
                "wrong ready issuer")
        require(receipt.get("binding") == self.plan.binding and
                receipt.get("target_disk_guid") == self.plan.target_disk_guid, "wrong ready binding")
        self.source["ready"] = copy.deepcopy(receipt)

    def fence_intent(self):
        self._grant()
        require(self.source["phase"] not in ("aborted", "committed"), "terminal source")
        if self.source["phase"] == "running":
            self.source["phase"] = "fence_intent"

    def install_fence(self):
        require(self.source["phase"] in ("fence_intent", "fenced", "stopped", "committed"),
                "durable fence intent missing")
        self.effects["source_fence"] = True
        if self.source["phase"] == "fence_intent":
            self.source["phase"] = "fenced"

    def stop(self):
        require(self.effects["source_fence"], "persistent physical fence must precede stop")
        self.effects["source_running"] = False

    def record_stopped(self):
        require(self.source["phase"] in ("fenced", "stopped"), "not fencing")
        require(self.effects["source_fence"] and not self.effects["source_running"], "stop not observed")
        self.source["phase"] = "stopped"

    def commit(self):
        self._grant()
        if self.source["phase"] == "committed":
            return
        require(self.source["phase"] == "stopped", "durable stop verification missing")
        require(self.effects["source_fence"] and not self.effects["source_running"], "source may run")
        receipt = self.source["ready"]
        require(receipt is not None and receipt["data"] == self.effects["source_data"],
                "target has not verified final source data")
        # Commit locally BEFORE any proof can leave the source. This is not a
        # distributed transaction. Lost messages can cause indefinite downtime.
        self.source["phase"] = "committed"
        self.source["proof"] = {
            "binding": self.plan.binding, "issuer": self.plan.source,
            "target_disk_guid": self.plan.target_disk_guid,
            "data": receipt["data"], "authenticated": True,
        }

    def send_proof(self):
        self._grant()
        require(self.source["phase"] == "committed", "no committed handoff")
        self.channel["proof"] = copy.deepcopy(self.source["proof"])

    def deliver_proof(self):
        self._grant()
        proof = self.channel["proof"]
        require(proof is not None, "handoff message lost")
        require(proof.get("authenticated") and proof.get("issuer") == self.plan.source,
                "wrong handoff issuer")
        require(proof.get("binding") == self.plan.binding and
                proof.get("target_disk_guid") == self.plan.target_disk_guid, "wrong handoff binding")
        if self.target["phase"] in ("accepted", "running"):
            require(proof == self.target["proof"], "conflicting replay")
            return
        require(self.target["phase"] == "ready", "staging not verified or already removed")
        require(self.effects["target_exists"] and proof["data"] == self.effects["target_data"],
                "target data changed after readiness")
        self.target.update(phase="accepted", proof=copy.deepcopy(proof))

    def start(self):
        # An already accepted ownership decision does not expire. New peer
        # requests require a grant; local completion uses its durable proof.
        require(self.target["phase"] in ("accepted", "running"), "handoff not accepted")
        require(self.effects["target_exists"], "target data missing")
        self.effects["target_running"] = True

    def record_running(self):
        require(self.target["phase"] in ("accepted", "running") and
                self.effects["target_running"], "start not observed")
        self.target["phase"] = "running"

    def source_write(self):
        require(self.effects["source_running"], "stopped source cannot write")
        self.effects["source_data"] = "v2" if self.effects["source_data"] == "v1" else "v1"

    def expire(self):
        self.authorized = False

    def renew(self):
        # Models a separately approved renewal for the IDENTICAL plan only.
        self.authorized = True

    def drop_messages(self):
        self.channel = {"ready": None, "proof": None}

    def crash_source(self):
        self.source = json.loads(json.dumps(self.source))

    def crash_target(self):
        self.target = json.loads(json.dumps(self.target))

    def reboot_source(self):
        self.crash_source()
        self.effects["source_running"] = not self.effects["source_fence"] and self.source["phase"] in ("running", "aborted")

    def reboot_target(self):
        self.crash_target()
        self.effects["target_running"] = self.target["phase"] in ("accepted", "running") and self.effects["target_exists"]

    def abort_source(self):
        require(self.source["phase"] != "committed", "committed ownership cannot roll back")
        self.source["phase"] = "aborted"
        self.effects.update(source_fence=False, source_running=True)

    def abort_target(self):
        require(self.target["phase"] not in ("accepted", "running"), "owned target cannot be discarded")
        if self.target["phase"] != "aborted":
            self.target["phase"] = "aborting"

    def remove_target(self):
        require(self.target["phase"] == "aborting", "cleanup intent missing")
        self.effects.update(target_exists=False, target_running=False, target_data=None)

    def release_target(self):
        require(self.target["phase"] == "aborting", "cleanup intent missing")
        require(not self.effects["target_exists"] and not self.effects["target_running"],
                "target absence not observed")
        self.target.update(phase="aborted", claim=0)

    def check(self):
        assert not (self.effects["source_running"] and self.effects["target_running"]), "two running copies"
        if self.effects["target_running"]:
            assert self.target["proof"] is not None and self.source["phase"] == "committed"
            assert self.effects["source_fence"] and not self.effects["source_running"]
            assert self.effects["target_data"] == self.target["proof"]["data"]
        if self.effects["target_exists"]:
            assert self.target["claim"] == self.plan.disk_charge, "unaccounted target data"
        if self.source["phase"] == "committed":
            assert self.effects["source_fence"], "committed source unfenced"

    def state_key(self):
        return json.dumps({"source": self.source, "target": self.target,
                           "effects": self.effects, "channel": self.channel,
                           "authorized": self.authorized}, sort_keys=True)
