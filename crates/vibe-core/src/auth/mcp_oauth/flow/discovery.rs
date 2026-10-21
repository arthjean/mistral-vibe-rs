//! What the flow learns about the servers a login talks to.
//!
//! The challenge a `401` or a `403` carries, where the protected-resource and
//! authorization-server metadata are looked up and how each answer is
//! validated, the scope a client asks for, and which resource a server URL
//! names. The helpers of `mcp/client/auth/utils.py` and
//! `mcp/shared/auth_utils.py` (SDK 1.28.1), with Python's `urllib.parse`
//! behavior where the URLs they build depend on it.

use serde_json::Value;

use crate::auth::mcp_oauth::any_http_url;
use crate::pycompat::pydantic;
use crate::pyurl::PyUrl;

/// SDK `ProtectedResourceMetadata`, the fields the flow reads.
pub(super) struct ProtectedResource {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Option<Vec<String>>,
}

/// SDK `OAuthMetadata`, the fields the flow reads.
pub(super) struct AuthorizationServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Option<Vec<String>>,
    pub client_id_metadata_document_supported: Option<bool>,
}

/// `response.headers.get("WWW-Authenticate")`: every value, comma-joined.
pub(super) fn www_authenticate(response: &reqwest::Response) -> Option<String> {
    let values = response
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(", "))
}

/// `extract_field_from_www_auth`.
pub(super) fn extract_field(header: Option<&str>, field: &str) -> Option<String> {
    let header = header.filter(|header| !header.is_empty())?;
    let pattern = regex::Regex::new(&format!(
        r#"{}=(?:"([^"]+)"|([^\s,]+))"#,
        regex::escape(field)
    ))
    .ok()?;
    let captures = pattern.captures(header)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str().to_owned())
}

/// `build_protected_resource_metadata_discovery_urls`.
pub(super) fn protected_resource_metadata_urls(
    advertised: Option<&str>,
    server_url: &str,
) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(advertised) = advertised.filter(|url| !url.is_empty()) {
        urls.push(advertised.to_owned());
    }
    let parsed = PyUrl::parse(server_url);
    let base = format!("{}://{}", parsed.scheme, parsed.netloc);
    if !parsed.path.is_empty() && parsed.path != "/" {
        urls.push(format!(
            "{base}/.well-known/oauth-protected-resource{}",
            parsed.path
        ));
    }
    urls.push(format!("{base}/.well-known/oauth-protected-resource"));
    urls
}

/// `build_oauth_authorization_server_metadata_discovery_urls`.
pub(super) fn authorization_server_metadata_urls(
    auth_server_url: Option<&str>,
    server_url: &str,
) -> Vec<String> {
    let Some(auth_server_url) = auth_server_url.filter(|url| !url.is_empty()) else {
        let parsed = PyUrl::parse(server_url);
        return vec![format!(
            "{}://{}/.well-known/oauth-authorization-server",
            parsed.scheme, parsed.netloc
        )];
    };
    let parsed = PyUrl::parse(auth_server_url);
    let base = format!("{}://{}", parsed.scheme, parsed.netloc);
    if !parsed.path.is_empty() && parsed.path != "/" {
        let path = parsed.path.trim_end_matches('/');
        return vec![
            format!("{base}/.well-known/oauth-authorization-server{path}"),
            format!("{base}/.well-known/openid-configuration{path}"),
            format!("{base}{path}/.well-known/openid-configuration"),
        ];
    }
    vec![
        format!("{base}/.well-known/oauth-authorization-server"),
        format!("{base}/.well-known/openid-configuration"),
    ]
}

/// `get_client_metadata_scopes`.
pub(super) fn client_metadata_scopes(
    challenge: Option<String>,
    protected: Option<&ProtectedResource>,
    server: Option<&AuthorizationServer>,
) -> Option<String> {
    if challenge.is_some() {
        return challenge;
    }
    if let Some(scopes) = protected.and_then(|metadata| metadata.scopes_supported.as_ref()) {
        return Some(scopes.join(" "));
    }
    server
        .and_then(|metadata| metadata.scopes_supported.as_ref())
        .map(|scopes| scopes.join(" "))
}

