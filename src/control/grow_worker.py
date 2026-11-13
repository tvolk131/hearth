"""Fixed growth phases, executed only after the compiled-in shrink helpers.

Rust prepends shrink_worker.py with a non-main __name__, then this source with
__name__='__main__'. Caller input never supplies Python source or command paths.
Each mutating phase is single-use and receipts are observations, not permission
to replay an interrupted filesystem command. `inspect` is read-only.
"""


def declared_gpt_size(disk, actual_size):
    # Imported images may still have a valid backup header before the zvol end.
    # Validate that complete existing pair before relocating it during growth.
    with open(disk, "rb", buffering=0) as stream:
        stream.seek(512)
        header = stream.read(512)
    require(len(header) == 512 and header[:8] == b"EFI PART", "missing primary GPT")
    span = (struct.unpack_from("<Q", header, 32)[0] + 1) * 512
    require(68*512 <= span <= actual_size, "GPT span exceeds actual volume")
    verify_gpt(disk, span)
    return span


def grow_marker(name):
    return run("zfs", "get", "-H", "-o", "value", "org.pve-dev:grow", name)


def growth_source(p, baseline, sizes):
    current = props(p["source_name"])
    stable = set(PROPS) - {"volsize", "refreservation"}
    require({key:current[key] for key in stable} == {key:baseline["source"][key] for key in stable},
            "growth source identity/ownership/flags changed")
    require(current["type"] == "volume" and current["origin"] == "-" and current["readonly"] == "off"
            and int(current["volsize"]) in sizes and int(current["refreservation"]) >= int(current["volsize"]),
            "growth source is not independently thick at an admitted size")
    require(names(p["source_name"]) == [p["source_name"]], "growth refuses snapshots or descendants")
    return current


def growth_config(p, baseline, size_mib, locked):
    value = api("get", p["vm_path"] + "/config")
    attachment = value.get("scsi0", "")
    expected_attachment = baseline["original_config"]["scsi0"]
    require([x for x in attachment.split(",") if not x.startswith("size=")]
            == [x for x in expected_attachment.split(",") if not x.startswith("size=")]
            and attachment_size(attachment) == size_mib*MIB, "growth attachment changed")
    expected = dict(baseline["original_config"], scsi0=attachment)
    config(p["vm_path"], expected, locked)
    return value, expected


def growth_baseline(p):
    baseline = read_receipt(p, "grow-baseline")
    prepared = _receipt_proof(p, "grow-prepare")
    require(prepared is not None and p.get("baseline") == prepared
            and all(prepared.get(key) == value for key, value in baseline.items()),
            "growth baseline differs from immutable preparation receipt")
    return baseline


def grow_geometry(p, baseline):
    sectors = p["target_mib"]*MIB//512 - 2048 - baseline["before"]["root_start"]
    require(sectors > baseline["before"]["root_size"], "growth would not enlarge root partition")
    return sectors


def growth_snapshot(p, baseline, sizes, marker=True):
    source = growth_source(p, baseline, sizes)
    foreign_references(p)
    if marker:
        require(grow_marker(p["source_name"]) == p["operation_id"], "growth fence marker changed")
    disk = device(p["source_name"])
    current, root = layout(disk, baseline["before"]["root_partition_uuid"])
    preserve(baseline["before"], current, int(source["volsize"]))
    require(partition_hashes(disk, current) == baseline["other_partition_sha256"],
            "growth changed non-root partition bytes")
    span = declared_gpt_size(disk, int(source["volsize"]))
    return source, current, root, span


