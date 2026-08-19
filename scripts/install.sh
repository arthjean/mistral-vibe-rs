#!/usr/bin/env sh
set -eu

VERSION="${VIBE_VERSION:-2.24.0}"
RELEASE_BASE_URL="${VIBE_RELEASE_BASE_URL:-https://github.com/arthjean/mistral-vibe-rs/releases/download/v${VERSION}}"
INSTALL_DIRECTORY="${VIBE_INSTALL_DIR:-${XDG_BIN_HOME:-${HOME}/.local/bin}}"
COMPLETION_DIRECTORY="${VIBE_COMPLETION_DIR:-${XDG_DATA_HOME:-${HOME}/.local/share}/vibe/completions}"

target_name() {
    system="$(uname -s)"
    architecture="$(uname -m)"
    case "${system}:${architecture}" in
        Linux:x86_64) echo "linux-x86_64" ;;
        Linux:aarch64|Linux:arm64) echo "linux-aarch64" ;;
        Darwin:x86_64) echo "macos-x86_64" ;;
        Darwin:arm64) echo "macos-aarch64" ;;
        *)
            echo "unsupported native platform: ${system}-${architecture}" >&2
            exit 2
            ;;
    esac
}

fetch() {
    source_path="$1"
    destination="$2"
    case "${source_path}" in
        file://*) cp "${source_path#file://}" "${destination}" ;;
        https://*) curl --fail --location --silent --show-error "${source_path}" --output "${destination}" ;;
        *)
            echo "refusing non-HTTPS release source" >&2
            exit 1
            ;;
    esac
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "SHA-256 utility is required" >&2
        exit 1
    fi
}

uninstall() {
    rm -f \
        "${INSTALL_DIRECTORY}/vibe" \
        "${INSTALL_DIRECTORY}/vibe-acp" \
        "${INSTALL_DIRECTORY}/vibe.previous" \
        "${INSTALL_DIRECTORY}/vibe-acp.previous"
    rm -f \
        "${COMPLETION_DIRECTORY}/vibe.bash" \
        "${COMPLETION_DIRECTORY}/_vibe" \
        "${COMPLETION_DIRECTORY}/vibe.fish" \
        "${COMPLETION_DIRECTORY}/vibe.ps1"
    echo "Mistral Vibe RS removed from ${INSTALL_DIRECTORY}"
}

if [ "${1:-}" = "--uninstall" ]; then
    uninstall
    exit 0
fi

command -v curl >/dev/null 2>&1 || {
    echo "curl is required" >&2
    exit 1
}

target="$(target_name)"
case "${target}" in
    macos-*) archive="mistral-vibe-rs-${VERSION}-${target}.zip" ;;
    *) archive="mistral-vibe-rs-${VERSION}-${target}.tar.gz" ;;
esac
temporary_directory="$(mktemp -d)"
transaction_active=false
transaction_file="${temporary_directory}/transaction-paths"
: > "${transaction_file}"

rollback_transaction() {
    while IFS= read -r destination; do
        backup="${destination}.previous"
        staged="${destination}.new"
        if [ -e "${backup}" ]; then
            rm -f "${destination}"
            mv "${backup}" "${destination}" || true
        elif [ -f "${temporary_directory}/new-install-$(basename "${destination}")" ] \
            && [ ! -e "${staged}" ]; then
            rm -f "${destination}"
        fi
        rm -f "${staged}"
    done < "${transaction_file}"
}

cleanup() {
    exit_code=$?
    trap - EXIT HUP INT TERM
    if [ "${transaction_active}" = true ]; then
        rollback_transaction
    fi
    rm -rf "${temporary_directory}"
    exit "${exit_code}"
}

interrupted() {
    exit_code="$1"
    trap - EXIT HUP INT TERM
    if [ "${transaction_active}" = true ]; then
        rollback_transaction
    fi
    rm -rf "${temporary_directory}"
    exit "${exit_code}"
}

trap cleanup EXIT
trap 'interrupted 129' HUP
trap 'interrupted 130' INT
trap 'interrupted 143' TERM

fetch "${RELEASE_BASE_URL}/${archive}" "${temporary_directory}/${archive}"
fetch "${RELEASE_BASE_URL}/SHA256SUMS" "${temporary_directory}/SHA256SUMS"

expected="$(
    awk -v archive="${archive}" '$2 == archive || $2 == "*" archive { print $1 }' \
        "${temporary_directory}/SHA256SUMS"
)"
if [ -z "${expected}" ]; then
    echo "release checksum does not contain ${archive}" >&2
    exit 1
fi
actual="$(sha256_file "${temporary_directory}/${archive}")"
if [ "${actual}" != "${expected}" ]; then
    echo "release checksum mismatch; the installed binary is unchanged" >&2
    exit 1
fi

mkdir -p "${temporary_directory}/extracted"
case "${archive}" in
    *.zip) unzip -q "${temporary_directory}/${archive}" -d "${temporary_directory}/extracted" ;;
    *) tar -xzf "${temporary_directory}/${archive}" -C "${temporary_directory}/extracted" ;;
esac
for executable in vibe vibe-acp; do
    test -x "${temporary_directory}/extracted/bin/${executable}" || {
        echo "release archive is missing ${executable}" >&2
        exit 1
    }
done

mkdir -p "${INSTALL_DIRECTORY}" "${COMPLETION_DIRECTORY}"
for executable in vibe vibe-acp; do
    destination="${INSTALL_DIRECTORY}/${executable}"
    staged="${destination}.new"
    backup="${destination}.previous"
    test ! -e "${backup}" && test ! -e "${staged}" || {
        echo "partial upgrade detected beside ${destination}; restore it before retrying" >&2
        exit 1
    }
    if [ ! -e "${destination}" ]; then
        : > "${temporary_directory}/new-install-${executable}"
    fi
    cp "${temporary_directory}/extracted/bin/${executable}" "${staged}"
    chmod 755 "${staged}"
    printf '%s\n' "${destination}" >> "${transaction_file}"
done
for completion in vibe.bash _vibe vibe.fish vibe.ps1; do
    destination="${COMPLETION_DIRECTORY}/${completion}"
    staged="${destination}.new"
    backup="${destination}.previous"
    test ! -e "${backup}" && test ! -e "${staged}" || {
        echo "partial upgrade detected beside ${destination}; restore it before retrying" >&2
        exit 1
    }
    if [ ! -e "${destination}" ]; then
        : > "${temporary_directory}/new-install-${completion}"
    fi
    cp "${temporary_directory}/extracted/completions/${completion}" "${staged}"
    printf '%s\n' "${destination}" >> "${transaction_file}"
done

transaction_active=true
while IFS= read -r destination; do
    if [ -e "${destination}" ]; then
        if ! mv "${destination}" "${destination}.previous"; then
            echo "upgrade preparation failed; installed files are unchanged" >&2
            exit 1
        fi
    fi
done < "${transaction_file}"
while IFS= read -r destination; do
    if ! mv "${destination}.new" "${destination}"; then
        echo "upgrade activation failed; the previous installation was restored" >&2
        exit 1
    fi
done < "${transaction_file}"
while IFS= read -r destination; do
    rm -f "${destination}.previous"
done < "${transaction_file}"
transaction_active=false

"${INSTALL_DIRECTORY}/vibe" --version
"${INSTALL_DIRECTORY}/vibe-acp" --help >/dev/null
echo "Mistral Vibe RS ${VERSION} installed in ${INSTALL_DIRECTORY}"
