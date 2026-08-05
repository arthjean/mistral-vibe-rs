//! MCP naming, addressing and decoding, exercised through the store that reads
//! and writes the entries rather than through the helpers alone.

use std::collections::{BTreeMap, BTreeSet};

use super::mcp::{
    McpRemoval, dedupe_mcp_server_name, mcp_server_url_key, normalize_mcp_server_name,
    normalize_mcp_server_url, resolve_new_mcp_server_name, suggest_mcp_server_name,
};
use super::*;
use crate::mcp::{
    McpAuthConfig, McpOAuthConfig, McpServerConfig, McpStaticAuth, McpTransportConfig,
};

const WORKSPACE: &str = "/workspace";

/// Loads `document` as the user configuration and decodes its servers, which is
/// the path every reader of `mcp_servers` takes.
fn servers(document: &str) -> Result<Vec<McpServerConfig>, ConfigError> {
    let (_temporary, store) = store_with(document);
    store.load()?.mcp_servers(Path::new(WORKSPACE))
}

fn only_server(document: &str) -> McpServerConfig {
    let mut decoded = servers(document).expect("the entry decodes");
    assert_eq!(decoded.len(), 1, "the fixture declares one server");
    decoded.remove(0)
}

fn store_with(document: &str) -> (tempfile::TempDir, LayeredConfig) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    let project = temporary.path().join("project");
    fs::create_dir_all(&home).expect("home directory");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(home.join("config.toml"), document).expect("user fixture");
    let store = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: project,
        },
        Table::new(),
    );
    (temporary, store)
}

fn remote(alias: &str, url: &str) -> McpServerConfig {
    McpServerConfig {
        alias: alias.to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: Url::parse(url).expect("fixture URL"),
            headers: BTreeMap::new(),
        },
        enabled: true,
        disabled_tools: BTreeSet::new(),
        startup_timeout_ms: crate::mcp::DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        tool_timeout_ms: crate::mcp::DEFAULT_MCP_TOOL_TIMEOUT_MS,
        auth: McpAuthConfig::default(),
        prompt: None,
        sampling_enabled: true,
    }
}

fn message(error: &ConfigError) -> String {
    error.to_string()
}

// --------------------------------------------------------------------------
// US-074: URLs
// --------------------------------------------------------------------------

#[test]
fn normalization_lowercases_drops_the_default_port_and_keeps_ipv6_bracketed() {
    assert_eq!(
        normalize_mcp_server_url("HTTPS://MCP.Example.COM:443/Tools").expect("normalizes"),
        "https://mcp.example.com/Tools"
    );
    assert_eq!(
        normalize_mcp_server_url("https://mcp.example.com").expect("normalizes"),
        "https://mcp.example.com",
        "a URL with no path keeps none"
    );
    assert_eq!(
        normalize_mcp_server_url("https://[2001:DB8::1]:8443/rpc").expect("normalizes"),
        "https://[2001:db8::1]:8443/rpc"
    );
}

#[test]
fn a_trailing_slash_and_a_default_port_name_the_same_server() {
    assert_eq!(
        mcp_server_url_key("https://mcp.example.com/tools/"),
        mcp_server_url_key("https://MCP.example.com:443/tools")
    );
    assert_ne!(
        mcp_server_url_key("https://mcp.example.com/tools"),
        mcp_server_url_key("https://mcp.example.com/other")
    );
}

#[test]
fn plaintext_http_is_refused_unless_it_cannot_leave_the_machine() {
    for loopback in [
        "http://localhost:3000/mcp",
        "http://127.0.0.1:3000/mcp",
        "http://[::1]:3000/mcp",
    ] {
        assert!(
            normalize_mcp_server_url(loopback).is_ok(),
            "{loopback} points at this machine"
        );
    }
    let error = normalize_mcp_server_url("http://mcp.example.com/rpc")
        .expect_err("plaintext to another host is refused");
    assert!(message(&error).contains("https"));
}