def grow_prepare(p):
    require(grow_marker(p["source_name"]) == "-", "another growth operation marker exists")
    cfg = config(p["vm_path"], p["config"], False)
    same_volume(p["source_name"], p["source"], p["old_mib"]*MIB)
    require(p["source"]["readonly"] == "off", "growth requires writable source")
    require(names(p["source_name"]) == [p["source_name"]], "growth refuses source snapshots or descendants")
    foreign_references(p)
    disk = device(p["source_name"])
    before, root = layout(disk, p.get("root_partition_uuid"))
    span = declared_gpt_size(disk, p["old_mib"]*MIB)
    run("e2fsck", "-f", "-n", root)
    baseline = {"source":p["source"], "original_config":p["config"], "before":before,
                "gpt_span":span, "other_partition_sha256":partition_hashes(disk, before)}
    grow_geometry(p, baseline)
    write_receipt(p, "grow-baseline", baseline, create=True)
    api("set", p["vm_path"] + "/config", digest=cfg["digest"], lock="migrate")
    config(p["vm_path"], p["config"], True)
    run("zfs", "set", "org.pve-dev:grow=" + p["operation_id"], p["source_name"])
    actual, current, _, current_span = growth_snapshot(p, baseline, {p["old_mib"]*MIB})
    require(actual == baseline["source"] and current == before and current_span == span,
            "growth source changed during preparation")
    run("zpool", "sync", p["pool"])
    proof = dict(baseline, prepared=True)
    proof["receipt"] = write_receipt(p, "grow-prepare", proof)
    return proof


def grow_volume(p):
    baseline = growth_baseline(p)
    require(read_receipt(p, "grow-volume", optional=True) is None, "volume growth already has receipt; inspect instead")
    source, current, _, span = growth_snapshot(p, baseline, {p["old_mib"]*MIB})
    require(current == baseline["before"] and span == baseline["gpt_span"], "unexpected pre-growth layout")
    growth_config(p, baseline, p["old_mib"], True)
    run("zfs", "set", "volsize=" + str(p["target_mib"]*MIB), p["source_name"])
    run("zfs", "set", "refreservation=auto", p["source_name"])
    source, current, _, span = growth_snapshot(p, baseline, {p["target_mib"]*MIB})
    require(current == baseline["before"] and span == baseline["gpt_span"], "volume extension changed guest layout")
    require(int(run("blockdev", "--getsize64", device(p["source_name"]))) == p["target_mib"]*MIB,
            "kernel did not expose target size")
    run("zpool", "sync", p["pool"])
    proof = {"source":source, "layout":current, "gpt_span":span}
    proof["receipt"] = write_receipt(p, "grow-volume", proof)
    return proof


def grow_partition(p):
    baseline = growth_baseline(p)
    require(read_receipt(p, "grow-partition", optional=True) is None, "partition growth already has receipt; inspect instead")
    volume = read_receipt(p, "grow-volume")
    source, current, root, span = growth_snapshot(p, baseline, {p["target_mib"]*MIB})
    require(source == volume["source"] and current == baseline["before"] and span == baseline["gpt_span"],
            "unexpected pre-partition growth effect")
    growth_config(p, baseline, p["old_mib"], True)
    disk = device(p["source_name"])
    run("sgdisk", "--move-second-header", disk)
    verify_gpt(disk, p["target_mib"]*MIB)
    sectors = grow_geometry(p, baseline)
    run("sfdisk", "--lock=yes", "--wipe=never", "--wipe-partitions=never", "-N", str(partition_number(disk, root)),
        disk, data=f"size={sectors}\n")
    run("sgdisk", "--verify", disk)
    verify_gpt(disk, p["target_mib"]*MIB)
    source, current, _, span = growth_snapshot(p, baseline, {p["target_mib"]*MIB})
    require(current["root_size"] == sectors and current["filesystem_bytes"] == baseline["before"]["filesystem_bytes"]
            and span == p["target_mib"]*MIB, "root partition growth did not match plan")
    run("zpool", "sync", p["pool"])
    proof = {"source":source, "layout":current, "gpt_span":span}
    proof["receipt"] = write_receipt(p, "grow-partition", proof)
    return proof


def grown_filesystem(p, baseline, current):
    require(current["root_size"] == grow_geometry(p, baseline), "root partition target differs")
    expected = current["root_size"]*512//4096*4096
    require(current["filesystem_bytes"] == expected and expected > baseline["before"]["filesystem_bytes"],
            "filesystem does not fill the admitted grown root partition")


