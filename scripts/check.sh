#!/usr/bin/env bash
# Fast local and CI checks: formatting, lints, Rust tests, Python unit tests,
# syntax checks for every Python and shell file, and workflow lint.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest discover -s lab -p 'test_*.py' -v
git ls-files -z --cached --others --exclude-standard -- '*.py' | xargs -0 python3 -m py_compile
git ls-files -z --cached --others --exclude-standard -- '*.sh' | xargs -0 -n1 bash -n
actionlint -shellcheck='' .github/workflows/*.yml