/// Every rejection names the defect, and none echoes the URL, so a credential
/// spelled into one never reaches a log line.
#[test]
fn every_malformed_url_is_refused_without_echoing_it() {
    let cases = [
        ("https://user:secret@mcp.example.com/rpc", "credentials"),
        ("https://mcp.example.com/rpc#section", "fragment"),
        ("mcp.example.com/rpc", "scheme"),
        ("ftp://mcp.example.com/rpc", "https"),
        ("https://", "host"),
        ("   ", "required"),
    ];
    for (url, expected) in cases {
        let error = normalize_mcp_server_url(url).expect_err("the URL is refused");
        let rendered = message(&error);
        assert!(
            rendered.contains(expected),
            "`{url}` should be refused for its {expected}, got `{rendered}`"
        );
        assert!(
            !rendered.contains("secret") && !rendered.contains("mcp.example.com"),
            "the rejection must not echo the URL: `{rendered}`"
        );
    }
}

#[test]
fn an_equivalent_url_is_refused_and_names_the_server_that_holds_it() {
    let (_temporary, store) = store_with(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example.com/mcp/"
"#,
    );

    let error = store
        .preflight_mcp_add(&remote("other", "https://DOCS.example.com:443/mcp"))
        .expect_err("the same endpoint under another spelling is refused");

    assert!(
        message(&error).contains("`docs`"),
        "the rejection names the configured server: `{}`",
        message(&error)
    );
}

// --------------------------------------------------------------------------
// US-075: names
// --------------------------------------------------------------------------

#[test]
fn a_name_keeps_only_letters_digits_underscores_and_hyphens() {
    assert_eq!(normalize_mcp_server_name("My Server!"), "My_Server");
    assert_eq!(normalize_mcp_server_name("__docs--"), "docs");
    assert_eq!(normalize_mcp_server_name("héllo"), "h_llo");
    assert_eq!(normalize_mcp_server_name("!!!"), "");
    assert_eq!(normalize_mcp_server_name(&"a".repeat(300)).len(), 256);
}

#[test]
fn a_suggestion_drops_a_leading_prefix_and_falls_back_through_the_path() {
    assert_eq!(
        suggest_mcp_server_name("https://mcp.github.com/api"),
        "github"
    );
    assert_eq!(
        suggest_mcp_server_name("https://www.example.com/rpc"),
        "example"
    );
    assert_eq!(
        suggest_mcp_server_name("https://mcp.example/rpc"),
        "rpc",
        "a two-label host keeps its first label, which is generic, so the path names it"
    );
    assert_eq!(
        suggest_mcp_server_name("https://api.example.com/v1"),
        "v1",
        "`api` is not a droppable prefix, but it is too generic to name a server"
    );
    assert_eq!(
        suggest_mcp_server_name("https://api.example/api/mcp"),
        "mcp",
        "a generic first label with no usable path segment falls back"
    );
}

#[test]
fn a_suggested_name_is_numbered_until_it_is_free() {
    let taken = BTreeSet::from(["github".to_owned(), "github_2".to_owned()]);
    assert_eq!(dedupe_mcp_server_name("github", &taken), "github_3");
    assert_eq!(dedupe_mcp_server_name("gitlab", &taken), "gitlab");
    assert_eq!(
        resolve_new_mcp_server_name(None, "https://mcp.github.com/api", &taken)
            .expect("a suggestion is always available"),
        "github_3"
    );
}

#[test]
fn an_explicitly_requested_name_is_refused_rather_than_renamed() {
    let taken = BTreeSet::from(["github".to_owned()]);

    let error = resolve_new_mcp_server_name(Some("github"), "https://mcp.github.com/api", &taken)
        .expect_err("a requested name that collides is refused");
    assert!(message(&error).contains("`github`"));

    let empty = resolve_new_mcp_server_name(Some("!!!"), "https://mcp.github.com/api", &taken)
        .expect_err("a name that normalizes to nothing is refused");
    assert!(message(&empty).contains("letters or numbers"));
}

// --------------------------------------------------------------------------
// US-076: transports and auth
// --------------------------------------------------------------------------

