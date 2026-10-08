use super::*;
use crate::workspace::WorkspacePaths;

/// Each input against what reference `sanitize_api_base`
/// (`vibe/app_server/_provider_auth.py` at 376f6a3) answered for it, measured
/// with the pinned checkout's interpreter.
#[test]
fn an_api_base_is_shown_as_the_reference_shows_it() {
    let cases: &[(&str, &[&str], Option<&str>)] = &[
        (
            "https://api.mistral.ai/v1",
            &[],
            Some("https://api.mistral.ai/v1"),
        ),
        (
            "HTTPS://User:Pass@API.Example.COM:0443/v1/?k=1#f",
            &[],
            Some("https://api.example.com:443/v1/"),
        ),
        ("http://[::1]:8080/x", &[], Some("http://[::1]:8080/x")),
        ("http://[::1/x", &[], None),
        ("ftp://h/x", &[], None),
        ("https:///p", &[], None),
        ("https://h:99999/p", &[], None),
        ("https://h:abc/p", &[], None),
        (
            "https://h.example/sk-SECRET/v1",
            &["sk-secret"],
            Some("https://h.example/[redacted]/v1"),
        ),
        (
            "https://h.example/%73k-secret/v1",
            &["sk-secret"],
            Some("https://h.example/[redacted]/v1"),
        ),
        (
            "https://h.example/a%20b+c/x",
            &["a b"],
            Some("https://h.example/[redacted]+c/x"),
        ),
        (
            "https://h.example/a+b",
            &["a b"],
            Some("https://h.example/[redacted]"),
        ),
        ("https://h.example/p\u{85}q", &[], None),
        (
            "  https://h.example/p\tq",
            &[],
            Some("https://h.example/pq"),
        ),
        (
            "https://h.example/abcabc",
            &["abc", "abcabc"],
            Some("https://h.example/[redacted]"),
        ),
        (
            "https://h.example/caf%C3%A9",
            &["café"],
            Some("https://h.example/[redacted]"),
        ),
        ("//h.example/p", &[], None),
        ("https://h.example", &[], Some("https://h.example")),
        ("https://@h.example:/p", &[], Some("https://h.example/p")),
    ];
    for (api_base, secrets, expected) in cases {
        assert_eq!(
            sanitize_api_base(api_base, secrets).as_deref(),
            *expected,
            "{api_base:?} with {secrets:?}"
        );
    }
}

fn workspace_with(config: &str, dotenv: &str) -> (tempfile::TempDir, WorkspaceService) {
    let temporary = tempfile::tempdir().expect("temporary");
    let vibe_home = temporary.path().join("vibe-home");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    std::fs::write(vibe_home.join("config.toml"), config).expect("config");
    std::fs::write(vibe_core::config::global_env_file(&vibe_home), dotenv).expect("dotenv");
    let working_directory = temporary.path().join("workspace");
    std::fs::create_dir_all(&working_directory).expect("workspace");
    let workspace = WorkspaceService::new(
        WorkspacePaths {
            vibe_home,
            working_directory,
            session_root: temporary.path().join("sessions"),
        },
        false,
    )
    .expect("workspace service");
    (temporary, workspace)
}

/// Reference `build_provider_auth_view`: the model under its alias when it
/// declares no display name, the provider by name, and the base with the
/// credential the provider's variable holds redacted, read from the global
/// dotenv file as the reference reads it from the environment it loaded.
#[test]
fn the_view_names_the_active_model_and_redacts_its_provider_credential() {
    let (_temporary, workspace) = workspace_with(
        "active_model = \"lab\"\n\n[[providers]]\nname = \"lab-provider\"\n\
         api_base = \"https://user:pw@LAB.example:8443/v1/key-TOKEN?x=1\"\n\
         api_key_env_var = \"VIBE_PROVIDER_AUTH_VIEW_TEST_KEY\"\nbackend = \"generic\"\n\n\
         [[models]]\nname = \"lab-model\"\nprovider = \"lab-provider\"\nalias = \"lab\"\n",
        "VIBE_PROVIDER_AUTH_VIEW_TEST_KEY=key-token\n",
    );
    assert_eq!(
        workspace.provider_auth().expect("a view"),
        json!({
            "modelDisplayName": "lab",
            "providerName": "lab-provider",
            "apiBase": "https://lab.example:8443/v1/[redacted]",
        })
    );
}

#[test]
fn a_display_name_wins_over_the_alias_and_an_unshowable_base_is_null() {
    let (_temporary, workspace) = workspace_with(
        "active_model = \"lab\"\n\n[[providers]]\nname = \"lab-provider\"\n\
         api_base = \"ftp://lab.example/v1\"\nbackend = \"generic\"\n\n\
         [[models]]\nname = \"lab-model\"\nprovider = \"lab-provider\"\nalias = \"lab\"\n\
         display_name = \"Lab Model\"\n",
        "",
    );
    assert_eq!(
        workspace.provider_auth().expect("a view"),
        json!({
            "modelDisplayName": "Lab Model",
            "providerName": "lab-provider",
            "apiBase": null,
        })
    );
}
