#!/usr/bin/env bash
set -euo pipefail

declared_target="${1:?native target is required}"
case "$(uname -s):$(uname -m)" in
    Linux:x86_64) actual_target="linux-x86_64" ;;
    Linux:aarch64|Linux:arm64) actual_target="linux-aarch64" ;;
    Darwin:x86_64) actual_target="macos-x86_64" ;;
    Darwin:arm64) actual_target="macos-aarch64" ;;
    MINGW*:*|MSYS*:*|CYGWIN*:* ) actual_target="windows-x86_64" ;;
    *) echo "unsupported native host: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
test "${actual_target}" = "${declared_target}" || {
    echo "native certification target ${declared_target} does not match ${actual_target}" >&2
    exit 1
}

cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release --locked

binary_suffix=""
if [[ "${actual_target}" == windows-* ]]; then
    binary_suffix=".exe"
fi
vibe_binary="target/release/vibe${binary_suffix}"
acp_binary="target/release/vibe-acp${binary_suffix}"
"${vibe_binary}" --version
"${vibe_binary}" --help >/dev/null
"${vibe_binary}" --fake-response native-smoke --prompt smoke >/dev/null
"${acp_binary}" --help >/dev/null

if [[ "${actual_target}" != windows-* ]]; then
    cargo test -p vibe-cli --test tui_pty
    cargo test -p vibe-core process::tests::cleanup_signals_all_processes_and_leaves_no_handles
    cargo test -p vibe-core process::tests::release_reaps_descendants_after_process_group_leader_exits
fi
cargo test -p vibe-cli tui::setup::tests::unavailable_keyring_has_a_non_secret_recovery_path
cargo test -p vibe-core bootstrap::tests::missing_credentials_and_invalid_transport_settings_are_typed
cargo test -p vibe-core storage::tests::path_traversal_session_ids_are_rejected

echo "native smoke passed for ${actual_target}"