/// `handle_protected_resource_response`.
pub(super) async fn protected_resource_response(
    response: reqwest::Response,
) -> Option<ProtectedResource> {
    if response.status().as_u16() != 200 {
        return None;
    }
    let body = response.bytes().await.ok()?;
    let value = serde_json::from_slice::<Value>(&body).ok()?;
    let fields = value.as_object()?;
    let resource = any_http_url(fields.get("resource")?.as_str()?)?;
    let authorization_servers = fields
        .get("authorization_servers")?
        .as_array()?
        .iter()
        .map(|server| server.as_str().and_then(any_http_url))
        .collect::<Option<Vec<_>>>()?;
    if authorization_servers.is_empty() {
        return None;
    }
    for name in [
        "jwks_uri",
        "resource_documentation",
        "resource_policy_uri",
        "resource_tos_uri",
    ] {
        optional_http_url(fields.get(name))?;
    }
    for name in [
        "bearer_methods_supported",
        "resource_signing_alg_values_supported",
        "authorization_details_types_supported",
        "dpop_signing_alg_values_supported",
    ] {
        optional_string_list(fields.get(name))?;
    }
    for name in [
        "tls_client_certificate_bound_access_tokens",
        "dpop_bound_access_tokens_required",
    ] {
        optional_bool(fields.get(name))?;
    }
    optional_text(fields.get("resource_name"))?;
    Some(ProtectedResource {
        resource,
        authorization_servers,
        scopes_supported: optional_string_list(fields.get("scopes_supported"))?,
    })
}

/// `handle_auth_metadata_response`: whether to keep looking, and what was
/// found.
pub(super) async fn authorization_server_response(
    response: reqwest::Response,
) -> (bool, Option<AuthorizationServer>) {
    let status = response.status().as_u16();
    if status == 200 {
        let metadata = response
            .bytes()
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
            .and_then(|value| authorization_server_metadata(&value));
        return (true, metadata);
    }
    if !(400..500).contains(&status) {
        return (false, None);
    }
    (true, None)
}

/// `OAuthMetadata.model_validate`.
fn authorization_server_metadata(value: &Value) -> Option<AuthorizationServer> {
    let fields = value.as_object()?;
    let required = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .and_then(any_http_url)
    };
    required("issuer")?;
    let authorization_endpoint = required("authorization_endpoint")?;
    let token_endpoint = required("token_endpoint")?;
    for name in [
        "service_documentation",
        "op_policy_uri",
        "op_tos_uri",
        "revocation_endpoint",
        "introspection_endpoint",
    ] {
        optional_http_url(fields.get(name))?;
    }
    for name in [
        "response_modes_supported",
        "grant_types_supported",
        "token_endpoint_auth_methods_supported",
        "token_endpoint_auth_signing_alg_values_supported",
        "ui_locales_supported",
        "revocation_endpoint_auth_methods_supported",
        "revocation_endpoint_auth_signing_alg_values_supported",
        "introspection_endpoint_auth_methods_supported",
        "introspection_endpoint_auth_signing_alg_values_supported",
        "code_challenge_methods_supported",
    ] {
        optional_string_list(fields.get(name))?;
    }
    if let Some(types) = fields.get("response_types_supported") {
        optional_string_list(Some(types))?.filter(|_| !types.is_null())?;
    }
    Some(AuthorizationServer {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: optional_http_url(fields.get("registration_endpoint"))?,
        scopes_supported: optional_string_list(fields.get("scopes_supported"))?,
        client_id_metadata_document_supported: optional_bool(
            fields.get("client_id_metadata_document_supported"),
        )?,
    })
}

/// An optional `AnyHttpUrl` field: the outer `None` is a validation failure.
fn optional_http_url(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => any_http_url(text).map(Some),
        Some(_) => None,
    }
}

fn optional_string_list(value: Option<&Value>) -> Option<Option<Vec<String>>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some),
        Some(_) => None,
    }
}

fn optional_text(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => None,
    }
}

/// Pydantic's lax `bool | None`.
fn optional_bool(value: Option<&Value>) -> Option<Option<bool>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::Bool(flag)) => Some(Some(*flag)),
        Some(Value::Number(number)) => match number.as_f64() {
            Some(0.0) => Some(Some(false)),
            Some(1.0) => Some(Some(true)),
            _ => None,
        },
        Some(Value::String(text)) => pydantic::lax_bool(text).map(Some),
        Some(_) => None,
    }
}

/// `is_valid_client_metadata_url`.
pub(crate) fn is_valid_client_metadata_url(url: &str) -> bool {
    let parsed = PyUrl::parse(url);
    parsed.scheme == "https" && !parsed.path.is_empty() && parsed.path != "/"
}

/// `resource_url_from_server_url`.
pub(super) fn resource_url_from_server_url(url: &str) -> String {
    let parsed = PyUrl::split(url);
    PyUrl {
        netloc: parsed.netloc.to_lowercase(),
        fragment: String::new(),
        ..parsed
    }
    .unsplit()
}

/// `check_resource_allowed`.
pub(super) fn check_resource_allowed(requested: &str, configured: &str) -> bool {
    let requested = PyUrl::parse(requested);
    let configured = PyUrl::parse(configured);
    if requested.scheme.to_lowercase() != configured.scheme.to_lowercase()
        || requested.netloc.to_lowercase() != configured.netloc.to_lowercase()
    {
        return false;
    }
    let with_slash = |path: &str| {
        if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        }
    };
    with_slash(&requested.path).starts_with(&with_slash(&configured.path))
}
