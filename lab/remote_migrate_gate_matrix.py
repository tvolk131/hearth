#!/usr/bin/env python3
"""Combine explicitly independent actual PVE gates; never performs host actions."""
import argparse
import hashlib
import json
from pathlib import Path
import time

import lab
import remote_migrate_qualification as qualification
import remote_migrate_qualification_worker as fixture

ROOT = qualification.ROOT


def require(value, description):
    if not value: raise RuntimeError(description)


def read(report, name):
    return json.loads((report / (name + ".json")).read_text())


def terminal(report, name, *, successful):
    task = read(report, name)
    latest = task["observations"][-1]
    require(latest["status"] == "stopped", "native task outcome is not terminal")
    require(isinstance(latest.get("exitstatus"), str) and bool(latest["exitstatus"]), "native exit status is unknown")
    require((latest.get("exitstatus") == "OK") == successful, "unexpected native task outcome")
    return task["upid"]


def cleanup(report, summary):
    require(summary.get("qemu_children_stopped") is True, "owned QEMU cleanup is not proven")
    for role in ("a", "b"):
        require(read(report, role + "-cleanup_tokens").get("all_spike_tokens_removed") is True,
                "API token cleanup is not proven")
    stopped = read(report, "a-tunnel_stop")
    require(stopped.get("owned_tunnel_stopped") is True or stopped.get("already_absent") is True,
            "SSH tunnel cleanup is not proven")
    require(read(report, "b-tunnel_account_remove").get("temporary_ssh_account_removed") is True,
            "temporary peer SSH authority was not removed")
    private = lab.HOME / report.name.replace("remote-migrate-qualification-", "migration-qualification-", 1) / "qemu-cleanup.json"
    recorded = json.loads(private.read_text())
    require(set(recorded["roles"]) == {"a", "b"} and all(v["exit"] is not None for v in recorded["roles"].values()),
            "recorded owned emulator children were not reaped")
    return {"record": str(private.relative_to(ROOT)), "sha256": digest(private), "exits": recorded["roles"]}


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def descriptor(report, summary, ownership):
    files = {}
    for path in sorted(report.rglob("*")):
        if path.is_file():
            require(not path.is_symlink(), "evidence contains a symlink")
            files[str(path.relative_to(report))] = digest(path)
    return {"report": str(report.relative_to(ROOT)), "parent_run_complete": summary["complete"],
            "parent_failure": summary.get("failure"),
            "mode": summary.get("mode", "failed combined attempt; interruption phase only"),
            "outer_tcg_threading": summary["outer_tcg_threading"],
            "program_sha256": summary["program_sha256"], "files_sha256": files,
            "owned_process_cleanup": ownership}


def interruption_gate(report):
    summary = read(report, "summary")
    require(summary["complete"] is False and bool(summary.get("failure")), "failed parent attempt must remain explicitly incomplete")
    require("real_mid_copy_ssh_failure_native_upid_terminal_and_both_guests_stopped" in summary["checks"],
            "interruption phase did not pass")
    boundary = read(report, "interruption-boundary")
    require(boundary["task"]["status"] == "running" and boundary["transport"]["largest_bytes_acked"] > 2 * 1024**2,
            "copy was not demonstrably in progress before interruption")
    upid = terminal(report, "interrupted-task", successful=False)
    require(upid == read(report, "interrupted-submission")["upid"], "interruption native receipt changed")
    require(read(report, "interrupted-before-cleanup")["status"] == "stopped", "cleanup interrupted a still-active native task")
    for role in ("a", "b"):
        inventory = read(report, role + "-after-interruption")
        require(all(vm["status"] == "stopped" for vm in inventory["vms"]), "interrupted copy left an active VM")
        if role == "b" and "9001" in inventory["configs"]:
            probe = read(report, "interrupted-target-start-refused")
            require(probe["start_exit"] != 0 and probe["running_after_start"] is False, "target staging permits ordinary start")
    protected = read(report, "protected-source-small-fixture")
    require(protected["protection"] == 1 and protected["onboot"] == 0, "interruption source protection profile differs")
    return summary, cleanup(report, summary)


