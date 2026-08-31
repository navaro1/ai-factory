#!/usr/bin/env bash
# ai-factory installer. Builds the crate and installs the binaries and the
# default configuration. Existing configuration files are never overwritten.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
config_dir="${XDG_CONFIG_HOME:-${HOME}/.config}/aif"
bin_dir="${HOME}/.local/bin"

if ! command -v cargo >/dev/null 2>&1; then
    printf 'Error: cargo is missing. Install Rust first, then run this script again.\n' >&2
    exit 1
fi

printf 'Building aif and aifd...\n'
(cd "${here}" && cargo build --release -q)

mkdir -p "${bin_dir}"
install -m 755 "${here}/target/release/aif" "${bin_dir}/aif"
install -m 755 "${here}/target/release/aifd" "${bin_dir}/aifd"

mkdir -p "${config_dir}/prompts"

# The example config is a reference copy. The live config starts as a copy of
# it, so a new user has a working starting point. A file that exists stays.
if [[ ! -f "${config_dir}/factory.example.toml" ]]; then
    install -m 644 "${here}/docs/v0.5/factory.example.toml" "${config_dir}/factory.example.toml"
    printf 'Wrote %s\n' "${config_dir}/factory.example.toml"
fi
if [[ ! -f "${config_dir}/factory.toml" ]]; then
    install -m 644 "${here}/docs/v0.5/factory.example.toml" "${config_dir}/factory.toml"
    printf 'Wrote %s\n' "${config_dir}/factory.toml"
fi

# The default prompts. The daemon falls back to built-in prompts when these
# files are absent, so they exist only to give you a starting point to edit.
for stage in refine implement review release; do
    if [[ ! -f "${config_dir}/prompts/${stage}.md" ]]; then
        install -m 644 "${here}/docs/v0.5/prompts/${stage}.md" "${config_dir}/prompts/${stage}.md"
        printf 'Wrote %s\n' "${config_dir}/prompts/${stage}.md"
    fi
done

case ":${PATH}:" in
*":${bin_dir}:"*) ;;
*)
    printf 'Warning: %s is not on your PATH.\n' "${bin_dir}" >&2
    ;;
esac

printf 'Installed.\n'
printf 'Edit %s and set the path of every repository.\n' "${config_dir}/factory.toml"
printf 'Run aif doctor to check the installation.\n'
printf 'Run aif to start the daemon and open the terminal UI.\n'
