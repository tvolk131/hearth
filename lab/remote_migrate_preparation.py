#!/usr/bin/env python3
"""Read-only proof assembly for a supervised physical trial, not qualification."""
import argparse
import json
from pathlib import Path
import re
import time

import lab
import remote_migrate_gate_matrix as matrix

ROOT = matrix.ROOT
require, read = matrix.require, matrix.read


def private_api_gate(report):
    binding, tunnel = read(report, "loopback-api"), read(report, "scoped-ssh-tunnel")
    listeners = binding["destination_api_listeners"]
    require(len(listeners) == 1 and "127.0.0.1:8006" in listeners[0], "destination API was not exclusively loopback-bound")
    require(binding["only_forward_target"] == "127.0.0.1:8006" and binding["ssh_account_shell_sessions"] == 0,
            "peer SSH authority was not constrained to the fixed API forward")
    require(tunnel.get("direct_api_refused") is True and tunnel.get("tls_pin_verified") is True and tunnel.get("iroh_used") is False,
            "private API refusal/tunnel/pin proof is missing")


def first_ci_import_timeout(report):
    upid = matrix.terminal(report, "bootable-transfer-task", successful=False)
    require(upid == read(report, "lost-ack-recovered-receipt")["upid"], "failed import native receipt differs")
    final = read(report, "lost-ack-before-cleanup")
    require(final["status"] == "stopped" and final.get("exitstatus") == "migration aborted",
            "native import was not authoritatively stopped before cleanup")
    task = read(report, "bootable-transfer-task")["observations"][-1]
    require(task.get("exitstatus") == "migration aborted", "unexpected native import failure")
    imports = []
    for line in task.get("log", []):
        text = line["t"]
        match = re.search(r"tunnel command '(\{.*?\})' failed", text)
        if match:
            command = json.loads(match.group(1))
            if command.get("cmd") == "disk-import":
                imports.append((command, text))
    require(bool(imports), "no structured native disk-import timeout receipt")
    for command, text in imports:
        require(command.get("volname") == "vm-9002-cloudinit" and command.get("storage") == "dst-alloc",
                "native timeout did not concern the first cloud-init volume")
        require("failed to read readiness from disk import child: got timeout" in text,
                "native failure was not the bounded child readiness timeout")
    require(not any("successfully imported" in line["t"] for line in task.get("log", [])),
            "import log unexpectedly reports completed data transfer")
    return upid


def source_boot_gate(report):
    summary = read(report, "summary")
    require(summary["complete"] is False and summary.get("mode") == "guest" and bool(summary.get("failure")),
            "source preparation must preserve the failed guest parent run")
    require(summary["outer_tcg_threading"] == {"a": "multi", "b": "multi"}, "source boot fixture threading differs")
    identity = read(report, "source-guest-identity")
    require(identity["cloud_exit"] == 0 and identity["cloud"]["status"] == "done"
            and identity["cloud"].get("extended_status") == "done"
            and not identity["cloud"].get("errors") and not identity["cloud"].get("recoverable_errors"),
            "strict healthy source cloud-init/SSH readiness did not pass")
    for key in ("machine_id", "instance_id", "ssh_host_public", "sentinel_sha256"):
        require(isinstance(identity.get(key), str) and bool(identity[key]), "missing source identity field " + key)
    require(re.fullmatch("[a-f0-9]{64}", identity["sentinel_sha256"]) is not None, "invalid source sentinel digest")
    staged = read(report, "staged-custom-metadata")
    require(staged["a"] == staged["b"] and identity["instance_id"] == staged["a"]["instance_id"],
            "source instance ID or staged custom metadata differs")
    created = read(report, "source-guest-created")
    matrix.generated_schema_proof(created.get("generated_user_data", {}))
    before = read(report, "bootable-before")
    require(before["status"] == "stopped" and before["root_size"] == 4 * 1024**3
            and before["protection"] == 1 and before["onboot"] == 0, "source stopped/protected baseline differs")
    for name in ("root", "cloudinit"):
        require(before[name + "_refreservation"] >= before[name + "_size"], "source " + name + " is sparse")
    for key in ("smbios1", "vmgenid", "root_sha256", "cloudinit_sha256"):
        require(bool(before.get(key)), "source baseline missing " + key)
    require(before.get("cicustom") == "meta=spike-snippets:snippets/identity.yaml", "source metadata dependency differs")
    image, manifest = matrix.qualification.verified_cached_image(summary["guest_image_sha256"])
    require(read(report, "reused-pristine-image-manifest")["image_sha256"] == manifest["image_sha256"],
            "source boot image differs from the pristine export")
    export = read(report, "pristine-image-export")
    require(export["sha256"] == manifest["image_sha256"] and export["exported_before_guest_import_or_boot"] is True
            and export["root_zvol_exported"] is False, "pristine export proof differs")
    first_ci_import_timeout(report)
    return summary, matrix.cleanup(report, summary)


