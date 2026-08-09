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
