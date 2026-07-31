# Native platform certification

The required 1.0 targets are:

| Target | Native evidence required |
|---|---|
| Linux x86_64 | CLI, TUI, ACP, POSIX shell, filesystem, managed terminal, signals, keyring failure, proxy/TLS, persistence, installer and uninstall |
| Linux aarch64 | Same Linux suite on an aarch64 host |
| macOS x86_64 | CLI, TUI, ACP, shell, PTY, Keychain, filesystem, proxy/TLS, persistence, encodings, Homebrew paths, login shells, notifications, signing, notarization and Gatekeeper |
| macOS aarch64 | Same macOS suite on Apple Silicon |
| Windows x86_64 | CLI, TUI, ACP, cmd, PowerShell, Git Bash, ConPTY, credential store, path matrix, proxy/TLS, filesystem, persistence, process trees, locked files, signals and resize |

Cross-compilation, emulation, or Linux path simulation cannot certify another
target. Each evidence manifest records the native host OS and architecture,
artifact digest, source revision, suite verdicts, minimum runtime, signing
state, clean-host download result, and 10,000-trial cleanup measurements.

The declared minimum runtime is the exact native runner generation exercised
by certification. Older macOS or Windows versions remain unsupported until a
matching native runner produces evidence. Release certification requires all
five manifests from one immutable source revision. A missing target or failed
suite remains visible as a blocker.
