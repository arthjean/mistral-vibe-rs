#!/usr/bin/env bash
set -euo pipefail

target="${1:?target is required}"
version="${2:?version is required}"
output_directory="${3:-dist/release-assets}"
binary_suffix=""
archive_suffix="tar.gz"
if [[ "${target}" == windows-* || "${target}" == macos-* ]]; then
    archive_suffix="zip"
fi
if [[ "${target}" == windows-* ]]; then
    binary_suffix=".exe"
fi

if [[ "${VIBE_SKIP_BUILD:-false}" != "true" ]]; then
    cargo build --workspace --release --locked
fi

staging_directory="$(mktemp -d)"
cleanup() {
    exit_code=$?
    trap - EXIT
    rm -rf "${staging_directory}"
    exit "${exit_code}"
}
trap cleanup EXIT
mkdir -p "${staging_directory}/bin" "${staging_directory}/completions" "${output_directory}"
cp "target/release/vibe${binary_suffix}" "${staging_directory}/bin/"
cp "target/release/vibe-acp${binary_suffix}" "${staging_directory}/bin/"
cp completions/vibe.bash completions/_vibe completions/vibe.fish completions/vibe.ps1 \
    "${staging_directory}/completions/"
cp LICENSE NOTICE "${staging_directory}/"

archive="mistral-vibe-rs-${version}-${target}.${archive_suffix}"
if [[ "${archive_suffix}" == "zip" ]]; then
    archive_path="$(cd "${output_directory}" && pwd)/${archive}"
    if command -v zip >/dev/null 2>&1; then
        (cd "${staging_directory}" && zip -X -q -r "${archive_path}" .)
    else
        (cd "${staging_directory}" && 7z a -tzip "${archive_path}" . >/dev/null)
    fi
else
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
        -czf "${output_directory}/${archive}" -C "${staging_directory}" .
fi

# The checksum tool runs from inside the output directory so every line records
# a bare archive name, which is what `sha256sum -c` resolves relative to the
# directory it verifies.
checksum_file="${output_directory}/SHA256SUMS.${target}"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "${output_directory}" && sha256sum "${archive}") > "${checksum_file}"
else
    (cd "${output_directory}" && shasum -a 256 "${archive}") > "${checksum_file}"
fi

# `SHA256SUMS` aggregates every archive the directory holds. A matrix leg that
# overwrote it would leave the published manifest naming one target, so this run
# drops any line already claiming its own archive, appends its own, and sorts by
# archive name. The result is stable whichever order the targets are packaged in.
aggregate_file="${output_directory}/SHA256SUMS"
merged_file="${output_directory}/SHA256SUMS.merged"
: > "${merged_file}"
if [[ -f "${aggregate_file}" ]]; then
    awk -v archive="${archive}" \
        '$2 != archive && $2 != "*" archive' "${aggregate_file}" > "${merged_file}"
fi
cat "${checksum_file}" >> "${merged_file}"
LC_ALL=C sort -k2,2 "${merged_file}" > "${aggregate_file}"
rm -f "${merged_file}"