def grow_filesystem(p):
    baseline = growth_baseline(p)
    require(read_receipt(p, "grow-filesystem", optional=True) is None, "filesystem growth already has receipt; inspect instead")
    partition = read_receipt(p, "grow-partition")
    source, current, root, span = growth_snapshot(p, baseline, {p["target_mib"]*MIB})
    require(source == partition["source"] and current == partition["layout"] and span == partition["gpt_span"],
            "unexpected pre-filesystem growth effect")
    cfg, _ = growth_config(p, baseline, p["old_mib"], True)
    run("e2fsck", "-f", "-p", root, allowed=(0, 1))
    run("resize2fs", root)
    run("e2fsck", "-f", "-n", root)
    source, current, _, span = growth_snapshot(p, baseline, {p["target_mib"]*MIB})
    grown_filesystem(p, baseline, current)
    attachment = re.sub(r",size=[^,]+", "", cfg["scsi0"]) + f",size={p['target_mib']}M"
    api("set", p["vm_path"] + "/config", digest=cfg["digest"], skiplock=1, scsi0=attachment)
    _, expected = growth_config(p, baseline, p["target_mib"], True)
    run("zpool", "sync", p["pool"])
    proof = {"source":source, "layout":current, "gpt_span":span, "config":expected}
    proof["receipt"] = write_receipt(p, "grow-filesystem", proof)
    return proof


def grow_unlock(p):
    baseline = growth_baseline(p)
    require(read_receipt(p, "grow-unlock", optional=True) is None, "growth unlock already has receipt; inspect instead")
    filesystem = read_receipt(p, "grow-filesystem")
    source, current, root, span = growth_snapshot(p, baseline, {p["target_mib"]*MIB})
    require(source == filesystem["source"] and current == filesystem["layout"] and span == filesystem["gpt_span"],
            "grown disk differs from verified filesystem receipt")
    grown_filesystem(p, baseline, current)
    run("e2fsck", "-f", "-n", root)
    cfg, expected = growth_config(p, baseline, p["target_mib"], True)
    run("zfs", "inherit", "org.pve-dev:grow", p["source_name"])
    require(grow_marker(p["source_name"]) == "-", "growth marker could not be cleared")
    api("set", p["vm_path"] + "/config", digest=cfg["digest"], skiplock=1, delete="lock")
    growth_config(p, baseline, p["target_mib"], False)
    run("zpool", "sync", p["pool"])
    proof = {"grown":True, "source":source, "layout":current, "gpt_span":span, "config":expected}
    proof["receipt"] = write_receipt(p, "grow-unlock", proof)
    return proof


def grow_quick_inspect(p):
    result = {"classification":"full_verification_required", "state":"full_verification_required",
              "candidate_phases":{}, "verified_phases":{}, "abort_candidate":False, "abort_safe":False, "bytes_verified":False}
    try:
        baseline = read_receipt(p, "grow-baseline")
        source = growth_source(p, baseline, {p["old_mib"]*MIB, p["target_mib"]*MIB})
        marker = grow_marker(p["source_name"])
        actual = api("get", p["vm_path"] + "/config")
        require(actual.get("lock") in (None, "migrate"), "foreign VM lock")
        locked = actual.get("lock") == "migrate"
        size_mib = int(attachment_size(actual["scsi0"]))//MIB
        require(size_mib in (p["old_mib"], p["target_mib"]), "unexpected root size annotation")
        growth_config(p, baseline, size_mib, locked)
        result["observed"] = {"source_guid":source["guid"], "volsize":source["volsize"], "locked":locked, "marker":marker}
        if int(source["volsize"]) == p["old_mib"]*MIB:
            require(size_mib == p["old_mib"] and ((locked and marker == p["operation_id"]) or (not locked and marker == "-")),
                    "partial growth fence")
            result["abort_candidate"] = all(read_receipt(p, "grow-" + phase, optional=True) is None
                                              for phase in ("volume", "partition", "filesystem", "unlock"))
            prepared = _receipt_proof(p, "grow-prepare")
            if locked and prepared: result["candidate_phases"]["grow_prepare"] = prepared
            return result
        require((locked and marker in (p["operation_id"], "-")) or (not locked and marker == "-"), "growth marker/fence changed")
        phases = ("volume", "partition") if locked and marker == p["operation_id"] and size_mib == p["old_mib"] \
            else ("filesystem",) if locked and marker == p["operation_id"] and size_mib == p["target_mib"] \
            else ("unlock",) if not locked and size_mib == p["target_mib"] else ()
        for phase in phases:
            saved = _receipt_proof(p, "grow-" + phase)
            if saved and saved["source"] == source: result["candidate_phases"]["grow_" + phase] = saved
        return result
    except (OSError, RuntimeError, ValueError, KeyError, TypeError) as error:
        result.update(candidate_phases={}, abort_candidate=False, error=str(error)[:1500])
        return result


