#!/usr/bin/env bash
set -euo pipefail

target="${1:?target is required}"
asset_directory="$(cd "${2:?asset directory is required}" && pwd)"
archive="$(find "${asset_directory}" -maxdepth 1 -type f \( -name '*.tar.gz' -o -name '*.zip' \) | head -n 1)"
test -n "${archive}" || {
    echo "release archive is missing" >&2
    exit 1
}

temporary_directory="$(mktemp -d)"
cleanup() {
    exit_code=$?
    trap - EXIT
    rm -rf "${temporary_directory}"
    exit "${exit_code}"
}
trap cleanup EXIT

case "${target}" in
    linux-*)
        test -n "${LINUX_SIGNING_KEY:-}" || {
            echo "LINUX_SIGNING_KEY is required" >&2
            exit 1
        }
        printf '%s' "${LINUX_SIGNING_KEY}" > "${temporary_directory}/signing-key.pem"
        openssl dgst -sha256 -sign "${temporary_directory}/signing-key.pem" \
            -out "${archive}.sig" "${archive}"
        openssl pkey -in "${temporary_directory}/signing-key.pem" -pubout \
            -out "${archive}.pub.pem"
        openssl dgst -sha256 -verify "${archive}.pub.pem" \
            -signature "${archive}.sig" "${archive}"
        ;;
    macos-*)
        test -n "${MACOS_CERTIFICATE_P12:-}" || {
            echo "MACOS_CERTIFICATE_P12 is required" >&2
            exit 1
        }
        test -n "${MACOS_CERTIFICATE_PASSWORD:-}" || {
            echo "MACOS_CERTIFICATE_PASSWORD is required" >&2
            exit 1
        }
        test -n "${MACOS_NOTARY_KEY:-}" || {
            echo "MACOS_NOTARY_KEY is required" >&2
            exit 1
        }
        test -n "${MACOS_NOTARY_KEY_ID:-}" || {
            echo "MACOS_NOTARY_KEY_ID is required" >&2
            exit 1
        }
        test -n "${MACOS_NOTARY_ISSUER_ID:-}" || {
            echo "MACOS_NOTARY_ISSUER_ID is required" >&2
            exit 1
        }
        keychain="${temporary_directory}/signing.keychain-db"
        security create-keychain -p temporary "${keychain}"
        security unlock-keychain -p temporary "${keychain}"
        security set-keychain-settings -lut 21600 "${keychain}"
        printf '%s' "${MACOS_CERTIFICATE_P12}" | base64 --decode \
            > "${temporary_directory}/certificate.p12"
        security import "${temporary_directory}/certificate.p12" \
            -k "${keychain}" -P "${MACOS_CERTIFICATE_PASSWORD}" -T /usr/bin/codesign
        security set-key-partition-list -S apple-tool:,apple: -s \
            -k temporary "${keychain}"
        identity="$(security find-identity -v -p codesigning "${keychain}" | awk 'NR == 1 {print $2}')"
        test -n "${identity}" || {
            echo "Developer ID signing identity is unavailable" >&2
            exit 1
        }
        unzip -q "${archive}" -d "${temporary_directory}/archive"
        for executable in vibe vibe-acp; do
            codesign --force --options runtime --timestamp --sign "${identity}" \
                --keychain "${keychain}" "${temporary_directory}/archive/bin/${executable}"
            codesign --verify --strict --verbose=2 \
                "${temporary_directory}/archive/bin/${executable}"
        done
        rm -f "${archive}"
        (cd "${temporary_directory}/archive" && zip -X -q -r "${archive}" .)
        printf '%s' "${MACOS_NOTARY_KEY}" > "${temporary_directory}/notary-key.p8"
        xcrun notarytool submit "${archive}" \
            --key "${temporary_directory}/notary-key.p8" \
            --key-id "${MACOS_NOTARY_KEY_ID}" \
            --issuer "${MACOS_NOTARY_ISSUER_ID}" \
            --wait
        codesign --display --verbose=4 "${temporary_directory}/archive/bin/vibe" \
            > "${archive}.sig" 2>&1
        security delete-keychain "${keychain}"
        ;;
    windows-x86_64)
        test -n "${WINDOWS_CERTIFICATE_PFX:-}" || {
            echo "WINDOWS_CERTIFICATE_PFX is required" >&2
            exit 1
        }
        test -n "${WINDOWS_CERTIFICATE_PASSWORD:-}" || {
            echo "WINDOWS_CERTIFICATE_PASSWORD is required" >&2
            exit 1
        }
        printf '%s' "${WINDOWS_CERTIFICATE_PFX}" | base64 --decode \
            > "${temporary_directory}/certificate.pfx"
        unzip -q "${archive}" -d "${temporary_directory}/archive"
        for executable in vibe.exe vibe-acp.exe; do
            signtool sign /fd SHA256 /td SHA256 \
                /tr http://timestamp.digicert.com \
                /f "${temporary_directory}/certificate.pfx" \
                /p "${WINDOWS_CERTIFICATE_PASSWORD}" \
                "${temporary_directory}/archive/bin/${executable}"
            signtool verify /pa /all "${temporary_directory}/archive/bin/${executable}"
        done
        rm -f "${archive}"
        (cd "${temporary_directory}/archive" && 7z a -tzip "${archive}" . >/dev/null)
        powershell -NoProfile -Command \
            "Get-AuthenticodeSignature -LiteralPath '${temporary_directory}/archive/bin/vibe.exe' | ConvertTo-Json" \
            > "${archive}.sig"
        ;;
    *)
        echo "unsupported signing target ${target}" >&2
        exit 1
        ;;
esac

checksum_file="${asset_directory}/SHA256SUMS.${target}"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "${asset_directory}" && sha256sum "$(basename "${archive}")") > "${checksum_file}"
else
    (cd "${asset_directory}" && shasum -a 256 "$(basename "${archive}")") > "${checksum_file}"
fi
cp "${checksum_file}" "${asset_directory}/SHA256SUMS"
