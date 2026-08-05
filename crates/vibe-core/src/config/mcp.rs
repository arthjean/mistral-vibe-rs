//! How an MCP entry is named, addressed, decoded and deduplicated.
//!
//! The reference owns these rules in `vibe/core/config/mcp_servers.py` and the
//! transport and auth unions of `vibe/core/config/models.py`. They decide which
//! spellings of a URL mean one server, which alias an operator gets when they
//! supply none, and which entries load at all, so they live beside the store
//! rather than at the call sites that add or read a server.

use super::*;
use crate::mcp::{
    DEFAULT_MCP_API_KEY_FORMAT, DEFAULT_MCP_API_KEY_HEADER, DEFAULT_MCP_OAUTH_REDIRECT_PORT,
    MCP_TOKEN_PLACEHOLDER, McpAuthConfig, McpOAuthConfig, McpStaticAuth,
};

/// The greatest length a normalized alias keeps.
const MAX_NAME_CHARS: usize = 256;
/// Host labels dropped from a suggestion when the host has enough of them left.
const HOST_PREFIXES_TO_DROP: [&str; 2] = ["mcp", "www"];
/// Labels and path segments too generic to name a server by.
const GENERIC_ALIAS_SEGMENTS: [&str; 3] = ["api", "mcp", "server"];
/// How many labels a host needs before its leading prefix is dropped.
const LEADING_HOST_PREFIX_LABEL_MIN_COUNT: usize = 3;
/// The alias used when nothing in the URL yields one.
const FALLBACK_ALIAS: &str = "mcp";
/// Top-level keys promoted into a static auth block, as the reference promotes
/// them before validating an entry.
const LEGACY_STATIC_AUTH_KEYS: [&str; 4] =
    ["headers", "api_key_env", "api_key_header", "api_key_format"];
/// The non-privileged range an OAuth callback may bind.
const REDIRECT_PORT_RANGE: std::ops::RangeInclusive<u16> = 1024..=65535;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::InvalidMcp(message.into())
}

// --------------------------------------------------------------------------
// Names
// --------------------------------------------------------------------------

/// The alias form an entry is stored and matched under.
///
/// Anything outside letters, digits, underscore and hyphen becomes an
/// underscore, the ends are trimmed of both, and the result is bounded.
#[must_use]
pub fn normalize_mcp_server_name(value: &str) -> String {
    let substituted = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    substituted
        .trim_matches(['_', '-'])
        .chars()
        .take(MAX_NAME_CHARS)
        .collect()
}

/// The alias suggested for a server the operator named nothing for.
#[must_use]
pub fn suggest_mcp_server_name(url: &str) -> String {
    let host = Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| FALLBACK_ALIAS.to_owned())
        .to_lowercase();
    let mut labels = host
        .split('.')
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    if labels.len() >= LEADING_HOST_PREFIX_LABEL_MIN_COUNT
        && labels
            .first()
            .is_some_and(|label| HOST_PREFIXES_TO_DROP.contains(label))
    {
        labels.remove(0);
    }
    let mut candidate = labels.first().map_or_else(String::new, |label| {
        normalize_mcp_server_name(label).to_lowercase()
    });
    if GENERIC_ALIAS_SEGMENTS.contains(&candidate.as_str()) {
        candidate = Url::parse(url)
            .ok()
            .map(|parsed| path_alias_candidate(parsed.path()))
            .unwrap_or_default();
    }
    let normalized = normalize_mcp_server_name(&candidate);
    if normalized.is_empty() {
        FALLBACK_ALIAS.to_owned()
    } else {
        normalized
    }
}

fn path_alias_candidate(path: &str) -> String {
    path.split('/')
        .map(|segment| normalize_mcp_server_name(&segment.to_lowercase()))
        .find(|segment| !segment.is_empty() && !GENERIC_ALIAS_SEGMENTS.contains(&segment.as_str()))
        .unwrap_or_default()
}