def grow_inspect(p):
    if p.get("quick", False): return grow_quick_inspect(p)
    result = {"state":"ambiguous", "verified_phases":{}, "abort_safe":False}
    try:
        baseline = read_receipt(p, "grow-baseline")
        result["phase_receipts"]={}
        for phase in ("prepare", "volume", "partition", "filesystem", "unlock"):
            saved=_receipt_proof(p,"grow-"+phase)
            if saved is not None:result["phase_receipts"]["grow_"+phase]=saved
        source, current, root, span = growth_snapshot(p, baseline, {p["old_mib"]*MIB, p["target_mib"]*MIB}, marker=False)
        marker = grow_marker(p["source_name"])
        actual = api("get", p["vm_path"] + "/config")
        locked = actual.get("lock") == "migrate"
        require(actual.get("lock") in (None, "migrate"), "foreign VM lock")
        size_mib = int(attachment_size(actual["scsi0"]))//MIB
        require(size_mib in (p["old_mib"], p["target_mib"]), "unrecognized attachment annotation")
        growth_config(p, baseline, size_mib, locked)
        result["observed"] = {"source":source, "layout":current, "gpt_span":span, "marker":marker, "locked":locked}
        if int(source["volsize"]) == p["old_mib"]*MIB:
            require(current == baseline["before"] and span == baseline["gpt_span"] and size_mib == p["old_mib"],
                    "unexpected layout at original size")
            require((marker == p["operation_id"] and locked) or (marker == "-" and not locked), "partial growth fence")
            result.update(state="prepared" if locked else "untouched", abort_safe=True)
            prepared = _receipt_proof(p, "grow-prepare")
            if prepared and locked:
                result["verified_phases"]["grow_prepare"] = prepared
            aborted = _receipt_proof(p, "grow-abort")
            if aborted and not locked:
                result.update(state="aborted", verified_phases={"grow_abort":aborted})
            return result
        require(marker in (p["operation_id"], "-") and (locked or marker == "-"), "growth ownership marker changed")
        require(current["root_size"] in (baseline["before"]["root_size"], grow_geometry(p, baseline)),
                "unrecognized root partition size")
        prepared, volume = _receipt_proof(p, "grow-prepare"), _receipt_proof(p, "grow-volume")
        require(prepared is not None, "growth effect has no preparation receipt")
        if current["root_size"] == baseline["before"]["root_size"]:
            require(current == baseline["before"] and locked and marker == p["operation_id"] and size_mib == p["old_mib"],
                    "invalid volume-only growth state")
            result["state"] = "volume_extended"
            if volume and volume["source"] == source and volume["layout"] == current and volume["gpt_span"] == span:
                result["verified_phases"]["grow_volume"] = volume
            return result
        require(span == p["target_mib"]*MIB, "grown partition has stale GPT")
        partition = _receipt_proof(p, "grow-partition")
        if current["filesystem_bytes"] == baseline["before"]["filesystem_bytes"]:
            require(locked and marker == p["operation_id"] and size_mib == p["old_mib"], "invalid partition-only growth state")
            result["state"] = "partition_extended"
            if partition and partition["source"] == source and partition["layout"] == current and partition["gpt_span"] == span:
                result["verified_phases"]["grow_partition"] = partition
            return result
        grown_filesystem(p, baseline, current)
        run("e2fsck", "-f", "-n", root)
        filesystem = _receipt_proof(p, "grow-filesystem")
        result["state"] = "filesystem_extended" if locked else "unlocked"
        if locked and size_mib == p["target_mib"] and marker == p["operation_id"] and filesystem \
                and filesystem["source"] == source and filesystem["layout"] == current and filesystem["gpt_span"] == span:
            config(p["vm_path"], filesystem["config"], True)
            result["verified_phases"]["grow_filesystem"] = filesystem
        if not locked:
            require(size_mib == p["target_mib"] and marker == "-", "unlocked growth is incomplete")
            unlocked = _receipt_proof(p, "grow-unlock")
            if unlocked and unlocked["source"] == source and unlocked["layout"] == current and unlocked["gpt_span"] == span:
                config(p["vm_path"], unlocked["config"], False)
                result["verified_phases"]["grow_unlock"] = unlocked
        return result
    except (OSError, RuntimeError, ValueError, KeyError, TypeError) as error:
        result.update(state="ambiguous", verified_phases={}, abort_safe=False, error=str(error)[:1500])
        return result