def fence_gate(report):
    summary = read(report, "summary")
    require(summary["complete"] is True and summary.get("mode") == "fence", "source fence profile is incomplete")
    require(summary["outer_tcg_threading"]["a"] == "single" and summary.get("guest_os_boot_verified") is False,
            "fence fixture threading or no-OS-claim contract differs")
    upid = terminal(report, "fence-copy-task", successful=True)
    require(upid == read(report, "fence-copy-submission")["upid"], "fence copy receipt differs")
    source = read(report, "protected-source-small-fixture")
    target = read(report, "protected-small-target-after")
    for value in (source, target):
        require(value["protection"] == 1 and value["onboot"] == 0, "protected fixture config changed")
    require(source["root_sha256"] == target["root_sha256"], "fence copy root bytes differ")
    old, new = read(report, "source-before-reboot"), read(report, "source-after-reboot")
    require(old["boot_id"] != new["boot_id"] and old["machine_id_sha256"] == new["machine_id_sha256"],
            "genuine source reboot with stable machine identity is not proven")
    retained = new["inventory"]["configs"]["9001"]
    require(retained.get("lock") == "migrate" and retained.get("protection") == 1 and retained.get("onboot", 0) == 0,
            "source protected migration fence did not persist in config")
    refused = read(report, "rebooted-source-start-refused")
    require(refused["start_exit"] != 0 and not refused["running_after_start"] and "locked (migrate)" in refused["stderr"],
            "ordinary retained-source start was not fenced")
    active = read(report, "active-target-during-source-fence")
    require(any(int(v["vmid"]) == 9001 and v["status"] == "running" for v in active["vms"]), "target QEMU was not active")
    return summary, cleanup(report, summary)


def generated_schema_proof(generated):
    require(generated.get("schema") == "users-list/default" and isinstance(generated.get("content"), str),
            "actual PVE user-data default-user schema was not checked before boot")
    reconstructed = fixture.generated_user_schema(generated["content"])
    require(generated.get("sha256") == reconstructed["sha256"] and generated["schema"] == reconstructed["schema"],
            "actual PVE user-data schema/hash proof does not reconstruct")


def guest_gate(report):
    summary = read(report, "summary")
    require(summary["complete"] is True and summary.get("mode") == "guest", "bootable guest profile is incomplete")
    require(summary["outer_tcg_threading"] == {"a": "multi", "b": "multi"} and summary.get("source_host_reboot_tested") is False,
            "guest fixture threading or no-reboot-claim contract differs")
    require(summary.get("guest_os_boot_verified") is True, "guest OS boot was not verified")
    upid = terminal(report, "bootable-transfer-task", successful=True)
    require(upid == read(report, "lost-ack-recovered-receipt")["upid"], "lost-ack native receipt differs")
    require(read(report, "lost-ack-before-cleanup")["status"] == "stopped", "native copy uncertain before cleanup")
    before, after = read(report, "bootable-before"), read(report, "bootable-after")
    generated = read(report, "source-guest-created").get("generated_user_data", {})
    generated_schema_proof(generated)
    for field in ("root_sha256", "root_size", "smbios1", "vmgenid", "cloudinit_sha256", "cloudinit_meta_sha256", "cloudinit_size", "cicustom", "protection", "onboot"):
        require(before[field] == after[field], "bootable copy changed " + field)
    for value in (before, after):
        require(value["root_size"] == 4 * 1024**3 and value["protection"] == 1 and value["onboot"] == 0, "bootable protected profile differs")
        for name in ("root", "cloudinit"):
            require(value[name + "_refreservation"] >= value[name + "_size"], "a guest disk is sparse")
    first, moved = read(report, "source-guest-identity"), read(report, "target-guest-identity")
    for value in (first, moved):
        require(value["cloud_exit"] == 0 and value["cloud"]["status"] == "done", "cloud-init did not complete healthily")
    for field in ("machine_id", "instance_id", "ssh_host_public", "sentinel_sha256"):
        require(first[field] == moved[field], "guest identity changed: " + field)
    retained = read(report, "a-final-inventory")
    config = retained["configs"]["9002"]
    require(config.get("lock") == "migrate" and config.get("protection") == 1 and config.get("onboot", 0) == 0,
            "bootable retained source is not protected and fenced")
    require(all(vm["status"] == "stopped" for vm in retained["vms"]), "bootable retained source is active")
    staged = read(report, "staged-custom-metadata")
    require(staged["a"] == staged["b"] and first["instance_id"] == staged["a"]["instance_id"], "custom snippet identity is not preserved")
    image, manifest = qualification.verified_cached_image(summary["guest_image_sha256"])
    require(read(report, "reused-pristine-image-manifest")["image_sha256"] == manifest["image_sha256"], "qualified guest image differs from pristine cache")
    return summary, cleanup(report, summary)


