#!/usr/bin/env bash
set -euo pipefail
unset DEVELOPER_DIR SDKROOT TOOLCHAINS
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
cd "$(dirname "$0")/../macos"
/usr/bin/swift build
/usr/bin/swift build -c release
if [[ "$(/usr/bin/xcode-select -p)" == /Library/Developer/CommandLineTools ]]; then
    frameworks=/Library/Developer/CommandLineTools/Library/Developer/Frameworks
    libraries=/Library/Developer/CommandLineTools/Library/Developer/usr/lib
    /usr/bin/swift test -Xswiftc -F -Xswiftc "$frameworks" -Xlinker -F -Xlinker "$frameworks" \
        -Xlinker -rpath -Xlinker "$frameworks" -Xlinker -rpath -Xlinker "$libraries"
else
    /usr/bin/swift test
fi
