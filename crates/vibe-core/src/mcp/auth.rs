//! How a remote MCP server is authenticated.
//!
//! Two shapes: a static credential the operator supplies, rendered into a
//! header whose name and format they also control, and an OAuth client whose
//! token the session obtains and refreshes. The header format is a template
//! with Python's `str.format` spelling, because that is what an operator's
//! existing configuration carries, so the small subset of that spelling the
//! field can use is reproduced here rather than approximated.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use url::Url;

/// The authentication an MCP entry declares, discriminated by `type` as the
/// persisted `[auth]` block is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpAuthConfig {
    Static(McpStaticAuth),
    Oauth(McpOAuthConfig),
}

impl Default for McpAuthConfig {
    fn default() -> Self {
        Self::Static(McpStaticAuth::default())
    }
}

impl McpAuthConfig {
    /// The OAuth block when the entry declares one.
    #[must_use]
    pub const fn oauth(&self) -> Option<&McpOAuthConfig> {
        match self {
            Self::Oauth(oauth) => Some(oauth),
            Self::Static(_) => None,
        }
    }
}

/// A token carried in a header whose value is read from the environment when
/// the request is made, never from a persisted value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStaticAuth {
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default = "default_api_key_header")]
    pub api_key_header: String,
    #[serde(default = "default_api_key_format")]
    pub api_key_format: String,
}

impl Default for McpStaticAuth {
    fn default() -> Self {
        Self {
            api_key_env: String::new(),
            api_key_header: default_api_key_header(),
            api_key_format: default_api_key_format(),
        }
    }
}

pub const DEFAULT_MCP_API_KEY_HEADER: &str = "Authorization";
pub const DEFAULT_MCP_API_KEY_FORMAT: &str = "Bearer {token}";
/// The placeholder `api_key_format` must reference, and only it.
pub const MCP_TOKEN_PLACEHOLDER: &str = "{token}";

fn default_api_key_header() -> String {
    DEFAULT_MCP_API_KEY_HEADER.to_owned()
}

fn default_api_key_format() -> String {
    DEFAULT_MCP_API_KEY_FORMAT.to_owned()
}

impl McpStaticAuth {
    /// The token header this block contributes, read from the environment.
    ///
    /// Returns nothing when no variable is declared, when it is unset or empty,
    /// or when `declared` already carries a header of the same name, compared
    /// case-insensitively as HTTP compares header names.
    #[must_use]
    pub fn token_header(
        &self,
        declared: &BTreeMap<String, String>,
        variable: impl Fn(&str) -> Option<String>,
    ) -> Option<(String, String)> {
        if self.api_key_env.is_empty()
            || declared
                .keys()
                .any(|name| name.eq_ignore_ascii_case(&self.api_key_header))
        {
            return None;
        }
        let token = variable(&self.api_key_env).filter(|token| !token.is_empty())?;
        Some((
            self.api_key_header.clone(),
            render_token_format(&self.api_key_format, &token),
        ))
    }
}

/// Substitutes `token` into `format`, the way the format string the reference
/// validates is rendered.
///
/// Doubled braces are literal, and a replacement field may carry a format spec:
/// the field is validated to name the token and nothing else, so only its fill,
/// alignment, width and precision change the rendered value.
fn render_token_format(format: &str, token: &str) -> String {
    let mut rendered = String::with_capacity(format.len() + token.len());
    let mut characters = format.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                rendered.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                rendered.push('}');
            }
            '{' => {
                let mut field = String::new();
                for character in characters.by_ref() {
                    if character == '}' {
                        break;
                    }
                    field.push(character);
                }
                let specification = field.split_once(':').map(|(_, spec)| spec).unwrap_or("");
                rendered.push_str(&apply_string_format(token, specification));
            }
            character => rendered.push(character),
        }
    }
    rendered
}

/// Applies the part of a format specification that is meaningful for a string.
fn apply_string_format(value: &str, specification: &str) -> String {
    if specification.is_empty() {
        return value.to_owned();
    }
    let characters = specification.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut fill = ' ';
    let mut align = '<';
    if characters.len() >= 2 && matches!(characters[1], '<' | '>' | '^' | '=') {
        fill = characters[0];
        align = characters[1];
        index = 2;
    } else if characters
        .first()
        .is_some_and(|first| matches!(*first, '<' | '>' | '^' | '='))
    {
        align = characters[0];
        index = 1;
    }
    let mut width = String::new();
    while characters.get(index).is_some_and(char::is_ascii_digit) {
        width.push(characters[index]);
        index = index.saturating_add(1);
    }
    let mut precision = String::new();
    if characters.get(index) == Some(&'.') {
        index = index.saturating_add(1);
        while characters.get(index).is_some_and(char::is_ascii_digit) {
            precision.push(characters[index]);
            index = index.saturating_add(1);
        }
    }
    let truncated = match precision.parse::<usize>() {
        Ok(precision) => value.chars().take(precision).collect::<String>(),
        Err(_) => value.to_owned(),
    };
    let width = width.parse::<usize>().unwrap_or(0);
    let padding = width.saturating_sub(truncated.chars().count());
    if padding == 0 {
        return truncated;
    }
    match align {
        '>' | '=' => format!("{}{truncated}", fill.to_string().repeat(padding)),
        '^' => {
            let left = padding / 2;
            format!(
                "{}{truncated}{}",
                fill.to_string().repeat(left),
                fill.to_string().repeat(padding.saturating_sub(left))
            )
        }
        _ => format!("{truncated}{}", fill.to_string().repeat(padding)),
    }
}

/// The loopback port the reference binds its OAuth callback handler to.
pub const DEFAULT_MCP_OAUTH_REDIRECT_PORT: u16 = 47823;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    /// Scopes to request. Empty accepts whatever the authorization server
    /// advertises.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// A pre-registered public client, which skips dynamic registration.
    #[serde(default)]
    pub client_id: Option<String>,
    /// A client-metadata document URL, used as the client identifier when the
    /// authorization server advertises support for it.
    #[serde(default)]
    pub client_metadata_url: Option<Url>,
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

const fn default_redirect_port() -> u16 {
    DEFAULT_MCP_OAUTH_REDIRECT_PORT
}

impl Default for McpOAuthConfig {
    fn default() -> Self {
        Self {
            scopes: Vec::new(),
            client_id: None,
            client_metadata_url: None,
            redirect_port: DEFAULT_MCP_OAUTH_REDIRECT_PORT,
        }
    }
}
