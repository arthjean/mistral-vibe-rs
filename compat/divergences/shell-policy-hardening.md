# Shell policy hardening

Mistral Vibe 2.23.1 treats several command prefixes as read-only without
validating every option that changes the command's effective authority. For
example, `git diff --no-index /etc/passwd /dev/null` can read outside the
workspace, while `rg --pre ...` can execute an arbitrary preprocessor.

The Rust implementation intentionally returns `ASK` for these option forms and
for Git configuration or executable overrides. It returns `NEVER` for an
explicitly destructive command set. Workspace-local readers such as `cat
README.md`, plain `git status`, and ordinary ripgrep searches retain automatic
read behavior.

This is a bounded security correction, not a general shell-output divergence.
The semantic fixture records both the upstream and corrected verdicts.
