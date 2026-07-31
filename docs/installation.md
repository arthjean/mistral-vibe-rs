# Installation and updates

Release archives contain `vibe`, `vibe-acp`, shell completions, `LICENSE`,
`NOTICE`, and provenance policy. Do not install an archive unless its target,
version, checksum, signature, attestation, and source revision agree with the
release manifest.

On Linux or macOS:

```console
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/arthurjean/mistral-vibe-rs/main/scripts/install.sh |
  sh
```

On Windows PowerShell, download `scripts/install.ps1`, inspect it, then run it
from a trusted local path. Both installers stage and verify the complete
archive before replacing an installed binary. A checksum failure, unsupported
target, read-only destination, interrupted upgrade, or partial prior upgrade
leaves the working binary in place or preserves an adjacent `.previous` file
with explicit recovery guidance.

Uninstall with `scripts/install.sh --uninstall` or
`scripts/install.ps1 -Uninstall`. Completions are installed beside the native
binary under the configured completion directory.

The updater distinguishes current, available, offline, unsupported, and
partial-upgrade states. Offline or unsupported checks never remove or replace
the installed version.

No 1.0 archive is approved until the linked Release 5 certification report is
ready. Building a local archive is development evidence, not release
certification.
