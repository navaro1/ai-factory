#!/usr/bin/env bash
# ai-factory installer. Copies the workspace files into place.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
zdir="${ZELLIJ_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/zellij}"
bin_dir="${HOME}/.local/bin"

mkdir -p "${zdir}/themes" "${zdir}/layouts" "${zdir}/prompts" "${bin_dir}"

install -m 644 "${here}/zellij/themes/retro-future.kdl" "${zdir}/themes/"
install -m 644 "${here}/zellij/layouts/ai-factory.kdl" "${zdir}/layouts/"
install -m 644 "${here}"/zellij/prompts/*.md "${zdir}/prompts/"
install -m 755 "${here}/bin/ai-factory" "${here}/bin/clauded" "${here}/bin/codexd" "${here}/bin/opencoded" "${bin_dir}/"

if command -v cargo >/dev/null 2>&1; then
    printf 'Building the aif binary...\n'
    (cd "${here}/ui/console" && cargo build --release -q)
    install -m 755 "${here}/ui/console/target/release/aif" "${bin_dir}/"
else
    printf 'Warning: cargo not found; the aif binary was not built.\n' >&2
    printf 'Session start (ai-factory, aif start) will not work without it.\n' >&2
fi

config="${zdir}/config.kdl"
if [[ ! -f "${config}" ]]; then
    printf 'theme "retro-future"\n' >"${config}"
elif grep -q '^[[:space:]]*theme[[:space:]]' "${config}"; then
    if ! grep -q '^[[:space:]]*theme[[:space:]]*"retro-future"' "${config}"; then
        sed -i 's/^\([[:space:]]*theme[[:space:]]*\)"[^"]*"/\1"retro-future"/' "${config}"
    fi
else
    printf '\ntheme "retro-future"\n' >>"${config}"
fi

case ":${PATH}:" in
*":${bin_dir}:"*) ;;
*)
    printf 'Warning: %s is not on your PATH.\n' "${bin_dir}" >&2
    ;;
esac

printf 'Installed.\n'
printf 'Requirements: zellij, claude, opencode, gh, git.\n'
printf 'Start it inside a git repository with: aif start (or ai-factory).\n'
printf 'Run aif --help for the full guide.\n'
