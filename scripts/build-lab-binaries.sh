#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export RUSTC
RUSTC="$(rustup which --toolchain 1.99.1 rustc)"
cargo zigbuild --locked --release --target x86_64-unknown-linux-musl --bins
printf 'Portable Linux binaries: %s/target/x86_64-unknown-linux-musl/release\n' "$PWD"