def compose(interruption, fence, guest):
    reports = [Path(path).resolve() for path in (interruption, fence, guest)]
    for report in reports:
        require(report.parent == (ROOT / "evidence").resolve() and report.name.startswith("remote-migrate-qualification-"), "only exported local lab reports are accepted")
    checked = [interruption_gate(reports[0]), fence_gate(reports[1]), guest_gate(reports[2])]
    summaries = [value[0] for value in checked]
    require(all(value.get("production_contacted") is False and value.get("iroh_used") is False for value in summaries), "unexpected production or iroh scope")
    for report in reports:
        binding, tunnel = read(report, "loopback-api"), read(report, "scoped-ssh-tunnel")
        listeners = binding["destination_api_listeners"]
        require(len(listeners) == 1 and "127.0.0.1:8006" in listeners[0], "destination API was not exclusively loopback-bound")
        require(binding["only_forward_target"] == "127.0.0.1:8006" and binding["ssh_account_shell_sessions"] == 0,
                "peer SSH authority was not constrained to the fixed API forward")
        require(tunnel.get("direct_api_refused") is True and tunnel.get("tls_pin_verified") is True and tunnel.get("iroh_used") is False,
                "private API tunnel route/pin checks did not pass")
    return {"schema": "pve-native-independent-gates/1", "complete": True,
            "all_gates_same_vm_and_run": False, "production_contacted": False, "iroh_used": False,
            "qemu_children_stopped": True,
            "checks": ["loopback_only_destination_api_over_pinned_scoped_ssh_forward",
                       "real_mid_copy_ssh_failure_native_upid_terminal_and_both_guests_stopped",
                       "lost_ack_reconciled_without_native_resubmission_and_exact_bootable_copy",
                       "target_guest_os_boot_ssh_key_file_and_cloud_init_instance_identity_preserved",
                       "retained_source_lock_survives_host_reboot_and_refuses_start_while_target_is_active"],
            "rust_migration_rpc_implemented": False, "guest_image_sha256": summaries[2]["guest_image_sha256"],
            "decision": "conditional go for a supervised PVE-only disposable physical trial",
            "gates": {"real_interrupted_transfer_and_authoritative_reconciliation": True,
                      "protected_cold_copy_and_source_reboot_fence": True,
                      "bootable_copy_ssh_file_instance_identity_and_persisted_receipt_lost_ack": True},
            "reports": {name: descriptor(report, pair[0], pair[1]) for name, report, pair in zip(("interruption", "fence", "guest"), reports, checked)},
            "limits": ["the interruption parent run remains failed; only its completed native phase is reused",
                       "failed native copies retain staging/snapshots and require guarded reconciliation/cleanup",
                       "source reboot and guest boot/move were tested in different VM runs",
                       "the SSH tunnel is not an implemented iroh peer tunnel or Rust migration state machine",
                       "persisted-receipt lost-ack drill leaves an acceptance-before-persistence ambiguity window",
                       "no live migration, arbitrary root override or large real-user VM is qualified"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--interruption", required=True)
    parser.add_argument("--fence", required=True)
    parser.add_argument("--guest", required=True)
    args = parser.parse_args()
    summary = compose(args.interruption, args.fence, args.guest)
    export = ROOT / "evidence" / ("remote-migrate-composite-" + time.strftime("%Y%m%dT%H%M%S"))
    export.mkdir(mode=0o700)
    lab.atomic_json(export / "summary.json", summary)
    print(str(export / "summary.json"))