/// A free alias derived from `base`, numbered from two when it is taken.
#[must_use]
pub fn dedupe_mcp_server_name(base: &str, existing: &BTreeSet<String>) -> String {
    if !existing.contains(base) {
        return base.to_owned();
    }
    let mut index = 2_u32;
    loop {
        let candidate = format!("{base}_{index}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        index = index.saturating_add(1);
    }
}

/// The alias a new server is added under.
///
/// A suggestion is deduplicated; a name the operator asked for is not, because
/// renaming it would silently add a server under an alias they never chose.
pub fn resolve_new_mcp_server_name(
    requested: Option<&str>,
    normalized_url: &str,
    existing: &BTreeSet<String>,
) -> Result<String, ConfigError> {
    let Some(requested) = requested else {
        return Ok(dedupe_mcp_server_name(
            &suggest_mcp_server_name(normalized_url),
            existing,
        ));
    };
    let requested = normalize_mcp_server_name(requested);
    if requested.is_empty() {
        return Err(invalid("MCP server name must contain letters or numbers"));
    }
    if existing.contains(&requested) {
        return Err(invalid(format!(
            "MCP server name `{requested}` is already configured"
        )));
    }
    Ok(requested)
}

// --------------------------------------------------------------------------
// URLs
// --------------------------------------------------------------------------

/// The stored form of an MCP server URL, rejecting what cannot be one.
///
/// The scheme and host are lowercased, a default port for the scheme is
/// dropped, an IPv6 host stays bracketed and the fragment is removed. No
/// message echoes the URL, so a credential carried in one never reaches a log.
pub fn normalize_mcp_server_url(value: &str) -> Result<String, ConfigError> {
    let raw = value.trim();
    if raw.is_empty() {
        return Err(invalid("MCP server URL is required"));
    }
    let parsed = Url::parse(raw).map_err(|error| match error {
        url::ParseError::RelativeUrlWithoutBase => invalid("MCP server URL must include a scheme"),
        url::ParseError::EmptyHost => invalid("MCP server URL must include a host"),
        _ => invalid("MCP server URL must be a valid HTTP(S) URL"),
    })?;
    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https") {
        return Err(invalid("MCP server URL must use https"));
    }
    let Some(host) = parsed.host_str().filter(|host| !host.is_empty()) else {
        return Err(invalid("MCP server URL must include a host"));
    };
    if parsed
        .fragment()
        .is_some_and(|fragment| !fragment.is_empty())
    {
        return Err(invalid("MCP server URL must not include a fragment"));
    }
    if authority_of(raw).contains('@') {
        return Err(invalid("MCP server URL must not include credentials"));
    }
    if scheme == "http" && !crate::text::is_loopback_host(host) {
        return Err(invalid(
            "MCP server URL must use https unless it points to localhost",
        ));
    }
    Ok(render_url(&parsed, raw_has_path(raw), false))
}

/// The key two spellings of one endpoint share.
///
/// A trailing slash and a default port are the two differences the reference
/// collapses, so `https://example.com/mcp/` and `https://example.com:443/mcp`
/// are the same server.
#[must_use]
pub fn mcp_server_url_key(value: &str) -> String {
    let raw = value.trim();
    Url::parse(raw).map_or_else(
        |_| raw.to_owned(),
        |parsed| render_url(&parsed, raw_has_path(raw), true),
    )
}

/// The authority of `raw`, which is everything between the scheme separator and
/// the path, query or fragment that follows it.
fn authority_of(raw: &str) -> &str {
    let after_scheme = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    after_scheme
        .find(['/', '?', '#'])
        .map_or(after_scheme, |index| &after_scheme[..index])
}

/// Whether `raw` spelled a path at all, which `Url` cannot report because it
/// gives every special-scheme URL a `/` path.
fn raw_has_path(raw: &str) -> bool {
    let after_scheme = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    after_scheme
        .find(['/', '?', '#'])
        .is_some_and(|index| after_scheme.as_bytes().get(index) == Some(&b'/'))
}

fn render_url(parsed: &Url, has_path: bool, trim_trailing_slash: bool) -> String {
    let Some(host) = parsed.host_str() else {
        return parsed.as_str().to_owned();
    };
    let mut rendered = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        rendered.push(':');
        rendered.push_str(&port.to_string());
    }
    let path = if has_path { parsed.path() } else { "" };
    rendered.push_str(if trim_trailing_slash {
        path.trim_end_matches('/')
    } else {
        path
    });
    if let Some(query) = parsed.query() {
        rendered.push('?');
        rendered.push_str(query);
    }
    rendered
}

