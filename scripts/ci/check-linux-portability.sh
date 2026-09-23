#!/usr/bin/env bash
# Fails when a Linux release binary needs more than the reference's floor.
#
# The reference ships binaries built on manylinux_2_28, which run on glibc 2.28
# and newer. A binary that references a newer glibc symbol version, or a shared
# library outside the base system plus ALSA, would refuse to start on hosts the
# reference supports, so both are checked on the built binaries.
set -euo pipefail
# `readelf` and `objdump` translate their labels, and the parsing below reads
# the English ones.
export LC_ALL=C

floor="2.28"
allowed_libraries=(
    ld-linux-x86-64.so.2
    ld-linux-aarch64.so.1
    libc.so.6
    libm.so.6
    libdl.so.2
    libpthread.so.0
    librt.so.1
    libutil.so.1
    libgcc_s.so.1
    libasound.so.2
)

# A weak reference is one the binary resolves at run time when it can and does
# without otherwise, which is how the Rust standard library reaches newer
# kernel interfaces, so only the strong references set the floor.
strong_symbols() {
    objdump -T "$1" | grep -Ev '^[0-9a-f]+[[:space:]]+w[[:space:]]'
}

status=0
for binary in "$@"; do
    newest="$(strong_symbols "${binary}" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' \
        | sort -V | tail -n 1)"
    if [[ -n "${newest}" && "$(printf '%s\n%s\n' "${floor}" "${newest}" | sort -V | tail -n 1)" != "${floor}" ]]; then
        echo "${binary} needs glibc ${newest}, above the ${floor} floor:" >&2
        strong_symbols "${binary}" | grep "GLIBC_${newest}" >&2
        status=1
    fi
    while read -r library; do
        allowed=false
        for candidate in "${allowed_libraries[@]}"; do
            [[ "${library}" == "${candidate}" ]] && allowed=true
        done
        if [[ "${allowed}" != "true" ]]; then
            echo "${binary} links ${library}, which a base system does not carry" >&2
            status=1
        fi
    done < <(readelf -d "${binary}" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')
    libraries="$(readelf -d "${binary}" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')"
    if [[ -z "${libraries}" ]]; then
        echo "${binary} lists no shared library, so its dynamic section was not read" >&2
        status=1
    fi
    echo "${binary}: glibc ${newest:-none}; links ${libraries}"
done
exit "${status}"
