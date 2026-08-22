use super::builtins::builtin_skills;

/// US-170: the documentation link in the `vibe` body is pinned to the version
/// that is actually running and to the repository the manifest declares, so no
/// placeholder ships, the URL never advertises a release this binary is not,
/// and the published location cannot drift from `[workspace.package]`.
#[test]
fn the_vibe_body_names_the_running_version() {
    let skills = builtin_skills();
    let vibe = &skills["vibe"];
    assert!(
        vibe.body.contains(&format!(
            "{}/blob/v{}/",
            env!("CARGO_PKG_REPOSITORY"),
            env!("CARGO_PKG_VERSION")
        )),
        "the documentation URL carries the declared repository and the workspace version"
    );
    for placeholder in ["__VERSION__", "__REPOSITORY__"] {
        assert!(
            !vibe.body.contains(placeholder),
            "{placeholder} is fully substituted"
        );
    }
}

/// US-284: an agent asked about `--worktree` answers from the skill body, so
/// the body has to carry what the flag actually does: where the worktree lives,
/// the branch it checks out, that an existing one is reused, the trust it
/// grants, and that only a worktree this run created is offered for cleanup.
#[test]
fn the_vibe_body_documents_the_worktree_flag() {
    let skills = builtin_skills();
    let vibe = &skills["vibe"];
    for stated in [
        "`--worktree <name>`",
        "managed root",
        "reuses it when one is already there",
        "trusts that directory without asking",
        "only a worktree it created itself",
    ] {
        assert!(
            vibe.body.contains(stated),
            "the vibe body does not state {stated:?}"
        );
    }
}
