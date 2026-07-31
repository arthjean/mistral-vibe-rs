#!/usr/bin/env bash
set -euo pipefail

release_directory="${1:?release directory is required}"
version="${2:?version is required}"
temporary_directory="$(mktemp -d)"
cleanup() {
    exit_code=$?
    trap - EXIT
    rm -rf "${temporary_directory}"
    exit "${exit_code}"
}
trap cleanup EXIT

VIBE_VERSION="${version}" \
VIBE_RELEASE_BASE_URL="file://${release_directory}" \
VIBE_INSTALL_DIR="${temporary_directory}/bin" \
VIBE_COMPLETION_DIR="${temporary_directory}/completions" \
    sh scripts/install.sh

"${temporary_directory}/bin/vibe" --version
"${temporary_directory}/bin/vibe-acp" --help >/dev/null
test -f "${temporary_directory}/completions/vibe.bash"

VIBE_INSTALL_DIR="${temporary_directory}/bin" \
VIBE_COMPLETION_DIR="${temporary_directory}/completions" \
    sh scripts/install.sh --uninstall
test ! -e "${temporary_directory}/bin/vibe"
test ! -e "${temporary_directory}/bin/vibe-acp"