// --------------------------------------------------------------------------
// Decoding
// --------------------------------------------------------------------------

/// One persisted `[[mcp_servers]]` entry, decoded.
///
/// `working_directory` is where a relative stdio `cwd` resolves, and the
/// directory a stdio server runs in when it declares none.
pub(super) fn decode_mcp_server(
    table: &Table,
    working_directory: &Path,
) -> Result<McpServerConfig, ConfigError> {
    let alias = normalize_mcp_server_name(required_mcp_string(table, "name")?);
    if alias.is_empty() {
        return Err(invalid("MCP server name must contain letters or numbers"));
    }
    let declared = required_mcp_string(table, "transport")?;
    let auth = decode_auth(table)?;
    let transport = match declared {
        "stdio" => decode_stdio(table, working_directory)?,
        "http" | "streamable-http" => {
            let url = Url::parse(required_mcp_string(table, "url")?)
                .map_err(|_| invalid("MCP server field `url` must be a valid URL"))?;
            let headers = decode_headers(table)?;
            if declared == "http" {
                McpTransportConfig::Http { url, headers }
            } else {
                McpTransportConfig::StreamableHttp { url, headers }
            }
        }
        _ => {
            return Err(invalid(
                "MCP transport must be `http`, `streamable-http` or `stdio`",
            ));
        }
    };
    Ok(McpServerConfig {
        alias,
        transport,
        enabled: !optional_mcp_bool(table, "disabled")?.unwrap_or(false),
        disabled_tools: optional_mcp_strings(table, "disabled_tools")?
            .into_iter()
            .collect(),
        startup_timeout_ms: optional_mcp_timeout(
            table,
            "startup_timeout_sec",
            DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        )?,
        tool_timeout_ms: optional_mcp_timeout(
            table,
            "tool_timeout_sec",
            DEFAULT_MCP_TOOL_TIMEOUT_MS,
        )?,
        auth,
        prompt: optional_mcp_string(table, "prompt")?.map(str::to_owned),
        sampling_enabled: optional_mcp_bool(table, "sampling_enabled")?.unwrap_or(true),
    })
}

fn decode_stdio(
    table: &Table,
    working_directory: &Path,
) -> Result<McpTransportConfig, ConfigError> {
    let mut argv = match table.get("command") {
        Some(Value::String(command)) => shlex::split(command)
            .ok_or_else(|| invalid("MCP server field `command` is not valid shell quoting"))?,
        Some(Value::Array(_)) => optional_mcp_strings(table, "command")?,
        Some(_) => {
            return Err(invalid(
                "MCP server field `command` must be a string or an array of strings",
            ));
        }
        None => return Err(invalid("MCP server field `command` is required")),
    };
    if argv.is_empty() {
        return Err(invalid("MCP server field `command` is required"));
    }
    let command = argv.remove(0);
    argv.extend(optional_mcp_strings(table, "args")?);
    Ok(McpTransportConfig::Stdio {
        command,
        arguments: argv,
        environment: optional_mcp_environment_at(table, "env")?,
        working_directory: Some(
            optional_mcp_string(table, "cwd")?
                .map(PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        working_directory.join(path)
                    }
                })
                .unwrap_or_else(|| working_directory.to_path_buf()),
        ),
    })
}

/// The explicit headers an HTTP entry declares, from `[auth]` or from the
/// legacy top-level key the reference promotes into it.
fn decode_headers(table: &Table) -> Result<BTreeMap<String, String>, ConfigError> {
    let headers = match auth_table(table)? {
        Some(auth) => optional_mcp_environment_at(auth, "headers")?,
        None => optional_mcp_environment_at(table, "headers")?,
    };
    let mut seen = BTreeSet::new();
    for name in headers.keys() {
        if !is_http_header_name(name) {
            return Err(invalid(format!("invalid HTTP header name `{name}`")));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(invalid(format!("duplicate HTTP header `{name}`")));
        }
    }
    Ok(headers)
}