def grow_abort(p):
    observed = grow_inspect(p)
    require(observed["abort_safe"] and observed["state"] in ("prepared", "untouched"),
            "growth abort requires unchanged original volume size and exact baseline layout")
    require(read_receipt(p, "grow-abort", optional=True) is None, "growth abort already has receipt; inspect instead")
    baseline = read_receipt(p, "grow-baseline")
    require(all(read_receipt(p, "grow-" + phase, optional=True) is None for phase in ("volume", "partition", "filesystem", "unlock")),
            "completed growth phase refuses abort")
    if observed["state"] == "prepared":
        cfg, _ = growth_config(p, baseline, p["old_mib"], True)
        run("zfs", "inherit", "org.pve-dev:grow", p["source_name"])
        require(grow_marker(p["source_name"]) == "-", "growth marker could not be cleared")
        api("set", p["vm_path"] + "/config", digest=cfg["digest"], skiplock=1, delete="lock")
    growth_config(p, baseline, p["old_mib"], False)
    source = growth_source(p, baseline, {p["old_mib"]*MIB})
    require(source == baseline["source"], "growth abort changed original properties")
    run("zpool", "sync", p["pool"])
    proof = {"aborted":True, "source":source, "layout":baseline["before"]}
    proof["receipt"] = write_receipt(p, "grow-abort", proof)
    return proof


def grow_main(p):
    configure_scope(p, operation_key="operation_id", growing=True)
    descriptor = resize_lock(p)
    try:
        return {"prepare":grow_prepare, "volume":grow_volume, "partition":grow_partition,
                "filesystem":grow_filesystem, "unlock":grow_unlock, "inspect":grow_inspect, "abort":grow_abort}[p["phase"]](p)
    finally:
        os.close(descriptor)


if __name__ == "__main__":
    if "configure_scope" not in globals():
        raise RuntimeError("grow adapter requires the compiled-in fixed resize helpers")
    parameters = json.loads(sys.stdin.read())
    parent = os.getppid()
    require(parent == parameters["helper_pid"] and parent > 1, "unexpected helper parent")
    signal.signal(signal.SIGTERM, stop_child)
    signal.signal(signal.SIGINT, stop_child)
    require(ctypes.CDLL(None).prctl(1, signal.SIGTERM, 0, 0, 0) == 0 and os.getppid() == parent, "cannot bind worker lifetime to helper")
    print(json.dumps(grow_main(parameters), sort_keys=True))