#[test]
fn the_three_reference_transports_decode_and_anything_else_is_refused() {
    let decoded = servers(
        r#"
[[mcp_servers]]
name = "legacy"
transport = "http"
url = "https://legacy.example/mcp"

[[mcp_servers]]
name = "streaming"
transport = "streamable-http"
url = "https://streaming.example/mcp"

[[mcp_servers]]
name = "local"
transport = "stdio"
command = "vibe-mcp"
"#,
    )
    .expect("every reference transport decodes");
    assert!(matches!(
        decoded[0].transport,
        McpTransportConfig::Http { .. }
    ));
    assert!(matches!(
        decoded[1].transport,
        McpTransportConfig::StreamableHttp { .. }
    ));
    assert!(matches!(
        decoded[2].transport,
        McpTransportConfig::Stdio { .. }
    ));

    let error = servers(
        r#"
[[mcp_servers]]
name = "sse"
transport = "sse"
url = "https://sse.example/secret-path"
command = "secret-command"
"#,
    )
    .expect_err("an unknown transport is refused");
    let rendered = message(&error);
    assert!(rendered.contains("stdio"), "{rendered}");
    assert!(
        !rendered.contains("secret-path") && !rendered.contains("secret-command"),
        "the rejection must not echo the entry: `{rendered}`"
    );
}

#[test]
fn a_static_auth_block_validates_its_headers_variable_and_format() {
    let server = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_env = "DOCS_TOKEN"
api_key_header = "X-Api-Key"
api_key_format = "Token {token}"
headers = { X-Trace = "on" }
"#,
    );
    let McpAuthConfig::Static(statics) = &server.auth else {
        panic!("the entry declares static auth");
    };
    assert_eq!(statics.api_key_env, "DOCS_TOKEN");
    assert_eq!(statics.api_key_header, "X-Api-Key");
    assert_eq!(statics.api_key_format, "Token {token}");

    let refusals = [
        (r#"headers = { "Bad Header" = "1" }"#, "header name"),
        (
            r#"headers = { Authorization = "a", authorization = "b" }"#,
            "duplicate",
        ),
        (r#"api_key_env = "2BAD""#, "environment variable"),
        (r#"api_key_format = "Bearer {{token}}""#, "placeholder"),
        (r#"api_key_format = "Bearer {secret}""#, "token placeholder"),
    ];
    for (line, expected) in refusals {
        let document = format!(
            r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
{line}
"#
        );
        let error = servers(&document).expect_err("the auth block is refused");
        assert!(
            message(&error).contains(expected),
            "`{line}` should be refused for its {expected}, got `{}`",
            message(&error)
        );
    }
}

/// A format spec is a replacement field the substring test would miss, and the
/// reference accepts it because it still names the token.
#[test]
fn a_token_placeholder_carrying_a_format_spec_is_accepted() {
    let server = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_env = "DOCS_TOKEN"
api_key_format = "Bearer {token:>8}"
"#,
    );
    let McpAuthConfig::Static(statics) = &server.auth else {
        panic!("the entry declares static auth");
    };
    assert_eq!(statics.api_key_format, "Bearer {token:>8}");
}

#[test]
fn legacy_top_level_auth_keys_are_promoted_and_mixing_them_is_refused() {
    let server = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
api_key_env = "DOCS_TOKEN"
headers = { X-Trace = "on" }
"#,
    );
    let McpAuthConfig::Static(statics) = &server.auth else {
        panic!("the legacy keys promote into a static block");
    };
    assert_eq!(statics.api_key_env, "DOCS_TOKEN");
    let McpTransportConfig::StreamableHttp { headers, .. } = &server.transport else {
        panic!("the entry is remote");
    };
    assert_eq!(headers.get("X-Trace").map(String::as_str), Some("on"));

    let error = servers(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
api_key_env = "DOCS_TOKEN"

[mcp_servers.auth]
type = "static"
"#,
    )
    .expect_err("mixing the two forms is refused");
    assert!(message(&error).contains("legacy"), "{}", message(&error));
}

#[test]
fn an_oauth_block_requires_scopes_and_bounds_its_identity_and_port() {
    let server = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "oauth"
scopes = ["repo", "read"]
client_id = "vibe"
redirect_port = 51000
"#,
    );
    let oauth = server.auth.oauth().expect("the entry declares OAuth");
    assert_eq!(oauth.scopes, ["repo".to_owned(), "read".to_owned()]);
    assert_eq!(oauth.client_id.as_deref(), Some("vibe"));
    assert_eq!(oauth.redirect_port, 51_000);

    let refusals = [
        ("client_id = \"vibe\"", "scopes"),
        (
            "scopes = []\nclient_id = \"vibe\"\nclient_metadata_url = \"https://docs.example/client.json\"",
            "mutually exclusive",
        ),
        ("scopes = []\nredirect_port = 80", "redirect_port"),
        ("scopes = []\nredirect_port = 70000", "redirect_port"),
    ];
    for (block, expected) in refusals {
        let document = format!(
            r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "oauth"
{block}
"#
        );
        let error = servers(&document).expect_err("the OAuth block is refused");
        assert!(
            message(&error).contains(expected),
            "`{block}` should be refused for its {expected}, got `{}`",
            message(&error)
        );
    }
}