/// The `[auth]` block an entry declares, after the legacy top-level keys are
/// promoted into one.
fn auth_table(table: &Table) -> Result<Option<&Table>, ConfigError> {
    let legacy = LEGACY_STATIC_AUTH_KEYS
        .iter()
        .any(|key| table.contains_key(*key));
    match table.get("auth") {
        Some(Value::Table(auth)) if legacy => {
            let _ = auth;
            Err(invalid(
                "cannot mix top-level headers, api_key_env, api_key_header or api_key_format with an explicit [auth] block; move the legacy keys into [auth]",
            ))
        }
        Some(Value::Table(auth)) => Ok(Some(auth)),
        Some(_) => Err(invalid("MCP server field `auth` must be a table")),
        None => Ok(None),
    }
}

fn decode_auth(table: &Table) -> Result<McpAuthConfig, ConfigError> {
    let Some(auth) = auth_table(table)? else {
        return decode_static_auth(table).map(McpAuthConfig::Static);
    };
    match optional_mcp_string(auth, "type")? {
        None | Some("static") => {
            reject_unknown_auth_keys(
                auth,
                &[
                    "type",
                    "headers",
                    "api_key_env",
                    "api_key_header",
                    "api_key_format",
                ],
            )?;
            decode_static_auth(auth).map(McpAuthConfig::Static)
        }
        Some("oauth") => {
            reject_unknown_auth_keys(
                auth,
                &[
                    "type",
                    "scopes",
                    "client_id",
                    "client_metadata_url",
                    "redirect_port",
                ],
            )?;
            decode_oauth(auth).map(McpAuthConfig::Oauth)
        }
        Some(_) => Err(invalid("MCP auth type must be `static` or `oauth`")),
    }
}

/// Rejects a key the block does not declare, as the reference forbids extras
/// rather than ignoring a misspelled one.
fn reject_unknown_auth_keys(table: &Table, known: &[&str]) -> Result<(), ConfigError> {
    match table.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(invalid(format!("unknown MCP auth field `{key}`"))),
        None => Ok(()),
    }
}

fn decode_static_auth(table: &Table) -> Result<McpStaticAuth, ConfigError> {
    let api_key_env = optional_mcp_string(table, "api_key_env")?.unwrap_or_default();
    if !api_key_env.is_empty() && !is_environment_variable_name(api_key_env) {
        return Err(invalid(
            "`api_key_env` must be a valid environment variable name",
        ));
    }
    let api_key_header =
        optional_mcp_string(table, "api_key_header")?.unwrap_or(DEFAULT_MCP_API_KEY_HEADER);
    if !is_http_header_name(api_key_header) {
        return Err(invalid("`api_key_header` must be a valid HTTP header name"));
    }
    let api_key_format =
        optional_mcp_string(table, "api_key_format")?.unwrap_or(DEFAULT_MCP_API_KEY_FORMAT);
    validate_token_format(api_key_format)?;
    Ok(McpStaticAuth {
        api_key_env: api_key_env.to_owned(),
        api_key_header: api_key_header.to_owned(),
        api_key_format: api_key_format.to_owned(),
    })
}

fn decode_oauth(table: &Table) -> Result<McpOAuthConfig, ConfigError> {
    if !table.contains_key("scopes") {
        return Err(invalid(
            "MCP OAuth auth requires `scopes`; use an empty list to accept the server default",
        ));
    }
    let scopes = optional_mcp_strings(table, "scopes")?;
    let client_id = optional_mcp_string(table, "client_id")?.map(str::to_owned);
    let client_metadata_url =
        match optional_mcp_string(table, "client_metadata_url")? {
            Some(value) => Some(Url::parse(value).map_err(|_| {
                invalid("MCP server field `client_metadata_url` must be a valid URL")
            })?),
            None => None,
        };
    if client_id.is_some() && client_metadata_url.is_some() {
        return Err(invalid(
            "`client_id` and `client_metadata_url` are mutually exclusive",
        ));
    }
    let redirect_port = match table.get("redirect_port") {
        None => DEFAULT_MCP_OAUTH_REDIRECT_PORT,
        Some(Value::Integer(port)) => u16::try_from(*port)
            .ok()
            .filter(|port| REDIRECT_PORT_RANGE.contains(port))
            .ok_or_else(|| {
                invalid(format!(
                    "`redirect_port` must be between {} and {}",
                    REDIRECT_PORT_RANGE.start(),
                    REDIRECT_PORT_RANGE.end()
                ))
            })?,
        Some(_) => {
            return Err(invalid(
                "MCP server field `redirect_port` must be an integer",
            ));
        }
    };
    Ok(McpOAuthConfig {
        scopes,
        client_id,
        client_metadata_url,
        redirect_port,
    })
}

