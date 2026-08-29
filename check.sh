#!/usr/bin/env bash
# Local quality gate for the ai-factory repo.
set -euo pipefail
cd "$(dirname "$0")"

printf '== fmt ==\n'
cargo fmt --all --check --manifest-path ui/console/Cargo.toml

printf '== clippy ==\n'
cargo clippy --manifest-path ui/console/Cargo.toml --all-targets -- -D warnings

printf '== test ==\n'
cargo test --manifest-path ui/console/Cargo.toml

printf '== tokens drift ==\n'
cargo run -q --manifest-path ui/console/Cargo.toml -- tokens zellij --check

printf 'all checks passed\n'