/// The token is read when the request is made, and an explicit header of the
/// same name wins, compared the way HTTP compares header names.
#[test]
fn the_token_header_is_added_unless_the_entry_already_carries_one() {
    let server = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
api_key_env = "DOCS_TOKEN"
"#,
    );
    let McpAuthConfig::Static(statics) = &server.auth else {
        panic!("the entry declares static auth");
    };
    let McpTransportConfig::StreamableHttp { headers, .. } = &server.transport else {
        panic!("the entry is remote");
    };
    assert_eq!(
        statics.token_header(headers, |_| Some("secret".to_owned())),
        Some(("Authorization".to_owned(), "Bearer secret".to_owned()))
    );
    assert_eq!(
        statics.token_header(headers, |_| None),
        None,
        "an unset variable contributes nothing"
    );
    assert_eq!(
        statics.token_header(headers, |_| Some(String::new())),
        None,
        "an empty variable contributes nothing"
    );
    let explicit = BTreeMap::from([("authorization".to_owned(), "Bearer explicit".to_owned())]);
    assert_eq!(
        statics.token_header(&explicit, |_| Some("secret".to_owned())),
        None,
        "an explicit header of the same name wins"
    );
}

#[test]
fn a_stdio_command_is_shell_split_and_the_declared_arguments_follow() {
    let server = only_server(
        r#"
[[mcp_servers]]
name = "local"
transport = "stdio"
command = "npx -y @scope/server"
args = ["--stdio"]
"#,
    );
    let McpTransportConfig::Stdio {
        command, arguments, ..
    } = &server.transport
    else {
        panic!("the entry is stdio");
    };
    assert_eq!(command, "npx");
    assert_eq!(arguments, &["-y", "@scope/server", "--stdio"]);

    let listed = only_server(
        r#"
[[mcp_servers]]
name = "local"
transport = "stdio"
command = ["npx", "-y", "@scope/server"]
args = ["--stdio"]
"#,
    );
    assert_eq!(listed.transport, server.transport);
}

#[test]
fn the_remaining_entry_fields_carry_the_reference_defaults() {
    let bare = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
"#,
    );
    assert!(bare.enabled);
    assert!(bare.disabled_tools.is_empty());
    assert_eq!(bare.prompt, None);
    assert!(bare.sampling_enabled, "sampling defaults to allowed");

    let declared = only_server(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
prompt = "Search the handbook first"
sampling_enabled = false
disabled = true
disabled_tools = ["search"]
"#,
    );
    assert!(!declared.enabled);
    assert_eq!(
        declared.disabled_tools,
        BTreeSet::from(["search".to_owned()])
    );
    assert_eq!(
        declared.prompt.as_deref(),
        Some("Search the handbook first")
    );
    assert!(!declared.sampling_enabled);
}