/// Whether `name` is an HTTP field name, which RFC 9110 defines as a token.
fn is_http_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(character)
        })
}

fn is_environment_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether `value` is a format string whose only replacement field is the
/// token, matching what the reference enumerates rather than substring-matching:
/// `{{token}}` carries the substring but yields no field, and `{token:>8}` is a
/// field the substring test would miss.
fn validate_token_format(value: &str) -> Result<(), ConfigError> {
    let mut fields = 0_usize;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
            }
            '}' => return Err(invalid("`api_key_format` must be a valid format string")),
            '{' => {
                let mut field = String::new();
                let mut closed = false;
                for character in characters.by_ref() {
                    if character == '}' {
                        closed = true;
                        break;
                    }
                    field.push(character);
                }
                if !closed {
                    return Err(invalid("`api_key_format` must be a valid format string"));
                }
                let name = field
                    .split([':', '!'])
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                if name != "token" {
                    return Err(invalid(
                        "`api_key_format` may only reference the token placeholder",
                    ));
                }
                fields = fields.saturating_add(1);
            }
            _ => {}
        }
    }
    if fields == 0 {
        return Err(invalid(format!(
            "`api_key_format` must contain the `{MCP_TOKEN_PLACEHOLDER}` placeholder"
        )));
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Persistence
// --------------------------------------------------------------------------

/// The persisted form of `config`, carrying only what differs from the shipped
/// default so a written entry stays as small as the reference writes it.
pub(super) fn mcp_server_table(config: &McpServerConfig) -> Result<Table, ConfigError> {
    let mut table = Table::new();
    table.insert("name".to_owned(), Value::String(config.alias.clone()));
    table.insert(
        "transport".to_owned(),
        Value::String(transport_key(&config.transport).to_owned()),
    );
    match &config.transport {
        McpTransportConfig::Stdio {
            command,
            arguments,
            environment,
            working_directory,
        } => {
            table.insert("command".to_owned(), Value::String(command.clone()));
            if !arguments.is_empty() {
                table.insert("args".to_owned(), string_array(arguments));
            }
            if !environment.is_empty() {
                table.insert("env".to_owned(), string_table(environment));
            }
            if let Some(working_directory) = working_directory {
                table.insert(
                    "cwd".to_owned(),
                    Value::String(working_directory.to_string_lossy().into_owned()),
                );
            }
        }
        McpTransportConfig::Http { url, headers }
        | McpTransportConfig::StreamableHttp { url, headers } => {
            table.insert("url".to_owned(), Value::String(url.to_string()));
            if let Some(auth) = auth_value(&config.auth, headers) {
                table.insert("auth".to_owned(), auth);
            }
        }
    }
    if !config.enabled {
        table.insert("disabled".to_owned(), Value::Boolean(true));
    }
    if !config.disabled_tools.is_empty() {
        table.insert(
            "disabled_tools".to_owned(),
            string_array(&config.disabled_tools.iter().cloned().collect::<Vec<_>>()),
        );
    }
    if let Some(prompt) = &config.prompt {
        table.insert("prompt".to_owned(), Value::String(prompt.clone()));
    }
    if !config.sampling_enabled {
        table.insert("sampling_enabled".to_owned(), Value::Boolean(false));
    }
    if config.startup_timeout_ms != DEFAULT_MCP_STARTUP_TIMEOUT_MS {
        table.insert(
            "startup_timeout_sec".to_owned(),
            seconds(config.startup_timeout_ms),
        );
    }
    if config.tool_timeout_ms != DEFAULT_MCP_TOOL_TIMEOUT_MS {
        table.insert(
            "tool_timeout_sec".to_owned(),
            seconds(config.tool_timeout_ms),
        );
    }
    Ok(table)
}

/// The `[auth]` block for a persisted entry, absent when nothing but defaults
/// would be written.
fn auth_value(auth: &McpAuthConfig, headers: &BTreeMap<String, String>) -> Option<Value> {
    let mut block = Table::new();
    match auth {
        McpAuthConfig::Static(statics) => {
            if headers.is_empty() && statics == &McpStaticAuth::default() {
                return None;
            }
            block.insert("type".to_owned(), Value::String("static".to_owned()));
            if !headers.is_empty() {
                block.insert("headers".to_owned(), string_table(headers));
            }
            if !statics.api_key_env.is_empty() {
                block.insert(
                    "api_key_env".to_owned(),
                    Value::String(statics.api_key_env.clone()),
                );
            }
            if statics.api_key_header != DEFAULT_MCP_API_KEY_HEADER {
                block.insert(
                    "api_key_header".to_owned(),
                    Value::String(statics.api_key_header.clone()),
                );
            }
            if statics.api_key_format != DEFAULT_MCP_API_KEY_FORMAT {
                block.insert(
                    "api_key_format".to_owned(),
                    Value::String(statics.api_key_format.clone()),
                );
            }
        }
        McpAuthConfig::Oauth(oauth) => {
            block.insert("type".to_owned(), Value::String("oauth".to_owned()));
            block.insert("scopes".to_owned(), string_array(&oauth.scopes));
            if let Some(client_id) = &oauth.client_id {
                block.insert("client_id".to_owned(), Value::String(client_id.clone()));
            }
            if let Some(client_metadata_url) = &oauth.client_metadata_url {
                block.insert(
                    "client_metadata_url".to_owned(),
                    Value::String(client_metadata_url.to_string()),
                );
            }
            if oauth.redirect_port != DEFAULT_MCP_OAUTH_REDIRECT_PORT {
                block.insert(
                    "redirect_port".to_owned(),
                    Value::Integer(i64::from(oauth.redirect_port)),
                );
            }
        }
    }
    Some(Value::Table(block))
}

fn transport_key(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Http { .. } => "http",
        McpTransportConfig::StreamableHttp { .. } => "streamable-http",
        McpTransportConfig::Stdio { .. } => "stdio",
    }
}

