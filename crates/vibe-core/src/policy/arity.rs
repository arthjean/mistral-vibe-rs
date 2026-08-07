//! The command-prefix arity table and the session pattern it derives.
//!
//! Reference `vibe/core/tools/arity.py`. An approval is stored under a pattern
//! rather than under the exact command, and this table is what decides how much
//! of a command that pattern keeps: `git` carries arity 2, so approving
//! `git status` stores `git status *` and a later `git status --short` is
//! covered while `git log` is not.
//!
//! The entries are the ones `crates/vibe-core/tests/permission-surface/vocabulary.json`
//! recorded from the pinned reference, and `permission_parity_tests` fails when
//! either side moves.

/// The command prefixes the reference gives an arity, sorted by prefix.
///
/// The value is a token count, not a depth: `npm run` maps to 3, so a session
/// pattern built from `npm run build` keeps all three tokens.
pub const ARITY: [(&str, usize); 138] = [
    ("aws", 3),
    ("az", 3),
    ("bazel", 2),
    ("brew", 2),
    ("bun", 2),
    ("bun run", 3),
    ("bun x", 3),
    ("cargo", 2),
    ("cargo add", 3),
    ("cargo run", 3),
    ("cat", 1),
    ("cd", 1),
    ("cdk", 2),
    ("cf", 2),
    ("chmod", 1),
    ("chown", 1),
    ("cmake", 2),
    ("composer", 2),
    ("consul", 2),
    ("consul kv", 3),
    ("cp", 1),
    ("crictl", 2),
    ("deno", 2),
    ("deno task", 3),
    ("docker", 2),
    ("docker builder", 3),
    ("docker compose", 3),
    ("docker container", 3),
    ("docker image", 3),
    ("docker network", 3),
    ("docker volume", 3),
    ("doctl", 3),
    ("echo", 1),
    ("eksctl", 2),
    ("eksctl create", 3),
    ("env", 1),
    ("export", 1),
    ("firebase", 2),
    ("flyctl", 2),
    ("gcloud", 3),
    ("gh", 3),
    ("git", 2),
    ("git config", 3),
    ("git remote", 3),
    ("git stash", 3),
    ("go", 2),
    ("gradle", 2),
    ("grep", 1),
    ("helm", 2),
    ("heroku", 2),
    ("hugo", 2),
    ("ip", 2),
    ("ip addr", 3),
    ("ip link", 3),
    ("ip netns", 3),
    ("ip route", 3),
    ("kill", 1),
    ("killall", 1),
    ("kind", 2),
    ("kind create", 3),
    ("kubectl", 2),
    ("kubectl kustomize", 3),
    ("kubectl rollout", 3),
    ("kustomize", 2),
    ("ln", 1),
    ("ls", 1),
    ("make", 2),
    ("mc", 2),
    ("mc admin", 3),
    ("minikube", 2),
    ("mkdir", 1),
    ("mongosh", 2),
    ("mv", 1),
    ("mvn", 2),
    ("mysql", 2),
    ("ng", 2),
    ("npm", 2),
    ("npm exec", 3),
    ("npm init", 3),
    ("npm run", 3),
    ("npm view", 3),
    ("nvm", 2),
    ("nx", 2),
    ("openssl", 2),
    ("openssl req", 3),
    ("openssl x509", 3),
    ("pip", 2),
    ("pipenv", 2),
    ("pnpm", 2),
    ("pnpm dlx", 3),
    ("pnpm exec", 3),
    ("pnpm run", 3),
    ("podman", 2),
    ("podman container", 3),
    ("podman image", 3),
    ("poetry", 2),
    ("ps", 1),
    ("psql", 2),
    ("pulumi", 2),
    ("pulumi stack", 3),
    ("pwd", 1),
    ("pyenv", 2),
    ("python", 2),
    ("rake", 2),
    ("rbenv", 2),
    ("redis-cli", 2),
    ("rm", 1),
    ("rmdir", 1),
    ("rustup", 2),
    ("serverless", 2),
    ("sfdx", 3),
    ("skaffold", 2),
    ("sleep", 1),
    ("sls", 2),
    ("source", 1),
    ("sst", 2),
    ("swift", 2),
    ("systemctl", 2),
    ("tail", 1),
    ("terraform", 2),
    ("terraform workspace", 3),
    ("tmux", 2),
    ("touch", 1),
    ("turbo", 2),
    ("ufw", 2),
    ("unset", 1),
    ("uv", 2),
    ("uv run", 3),
    ("vault", 2),
    ("vault auth", 3),
    ("vault kv", 3),
    ("vercel", 2),
    ("volta", 2),
    ("which", 1),
    ("wp", 2),
    ("yarn", 2),
    ("yarn dlx", 3),
    ("yarn run", 3),
];

/// The session pattern an approval for `tokens` is stored under.
///
/// Reference `build_session_pattern`: the longest prefix of `tokens` present in
/// [`ARITY`] selects an arity, and the pattern is that many leading tokens
/// followed by ` *`. A first token the table does not know falls back to that
/// token alone, and an empty command has no pattern rather than a panic.
#[must_use]
pub fn build_session_pattern(tokens: &[&str]) -> String {
    let Some(first) = tokens.first() else {
        return String::new();
    };
    for length in (1..=tokens.len()).rev() {
        let prefix = tokens[..length].join(" ");
        if let Some(arity) = arity_of(&prefix) {
            // The reference slices with `tokens[:arity]`, which stops at the
            // end of a command shorter than its own arity rather than padding.
            let kept = arity.min(tokens.len());
            return format!("{} *", tokens[..kept].join(" "));
        }
    }
    format!("{first} *")
}

/// The arity the table gives `prefix`, or [`None`] when it declares none.
#[must_use]
pub fn arity_of(prefix: &str) -> Option<usize> {
    ARITY
        .binary_search_by(|(candidate, _)| (*candidate).cmp(prefix))
        .ok()
        .map(|index| ARITY[index].1)
}

#[cfg(test)]
mod arity_tests;