/// What a persisted entry keeps is proved by writing one and reading it back,
/// because a configuration shared between two clients depends on that trip.
#[test]
fn a_persisted_server_round_trips_through_the_file_it_was_written_to() {
    let (_temporary, store) = store_with("");
    let mut server = remote("docs", "https://docs.example/mcp");
    // An OAuth entry declares no headers of its own: the bearer token is put on
    // the request when the login completes, and never persisted.
    server.transport = McpTransportConfig::Http {
        url: Url::parse("https://docs.example/mcp").expect("fixture URL"),
        headers: BTreeMap::new(),
    };
    server.auth = McpAuthConfig::Oauth(McpOAuthConfig {
        scopes: vec!["repo".to_owned()],
        ..McpOAuthConfig::default()
    });
    server.prompt = Some("Search the handbook first".to_owned());
    server.sampling_enabled = false;
    server.enabled = false;

    store.persist_mcp_add(&server).expect("the entry persists");
    let mut reloaded = store
        .load()
        .expect("the configuration reloads")
        .mcp_servers(Path::new(WORKSPACE))
        .expect("the entry decodes");

    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded.remove(0), server);
}

#[test]
fn a_static_auth_entry_round_trips_without_the_defaults_it_never_set() {
    let (temporary, store) = store_with("");
    let mut server = remote("docs", "https://docs.example/mcp");
    server.transport = McpTransportConfig::StreamableHttp {
        url: Url::parse("https://docs.example/mcp").expect("fixture URL"),
        headers: BTreeMap::from([("X-Trace".to_owned(), "on".to_owned())]),
    };
    server.auth = McpAuthConfig::Static(McpStaticAuth {
        api_key_env: "DOCS_TOKEN".to_owned(),
        ..McpStaticAuth::default()
    });

    store.persist_mcp_add(&server).expect("the entry persists");

    let written = fs::read_to_string(temporary.path().join("home/.vibe/config.toml"))
        .expect("the written file reads back");
    assert!(written.contains("DOCS_TOKEN"));
    assert!(
        !written.contains("api_key_header"),
        "a default is not written back: {written}"
    );
    let reloaded = store
        .load()
        .expect("the configuration reloads")
        .mcp_servers(Path::new(WORKSPACE))
        .expect("the entry decodes");
    assert_eq!(reloaded[0], server);
}

#[test]
fn removing_by_name_drops_the_entry_and_reports_an_unknown_name() {
    let (_temporary, store) = store_with(
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[[mcp_servers]]
name = "issues"
transport = "streamable-http"
url = "https://issues.example/mcp"
"#,
    );

    assert_eq!(
        store
            .persist_mcp_remove("docs")
            .expect("the entry is removed"),
        McpRemoval {
            name: "docs".to_owned(),
            removed: true,
        }
    );
    assert_eq!(
        store
            .persist_mcp_remove("unknown")
            .expect("an unknown name is answered, not raised"),
        McpRemoval {
            name: "unknown".to_owned(),
            removed: false,
        }
    );

    let remaining = store
        .load()
        .expect("the configuration reloads")
        .mcp_servers(Path::new(WORKSPACE))
        .expect("the remaining entry decodes");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].alias, "issues");
}

/// A reader only ever sees the normalized alias, so a write addressed with it
/// has to find the entry it came from, whatever spelling the file holds.
#[test]
fn an_entry_written_under_a_raw_name_is_addressed_by_its_normalized_alias() {
    let (_temporary, store) = store_with(
        r#"
[[mcp_servers]]
name = "My Server!"
transport = "streamable-http"
url = "https://docs.example/mcp"
"#,
    );
    let snapshot = store.load().expect("the configuration loads");
    assert_eq!(
        snapshot.mcp_aliases().expect("aliases decode"),
        BTreeSet::from(["My_Server".to_owned()])
    );

    store
        .persist_mcp_state("My_Server", false, &BTreeSet::from(["search".to_owned()]))
        .expect("the entry is found under the alias every reader sees");

    let reloaded = store
        .load()
        .expect("the configuration reloads")
        .mcp_servers(Path::new(WORKSPACE))
        .expect("the entry decodes");
    assert_eq!(reloaded.len(), 1, "the write updated the entry in place");
    assert!(!reloaded[0].enabled);
    assert_eq!(
        reloaded[0].disabled_tools,
        BTreeSet::from(["search".to_owned()])
    );
}