fn string_array(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

fn string_table(values: &BTreeMap<String, String>) -> Value {
    Value::Table(
        values
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

fn seconds(milliseconds: u64) -> Value {
    Value::Float(milliseconds as f64 / 1_000.0)
}

/// The URL an entry addresses, for the equivalence test.
pub(super) fn mcp_transport_url(transport: &McpTransportConfig) -> Option<&Url> {
    match transport {
        McpTransportConfig::Http { url, .. } | McpTransportConfig::StreamableHttp { url, .. } => {
            Some(url)
        }
        McpTransportConfig::Stdio { .. } => None,
    }
}

/// Whether `config` can join the servers `table` already declares.
///
/// Both rejections name the entry that stands in the way, which is what tells
/// an operator whether they added the same server twice under two spellings.
pub(super) fn preflight_mcp_add(
    table: &Table,
    config: &McpServerConfig,
) -> Result<(), ConfigError> {
    let requested_key =
        mcp_transport_url(&config.transport).map(|url| mcp_server_url_key(url.as_str()));
    for entry in config_array(table, IntegrationCollection::McpServers)? {
        let entry = entry
            .as_table()
            .ok_or_else(|| invalid("each mcp_servers entry must be a table"))?;
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(normalize_mcp_server_name);
        if name.as_deref() == Some(config.alias.as_str()) {
            return Err(invalid(format!(
                "MCP server name `{}` is already configured",
                config.alias
            )));
        }
        let Some(requested_key) = &requested_key else {
            continue;
        };
        let Some(existing_url) = entry.get("url").and_then(Value::as_str) else {
            continue;
        };
        if &mcp_server_url_key(existing_url) == requested_key {
            return Err(invalid(format!(
                "MCP server URL is already configured as `{}`",
                name.unwrap_or_else(|| FALLBACK_ALIAS.to_owned())
            )));
        }
    }
    Ok(())
}

/// What removing a server by name did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRemoval {
    /// The normalized name the removal was addressed to.
    pub name: String,
    /// False when no entry carried that name, which is not a failure.
    pub removed: bool,
}
