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

# Write a portable copy of the committed example.
write_config() {
    local destination="$1"
    local temporary
    temporary="$(mktemp "${config_dir}/.factory.XXXXXX")"
    if ! sed 's|^path = "/home/[^/]*/Workplace/|path = "/home/you/Workplace/|' \
        "${here}/docs/v0.5/factory.example.toml" >"${temporary}"; then
        rm -f -- "${temporary}"
        return 1
    fi
    if ! install -m 644 "${temporary}" "${destination}"; then
        rm -f -- "${temporary}"
        return 1
    fi
    rm -f -- "${temporary}"
}

# The live config starts as a copy of the example. A file that exists stays.
if [[ ! -f "${config_dir}/factory.example.toml" ]]; then
    write_config "${config_dir}/factory.example.toml"
    printf 'Wrote %s\n' "${config_dir}/factory.example.toml"
fi
if [[ ! -f "${config_dir}/factory.toml" ]]; then
    write_config "${config_dir}/factory.toml"
    printf 'Wrote %s\n' "${config_dir}/factory.toml"
fi

# The default prompts. The daemon falls back to built-in prompts when these
# files are absent, so they exist only to give you a starting point to edit.
for prompt in refine implement review release ticket-chat; do
    if [[ ! -f "${config_dir}/prompts/${prompt}.md" ]]; then
        install -m 644 "${here}/docs/v0.5/prompts/${prompt}.md" "${config_dir}/prompts/${prompt}.md"
        printf 'Wrote %s\n' "${config_dir}/prompts/${prompt}.md"
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
