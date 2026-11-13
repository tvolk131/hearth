#!/usr/bin/env bash
# Build a NEW isolated app. All inputs are local; this script never deploys or launches.
set -euo pipefail
unset DEVELOPER_DIR SDKROOT TOOLCHAINS
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
if [[ $# -ne 3 && $# -ne 4 ]]; then
    echo "Usage: $0 /absolute/pve-devctl /absolute/pve-dev-inventory /absolute/two-controller-settings.json [/absolute/new-output.app]" >&2
    exit 64
fi
project="$(cd "$(dirname "$0")/../.." && pwd)"
app="${4:-$project/.lab/persistent-management-20261112/PveDevApp.app}"
[[ "$app" == /* && "$app" == *.app && -d "$(dirname "$app")" ]] || { echo "Existing absolute parent and new .app output required" >&2; exit 65; }
[[ ! -e "$app" && ! -L "$app" ]] || { echo "Refusing to overwrite existing app: $app" >&2; exit 73; }
for binary in "$1" "$2"; do
    [[ "$binary" == /* && -f "$binary" && -x "$binary" ]] || { echo "Portable CLI must be an absolute executable file: $binary" >&2; exit 65; }
    /usr/bin/file "$binary" | /usr/bin/grep -q 'Mach-O' || { echo "CLI must be a macOS Mach-O executable: $binary" >&2; exit 65; }
    if /usr/bin/otool -L "$binary" | /usr/bin/tail -n +2 | /usr/bin/awk '{print $1}' | /usr/bin/grep -Ev '^(/usr/lib/|/System/Library/)' >/dev/null; then
        echo "CLI has non-system dynamic library dependencies; supply a portable build: $binary" >&2
        exit 65
    fi
done
[[ "$3" == /* && -f "$3" ]] || { echo "Settings input must be an absolute JSON file" >&2; exit 65; }
# Validate before creating the final destination. Never read/copy device private keys.
/usr/bin/python3 - "$3" <<'PY'
import json, pathlib, re, sys, uuid
s=json.loads(pathlib.Path(sys.argv[1]).read_text())
servers=s.get('servers', [])
assert len(servers)==2, 'Exactly two controller profiles are required'
assert len({str(uuid.UUID(x['id'])) for x in servers})==2, 'Server IDs must be valid and unique'
assert s.get('selectedServerId') in (None, *[x['id'] for x in servers]), 'Selected server must be configured'
pins=[]
for server in servers:
    assert isinstance(server.get('name'), str) and server['name'].strip(), 'Each server needs a name'
    assert server.get('mode', 'controller')=='controller', 'Controller profiles only'
    assert server.get('observationIdentity') is None, 'Observation identities cannot authorize management'
    p=pathlib.Path(server['profilePath'])
    assert p.is_absolute() and p.suffix=='.json' and p.is_file(), 'Persistent profile paths must exist'
    pin=server['identity']
    for key,n in [('server_id',64),('environment',32),('device_id',64)]:
        assert re.fullmatch('[0-9a-f]{%d}'%n,pin[key]), 'Invalid pinned public identity'
    pins.append((pin['server_id'], pin['environment'], pin['device_id']))
assert len(set(pins))==2, 'Controller identities must be distinct'
PY
cd "$project/macos"
export CLANG_MODULE_CACHE_PATH="$project/macos/.build/ModuleCache"
export SWIFTPM_MODULECACHE_OVERRIDE="$CLANG_MODULE_CACHE_PATH"
/usr/bin/swift build --disable-sandbox -c release
build_dir="$(/usr/bin/swift build --disable-sandbox -c release --show-bin-path)"
# Construct next to the target and rename only after every packaging step succeeds.
staging="$(/usr/bin/mktemp -d "$project/.lab/persistent-management-20261112/.app-package.XXXXXX")"
trap '/bin/rm -rf "$staging"' EXIT
bundle="$staging/PveDevApp.app"
/bin/mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources/bin"
/bin/cp "$build_dir/PveDevApp" "$bundle/Contents/MacOS/PveDevApp"
/bin/cp "$1" "$bundle/Contents/Resources/bin/pve-devctl"
/bin/cp "$2" "$bundle/Contents/Resources/bin/pve-dev-inventory"
/usr/bin/python3 - "$3" "$bundle" "$app" <<'PY'
import json, pathlib, plistlib, sys
s=json.loads(pathlib.Path(sys.argv[1]).read_text())
bundle=pathlib.Path(sys.argv[2]); final=pathlib.Path(sys.argv[3])
s['executablePath']=str(final/'Contents/Resources/bin/pve-devctl')
s['inventoryExecutablePath']=str(final/'Contents/Resources/bin/pve-dev-inventory')
s['pollingEnabled']=False
for server in s['servers']: server['expectedBackend']='managed'
if not s.get('selectedServerId'): s['selectedServerId']=s['servers'][0]['id']
(bundle/'Contents/Resources/bootstrap-settings.json').write_text(json.dumps(s, indent=2)+'\n')
info={'CFBundleExecutable':'PveDevApp','CFBundleIdentifier':'dev.pve.persistent-management-20261112',
      'CFBundleName':'PVE Dev Management','CFBundleDisplayName':'PVE Dev Management','CFBundlePackageType':'APPL',
      'CFBundleShortVersionString':'1.0','CFBundleVersion':'20261112','LSMinimumSystemVersion':'14.0',
      'NSHighResolutionCapable':True,'PveDevSettingsSuite':'dev.pve.persistent-management-20261112',
      'PveDevLedgerNamespace':'PveDevApp-persistent-management-20261112'}
with (bundle/'Contents/Info.plist').open('wb') as f: plistlib.dump(info,f)
PY
/usr/bin/codesign --force --sign - "$bundle/Contents/Resources/bin/pve-devctl"
/usr/bin/codesign --force --sign - "$bundle/Contents/Resources/bin/pve-dev-inventory"
/usr/bin/codesign --force --sign - "$bundle"
/usr/bin/codesign --verify --deep --strict "$bundle"
[[ ! -e "$app" && ! -L "$app" ]] || { echo "Destination appeared; refusing overwrite" >&2; exit 73; }
/bin/mv -n "$bundle" "$app"
echo "$app"
