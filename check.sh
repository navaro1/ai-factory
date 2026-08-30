#!/usr/bin/env bash
# Local quality gate for the aif crate at the repository root.
set -euo pipefail
cd "$(dirname "$0")"

printf '== fmt ==\n'
cargo fmt --all --check

printf '== clippy ==\n'
cargo clippy --all-targets -- -D warnings

printf '== test ==\n'
cargo test

printf 'all checks passed\n'
