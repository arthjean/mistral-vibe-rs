#!/usr/bin/env bash
# Prepares a manylinux_2_28 container to build the Linux release.
#
# The reference builds its Linux binaries in the same image so they run on any
# distribution with glibc 2.28 or newer (`.github/workflows/build-and-upload.yml`
# in the reference checkout). The image is AlmaLinux 8: it carries the compiler
# and binutils, and lacks Rust and the ALSA headers `cpal` links against.
set -euo pipefail

dnf install -y --setopt=install_weak_deps=False alsa-lib-devel
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable
if [[ -n "${GITHUB_PATH:-}" ]]; then
    echo "${HOME}/.cargo/bin" >> "${GITHUB_PATH}"
fi
# The checkout belongs to the runner's user, not to the container's root, and
# `git log` refuses a repository it does not own.
if [[ -n "${GITHUB_WORKSPACE:-}" ]]; then
    git config --global --add safe.directory "${GITHUB_WORKSPACE}"
fi
