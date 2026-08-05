//! Differential oracle for the MCP configuration surface.
//!
//! `scripts/parity/config_surface.py` drives the reference's own URL
//! normalization, name suggestion and entry validation over a fixture set and
//! records what each produced. This module replays that section against the
//! store: the same URL is normalized the same way or refused, the same alias is
//! suggested and deduplicated, and the same entry decodes to the same values or
//! is refused.
//!
//! Only observations are recorded. A rejection carries the fact that the
//! reference refused the input and never its message, which is reference
//! prose `NOTICE` forbids shipping.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::mcp::{
    mcp_server_url_key, normalize_mcp_server_name, normalize_mcp_server_url,
    resolve_new_mcp_server_name, suggest_mcp_server_name,
};
use super::*;
use crate::mcp::{McpAuthConfig, McpServerConfig, McpTransportConfig};

const CORPUS_RELATIVE: &str = "tests/config-surface/corpus.json";
const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    reference: Reference,
    mcp: McpCorpus,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct McpCorpus {
    token_variable: String,
    token_value: String,
    urls: Vec<UrlCase>,
    names: Vec<NameCase>,
    resolutions: Vec<ResolutionCase>,
    entries: Vec<EntryCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UrlCase {
    input: String,
    rejected: bool,
    #[serde(default)]
    normalized: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    suggested: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NameCase {
    input: String,
    normalized: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolutionCase {
    requested: Option<String>,
    url: String,
    existing: Vec<String>,
    rejected: bool,
    #[serde(default)]
    resolved: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntryCase {
    name: String,
    toml: String,
    rejected: bool,
    #[serde(default)]
    decoded: Option<DecodedEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DecodedEntry {
    name: String,
    transport: String,
    prompt: Option<String>,
    sampling_enabled: bool,
    disabled: bool,
    disabled_tools: Vec<String>,
    startup_timeout_sec: f64,
    tool_timeout_sec: f64,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    auth_type: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    api_key_header: Option<String>,
    #[serde(default)]
    api_key_format: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_metadata_url: Option<String>,
    #[serde(default)]
    redirect_port: Option<u16>,
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Recorded for completeness. This port defaults an undeclared working
    /// directory to the session's, where the reference leaves it unset, so the
    /// two are not compared.
    #[serde(default)]
    #[expect(dead_code, reason = "recorded, deliberately not compared")]
    cwd: Option<String>,
}

fn corpus() -> Corpus {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

/// Decodes `document` through a real load, which is how every reader reaches
/// the entries.
fn servers(document: &str) -> Result<Vec<McpServerConfig>, ConfigError> {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    let project = temporary.path().join("project");
    fs::create_dir_all(&home).expect("home directory");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(home.join("config.toml"), document).expect("user fixture");
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: project.clone(),
        },
        Table::new(),
    )
    .load()?
    .mcp_servers(&project)
}

#[test]
fn every_recorded_url_normalizes_or_is_refused_as_the_reference_does() {
    let corpus = corpus();
    for case in &corpus.mcp.urls {
        let outcome = normalize_mcp_server_url(&case.input);
        if case.rejected {
            assert!(
                outcome.is_err(),
                "the reference refuses `{}`, this port accepts it",
                case.input
            );
            continue;
        }
        let normalized = outcome.unwrap_or_else(|error| {
            panic!(
                "the reference accepts `{}`, this port refuses it: {error}",
                case.input
            )
        });
        assert_eq!(
            Some(&normalized),
            case.normalized.as_ref(),
            "`{}` normalizes differently",
            case.input
        );
        assert_eq!(
            Some(mcp_server_url_key(&normalized)),
            case.key,
            "`{}` compares under another key",
            case.input
        );
        assert_eq!(
            Some(suggest_mcp_server_name(&normalized)),
            case.suggested,
            "`{}` suggests another name",
            case.input
        );
    }
}

#[test]
fn every_recorded_name_normalizes_as_the_reference_does() {
    let corpus = corpus();
    for case in &corpus.mcp.names {
        assert_eq!(
            normalize_mcp_server_name(&case.input),
            case.normalized,
            "`{}` normalizes differently",
            case.input
        );
    }
}

#[test]
fn every_recorded_add_resolves_to_the_reference_name_or_the_same_refusal() {
    let corpus = corpus();
    for case in &corpus.mcp.resolutions {
        let normalized_url = normalize_mcp_server_url(&case.url).expect("the fixture URL is valid");
        let existing = case.existing.iter().cloned().collect();
        let outcome =
            resolve_new_mcp_server_name(case.requested.as_deref(), &normalized_url, &existing);
        if case.rejected {
            assert!(
                outcome.is_err(),
                "the reference refuses {:?} against {:?}, this port accepts it",
                case.requested,
                case.existing
            );
            continue;
        }
        assert_eq!(
            outcome.ok(),
            case.resolved,
            "{:?} against {:?} resolves differently",
            case.requested,
            case.existing
        );
    }
}

#[test]
fn every_recorded_entry_decodes_to_the_reference_values_or_the_same_refusal() {
    let corpus = corpus();
    for case in &corpus.mcp.entries {
        let outcome = servers(&case.toml);
        let Some(expected) = &case.decoded else {
            assert!(
                case.rejected,
                "scenario `{}` records neither a decode nor a refusal",
                case.name
            );
            assert!(
                outcome.is_err(),
                "the reference refuses scenario `{}`, this port accepts it",
                case.name
            );
            continue;
        };
        let decoded = outcome.unwrap_or_else(|error| {
            panic!(
                "the reference accepts scenario `{}`, this port refuses it: {error}",
                case.name
            )
        });
        assert_eq!(
            decoded.len(),
            1,
            "scenario `{}` declares one entry",
            case.name
        );
        compare_entry(&case.name, expected, &decoded[0], &corpus.mcp);
    }
    println!(
        "config surface: {}/{} MCP entry scenarios conform, {} URL scenarios, {} add resolutions",
        corpus.mcp.entries.len(),
        corpus.mcp.entries.len(),
        corpus.mcp.urls.len(),
        corpus.mcp.resolutions.len()
    );
}

fn compare_entry(
    scenario: &str,
    expected: &DecodedEntry,
    actual: &McpServerConfig,
    corpus: &McpCorpus,
) {
    assert_eq!(actual.alias, expected.name, "scenario `{scenario}` name");
    assert_eq!(
        transport_name(&actual.transport),
        expected.transport,
        "scenario `{scenario}` transport"
    );
    assert_eq!(
        actual.prompt, expected.prompt,
        "scenario `{scenario}` prompt"
    );
    assert_eq!(
        actual.sampling_enabled, expected.sampling_enabled,
        "scenario `{scenario}` sampling"
    );
    assert_eq!(
        !actual.enabled, expected.disabled,
        "scenario `{scenario}` enablement"
    );
    assert_eq!(
        actual.disabled_tools.iter().cloned().collect::<Vec<_>>(),
        expected.disabled_tools,
        "scenario `{scenario}` disabled tools"
    );
    assert_eq!(
        actual.startup_timeout_ms as f64 / 1_000.0,
        expected.startup_timeout_sec,
        "scenario `{scenario}` startup timeout"
    );
    assert_eq!(
        actual.tool_timeout_ms as f64 / 1_000.0,
        expected.tool_timeout_sec,
        "scenario `{scenario}` tool timeout"
    );

    match &actual.transport {
        McpTransportConfig::Stdio {
            command,
            arguments,
            environment,
            ..
        } => {
            let argv = std::iter::once(command.clone())
                .chain(arguments.iter().cloned())
                .collect::<Vec<_>>();
            assert_eq!(
                Some(&argv),
                expected.argv.as_ref(),
                "scenario `{scenario}` argument vector"
            );
            assert_eq!(
                environment, &expected.env,
                "scenario `{scenario}` environment"
            );
        }
        McpTransportConfig::Http { url, headers }
        | McpTransportConfig::StreamableHttp { url, headers } => {
            assert_eq!(
                Some(url.to_string()),
                expected.url,
                "scenario `{scenario}` URL"
            );
            assert_eq!(
                headers, &expected.headers,
                "scenario `{scenario}` declared headers"
            );
            compare_auth(scenario, expected, actual, headers, corpus);
        }
    }
}

fn compare_auth(
    scenario: &str,
    expected: &DecodedEntry,
    actual: &McpServerConfig,
    headers: &BTreeMap<String, String>,
    corpus: &McpCorpus,
) {
    match &actual.auth {
        McpAuthConfig::Static(statics) => {
            assert_eq!(
                expected.auth_type.as_deref(),
                Some("static"),
                "scenario `{scenario}` auth kind"
            );
            assert_eq!(
                Some(&statics.api_key_env),
                expected.api_key_env.as_ref(),
                "scenario `{scenario}` token variable"
            );
            assert_eq!(
                Some(&statics.api_key_header),
                expected.api_key_header.as_ref(),
                "scenario `{scenario}` token header"
            );
            assert_eq!(
                Some(&statics.api_key_format),
                expected.api_key_format.as_ref(),
                "scenario `{scenario}` token format"
            );
            // The reference resolves the token when it makes the request, and
            // so does this port; the capture seeded one variable, which the
            // replay answers here rather than mutating the process.
            let mut computed = headers.clone();
            if let Some((name, value)) = statics.token_header(headers, |variable| {
                (variable == corpus.token_variable).then(|| corpus.token_value.clone())
            }) {
                computed.insert(name, value);
            }
            assert_eq!(
                computed, expected.http_headers,
                "scenario `{scenario}` request headers"
            );
        }
        McpAuthConfig::Oauth(oauth) => {
            assert_eq!(
                expected.auth_type.as_deref(),
                Some("oauth"),
                "scenario `{scenario}` auth kind"
            );
            assert_eq!(
                Some(&oauth.scopes),
                expected.scopes.as_ref(),
                "scenario `{scenario}` scopes"
            );
            assert_eq!(
                oauth.client_id, expected.client_id,
                "scenario `{scenario}` client id"
            );
            assert_eq!(
                oauth.client_metadata_url.as_ref().map(Url::to_string),
                expected.client_metadata_url,
                "scenario `{scenario}` client metadata URL"
            );
            assert_eq!(
                Some(oauth.redirect_port),
                expected.redirect_port,
                "scenario `{scenario}` redirect port"
            );
            assert!(
                expected.http_headers.is_empty(),
                "scenario `{scenario}` sends no static header under OAuth"
            );
        }
    }
}

fn transport_name(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Http { .. } => "http",
        McpTransportConfig::StreamableHttp { .. } => "streamable-http",
        McpTransportConfig::Stdio { .. } => "stdio",
    }
}
