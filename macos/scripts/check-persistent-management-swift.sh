#!/usr/bin/env bash
# Same CLT framework/linker flags as scripts/check-swift.sh, with workspace caches
# and SwiftPM manifest sandbox disabled for execution inside an outer sandbox.
set -euo pipefail
unset DEVELOPER_DIR SDKROOT TOOLCHAINS PVE_DEV_TEST_CLI PVE_DEV_TEST_PROFILE
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
cd "$(dirname "$0")/.."
export CLANG_MODULE_CACHE_PATH="$PWD/.build/ModuleCache"
export SWIFTPM_MODULECACHE_OVERRIDE="$CLANG_MODULE_CACHE_PATH"
/usr/bin/swift build --disable-sandbox
/usr/bin/swift build --disable-sandbox -c release
if [[ "$(/usr/bin/xcode-select -p)" == /Library/Developer/CommandLineTools ]]; then
    frameworks=/Library/Developer/CommandLineTools/Library/Developer/Frameworks
    libraries=/Library/Developer/CommandLineTools/Library/Developer/usr/lib
    /usr/bin/swift test --disable-sandbox -Xswiftc -F -Xswiftc "$frameworks" -Xlinker -F -Xlinker "$frameworks" \
        -Xlinker -rpath -Xlinker "$frameworks" -Xlinker -rpath -Xlinker "$libraries"
else
    /usr/bin/swift test --disable-sandbox
fi