def compose(interruption, fence, source_boot):
    reports = [Path(path).resolve() for path in (interruption, fence, source_boot)]
    for report in reports:
        require(report.parent == (ROOT / "evidence").resolve() and report.name.startswith("remote-migrate-qualification-"),
                "only exported local lab reports are accepted")
    checked = [matrix.interruption_gate(reports[0]), matrix.fence_gate(reports[1]), source_boot_gate(reports[2])]
    summaries = [pair[0] for pair in checked]
    for report, summary in zip(reports, summaries):
        require(summary.get("production_contacted") is False and summary.get("iroh_used") is False, "unexpected test scope")
        private_api_gate(report)
    return {"schema": "pve-native-physical-preparation/1", "complete": False,
            "preparation_complete": True, "bootable_transfer_qualified": False,
            "production_contacted": False, "qemu_children_stopped": True, "iroh_used": False,
            "all_gates_same_vm_and_run": False, "rust_migration_rpc_implemented": False,
            "guest_image_sha256": summaries[2]["guest_image_sha256"],
            "decision": "prepared for an explicitly supervised isolated physical trial; remaining copy gate must run there",
            "checks": ["loopback_only_destination_api_over_pinned_scoped_ssh_forward",
                       "real_mid_copy_ssh_failure_native_upid_terminal_and_both_guests_stopped",
                       "retained_source_lock_survives_host_reboot_and_refuses_start_while_target_is_active",
                       "strict_source_guest_cloud_init_ssh_identity_and_default_user_schema_verified",
                       "first_cloudinit_import_native_readiness_timeout_reconciled"],
            "reports": {name: matrix.descriptor(report, pair[0], pair[1]) for name, report, pair in
                        zip(("interruption", "fence", "source_boot"), reports, checked)},
            "remaining_gates": ["successful bootable native copy and target guest boot",
                                "preserved machine/SSH/file/cloud-init instance identity on actual target",
                                "successful persisted-receipt lost-ack reconciliation for a completed bootable copy",
                                "actual hardware data-path, thick allocation, fence and final guarded cleanup checks"],
            "limits": ["failed local parent runs remain failed and all linked file hashes are retained",
                       "no native timeout was patched or relaxed; the first CI import timed out before data transfer",
                       "source-host reboot and source guest boot were separate actual lab runs",
                       "native PVE primitive only; no Rust migration API or iroh peer tunnel is implemented"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--interruption", required=True)
    parser.add_argument("--fence", required=True)
    parser.add_argument("--source-boot", required=True)
    args = parser.parse_args()
    summary = compose(args.interruption, args.fence, args.source_boot)
    export = ROOT / "evidence" / ("remote-migrate-preparation-" + time.strftime("%Y%m%dT%H%M%S"))
    export.mkdir(mode=0o700)
    lab.atomic_json(export / "summary.json", summary)
    print(str(export / "summary.json"))
