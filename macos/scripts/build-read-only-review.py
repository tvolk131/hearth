#!/usr/bin/env python3
"""Build an isolated local review app seeded with inspected read-only hosts.

No profile/private-key contents enter Swift. This script asks Rust for public
pins, stores only app settings in the review bundle, and does not launch it.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory-executable", required=True, type=Path)
    parser.add_argument("--host", action="append", required=True, help="Display name=/absolute/profile.json")
    parser.add_argument("--app-output", type=Path)
    args = parser.parse_args()
    package = Path(__file__).resolve().parents[1]
    inventory = args.inventory_executable.resolve(strict=True)
    output = (args.app_output or package / ".build/PveDevApp-ReadOnly.app").absolute()
    if output.exists():
        parser.error("review app already exists; choose a new --app-output or explicitly remove the old review wrapper")
    output.parent.mkdir(parents=True, exist_ok=True)
    servers = []
    for host in args.host:
        name, separator, raw_path = host.partition("=")
        if not separator or not name.strip() or not Path(raw_path).is_absolute():
            parser.error("--host requires a name and absolute JSON profile path")
        profile = Path(raw_path).resolve(strict=True)
        result = subprocess.run([str(inventory), "observe-profile-info", "--profile", str(profile)],
                                check=True, capture_output=True, timeout=20)
        identity = json.loads(result.stdout)
        if len(identity.get("binding", "")) != 64 or len(identity.get("host_id", "")) != 64:
            raise ValueError("inventory CLI returned invalid public identity")
        servers.append({"id": str(uuid.uuid4()).upper(), "name": name.strip(), "profilePath": str(profile),
                        "mode": "observation", "observationIdentity": identity})
    if not 1 <= len(servers) <= 32 or len({s["observationIdentity"]["binding"] for s in servers}) != len(servers):
        parser.error("choose 1–32 distinct read-only hosts")
    settings = {"executablePath": str(inventory.with_name("pve-devctl")), "inventoryExecutablePath": str(inventory),
                "servers": servers, "selectedServerId": servers[0]["id"], "timeoutSeconds": 90,
                "pollingEnabled": False, "pollingIntervalSeconds": 30}
    source = Path(tempfile.mkdtemp(prefix="readonly-review-source-", dir=output.parent))
    state = output.parent / ".read-only-review-state"
    state.mkdir(mode=0o700, exist_ok=True)
    os.chmod(state, 0o700)
    shutil.copy2(package / "Package.swift", source / "Package.swift")
    shutil.copytree(package / "Sources", source / "Sources")
    shutil.copytree(package / "Tests", source / "Tests")
    entry = source / "Sources/PveDevApp/PveDevApplication.swift"
    text = entry.read_text()
    old = "@State private var model = AppModel()"
    if text.count(old) != 1:
        raise ValueError("review entry-point injection no longer matches production source")
    text = text.replace(old, "@State private var model = makeReadOnlyReviewModel()")
    ledger_literal = json.dumps(str(state / "pending.json"))
    text += f'''

@MainActor
private func makeReadOnlyReviewModel() -> AppModel {{
    let store = SettingsStore(suiteName: "dev.pve.readonlyreview")
    if store.load().servers.isEmpty,
       let url = Bundle.main.url(forResource: "review-settings", withExtension: "json"),
       let data = try? Data(contentsOf: url),
       let settings = try? JSONDecoder().decode(AppSettings.self, from: data),
       !settings.servers.isEmpty,
       settings.servers.allSatisfy({{ $0.isReadOnly && $0.observationIdentity?.isValid == true && $0.identity == nil }}),
       settings.serverListIssues.isEmpty {{
        store.save(settings)
    }}
    let model = AppModel(settingsStore: store,
        ledgerStore: LedgerStore(fileURL: URL(fileURLWithPath: {ledger_literal})))
    model.refresh()
    return model
}}
'''
    entry.write_text(text)
    hashes = {}
    for original in sorted((package / "Sources").rglob("*.swift")):
        relative = original.relative_to(package)
        copied = source / relative
        matches = original.read_bytes() == copied.read_bytes()
        if original.name != "PveDevApplication.swift" and not matches:
            raise ValueError(f"unexpected review source difference: {relative}")
        hashes[str(relative)] = {"production_sha256": hashlib.sha256(original.read_bytes()).hexdigest(),
                                 "review_sha256": hashlib.sha256(copied.read_bytes()).hexdigest(), "matches": matches}
    environment = {k: v for k, v in os.environ.items() if k not in ["DEVELOPER_DIR", "SDKROOT", "TOOLCHAINS"]}
    environment.update({"PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
                        "CLANG_MODULE_CACHE_PATH": str(source / "clang-cache"),
                        "SWIFTPM_MODULECACHE_OVERRIDE": str(source / "swift-cache")})
    log = output.parent / "read-only-review-build.log"
    with log.open("w") as stream:
        subprocess.run(["/usr/bin/swift", "build", "--package-path", str(source), "--disable-sandbox", "-c", "release"],
                       check=True, stdout=stream, stderr=subprocess.STDOUT, env=environment, timeout=300)
    binary = source / ".build/release/PveDevApp"
    executable = output / "Contents/MacOS/PveDevApp"
    executable.parent.mkdir(parents=True)
    resources = output / "Contents/Resources"
    resources.mkdir()
    shutil.copy2(binary, executable)
    with (output / "Contents/Info.plist").open("wb") as stream:
        plistlib.dump({"CFBundleExecutable": "PveDevApp", "CFBundleIdentifier": "dev.pve.readonlyreview",
                      "CFBundleName": "PVE Read-only Review", "CFBundleDisplayName": "PVE Read-only Review",
                      "CFBundlePackageType": "APPL", "CFBundleVersion": "3", "CFBundleShortVersionString": "0.3",
                      "NSHighResolutionCapable": True, "LSMinimumSystemVersion": "14.0"}, stream)
    (resources / "review-settings.json").write_text(json.dumps(settings, indent=2) + "\n")
    (resources / "source-comparison.json").write_text(json.dumps(hashes, indent=2) + "\n")
    subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(output)], check=True, capture_output=True)
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(output)], check=True, capture_output=True)
    print(json.dumps({"app": str(output), "preferences_suite": "dev.pve.readonlyreview",
                      "ledger": str(state / "pending.json"), "source_copy": str(source),
                      "build_log": str(log), "hosts": [s["name"] for s in servers],
                      "launched": False, "production_preferences_changed": False}, indent=2))


if __name__ == "__main__":
    main()
