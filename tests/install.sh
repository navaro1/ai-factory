#!/usr/bin/env bash
# Test the installer without a real build or a user configuration directory.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_root="$(mktemp -d)"
trap 'rm -rf -- "${test_root}"' EXIT

fixture="${test_root}/repo"
fake_bin="${test_root}/fake-bin"
test_home="${test_root}/home"
mkdir -p "${fixture}/docs/v0.5" "${fake_bin}" "${test_home}"
cp "${repo}/install.sh" "${fixture}/install.sh"
cp "${repo}/docs/v0.5/factory.example.toml" "${fixture}/docs/v0.5/"
cp -R "${repo}/docs/v0.5/prompts" "${fixture}/docs/v0.5/prompts"

cat >"${fake_bin}/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
mkdir -p target/release
for binary in aif aifd; do
    printf '#!/usr/bin/env bash\nprintf "%%s test binary\\n" "${0##*/}"\n' >"target/release/${binary}"
    chmod 755 "target/release/${binary}"
done
FAKE_CARGO
chmod 755 "${fake_bin}/cargo"

test_path="${test_home}/.local/bin:${fake_bin}:/usr/bin:/bin"
env -u XDG_CONFIG_HOME HOME="${test_home}" PATH="${test_path}" \
    "${fixture}/install.sh" >/dev/null

for binary in aif aifd; do
    installed="${test_home}/.local/bin/${binary}"
    [[ -x "${installed}" ]] || {
        printf 'missing installed binary: %s\n' "${installed}" >&2
        exit 1
    }
done

config_dir="${test_home}/.config/aif"
for file in factory.example.toml factory.toml prompts/refine.md \
    prompts/implement.md prompts/review.md prompts/release.md; do
    [[ -f "${config_dir}/${file}" ]] || {
        printf 'missing installed config file: %s\n' "${file}" >&2
        exit 1
    }
done

for file in factory.example.toml factory.toml; do
    grep -q '/home/you/Workplace/borsuk' "${config_dir}/${file}" || {
        printf 'the installed config contains a developer-specific repository path: %s\n' \
            "${file}" >&2
        exit 1
    }
done

printf 'keep factory\n' >"${config_dir}/factory.toml"
printf 'keep prompt\n' >"${config_dir}/prompts/refine.md"
env -u XDG_CONFIG_HOME HOME="${test_home}" PATH="${test_path}" \
    "${fixture}/install.sh" >/dev/null
grep -qx 'keep factory' "${config_dir}/factory.toml"
grep -qx 'keep prompt' "${config_dir}/prompts/refine.md"

legacy_dir="${test_home}/.config/ze""llij"
[[ ! -e "${legacy_dir}" ]] || {
    printf 'the installer created a legacy configuration directory\n' >&2
    exit 1
}

printf 'installer test passed\n'
