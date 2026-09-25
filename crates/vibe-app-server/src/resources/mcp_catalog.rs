//! The MCP catalog: the configured MCP servers as one app-wide resource.
//!
//! Reference `MCPCatalogService` (`vibe/app_server/mcp_catalog.py`) serves
//! `mcp_catalog/{read,refresh,toggle,add,remove,login,logout}` and the six
//! `mcp/*` aliases the older clients call. The configuration is the record: a
//! mutation is persisted first and the session it targets is then converged on
//! what the configuration says. A mutation may also name no session at all,
//! and then it edits the configuration a sessionless caller reads, which is
//! only allowed while no session is attached to the connection.
//!
//! This module owns what is decided before a backend is reached: which method
//! a name means, whether the parameters validate, and which catalog a call
//! targets. The backend does the work.

use std::sync::Arc;

use serde_json::{Map, Value};
use vibe_protocol::{PathSegment, ProtocolErrorCode};

/// The methods the catalog answers, canonical names first.
pub const MCP_CATALOG_METHODS: &[&str] = &[
    "mcp_catalog/read",
    "mcp_catalog/refresh",
    "mcp_catalog/toggle",
    "mcp_catalog/add",
    "mcp_catalog/remove",
    "mcp_catalog/login",
    "mcp_catalog/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "mcp/add",
    "mcp/login",
    "mcp/logout",
];

/// Reference `MCPCatalogService.handles`: every alias, and every name under
/// `mcp_catalog/`, including one the catalog then refuses as not found.
#[must_use]
pub fn handles(method: &str) -> bool {
    method.starts_with("mcp_catalog/") || canonical(method).is_some_and(|name| name != method)
}

fn canonical(method: &str) -> Option<&'static str> {
    Some(match method {
        "mcp/read" | "mcp_catalog/read" => "mcp_catalog/read",
        "mcp/refresh" | "mcp_catalog/refresh" => "mcp_catalog/refresh",
        "mcp/toggle" | "mcp_catalog/toggle" => "mcp_catalog/toggle",
        "mcp/add" | "mcp_catalog/add" => "mcp_catalog/add",
        "mcp_catalog/remove" => "mcp_catalog/remove",
        "mcp/login" | "mcp_catalog/login" => "mcp_catalog/login",
        "mcp/logout" | "mcp_catalog/logout" => "mcp_catalog/logout",
        _ => return None,
    })
}

/// One catalog call with its parameters validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCatalogCall {
    Read {
        session_id: String,
    },
    Refresh {
        session_id: String,
    },
    Toggle {
        session_id: Option<String>,
        name: String,
        disabled: bool,
        tool_name: Option<String>,
    },
    Add {
        session_id: Option<String>,
        url: String,
        name: Option<String>,
        scopes: Vec<String>,
        /// The legacy `http` transport rather than `streamable-http`.
        legacy_http: bool,
        allow_insecure_http: bool,
    },
    Remove {
        session_id: Option<String>,
        name: String,
    },
    Login {
        session_id: Option<String>,
        name: String,
    },
    Logout {
        session_id: Option<String>,
        name: String,
    },
}

impl McpCatalogCall {
    /// Whether the answer declares a `runtime`, which is every call but a read.
    #[must_use]
    pub const fn answers_runtime(&self) -> bool {
        !matches!(self, Self::Read { .. })
    }

    fn session_id(&self) -> Option<&str> {
        match self {
            Self::Read { session_id } | Self::Refresh { session_id } => Some(session_id),
            Self::Toggle { session_id, .. }
            | Self::Add { session_id, .. }
            | Self::Remove { session_id, .. }
            | Self::Login { session_id, .. }
            | Self::Logout { session_id, .. } => session_id.as_deref(),
        }
    }
}

/// Which configuration a call acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCatalogTarget {
    /// The session's own configuration, which the session is converged on.
    Session(String),
    /// The configuration a sessionless caller reads, with no session to
    /// converge.
    Sessionless,
}

/// Why a catalog call was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum McpCatalogError {
    /// The parameters did not validate: every violation, each at its path.
    Params(Vec<McpCatalogIssue>),
    /// A refusal carrying no structured detail.
    Refused {
        code: ProtocolErrorCode,
        message: String,
    },
}

impl McpCatalogError {
    pub(crate) fn refused(code: ProtocolErrorCode, message: impl Into<String>) -> Self {
        Self::Refused {
            code,
            message: message.into(),
        }
    }
}

/// One parameter violation.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCatalogIssue {
    pub path: Vec<PathSegment>,
    pub message: String,
}

/// Where the catalog publishes a notification while a call is still running,
/// which is how a sign-in hands out its authorization URL before it answers.
pub type McpCatalogNotify = Arc<dyn Fn(&str, Map<String, Value>) + Send + Sync>;

/// What a call that went through answers.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCatalogOutcome {
    /// The answer, less the `runtime` the server composes.
    pub result: std::collections::BTreeMap<String, Value>,
    /// The session state moved, so the answer carries the runtime and a
    /// `runtime/updated` follows it.
    pub runtime_updated: bool,
    /// The session's integration state after the call.
    pub integrations: Option<super::IntegrationState>,
    /// The `mcp_catalog/authRequired` parameters the call's convergence
    /// raised and the catalog accepted, each followed by a `runtime/updated`.
    pub auth_required: Vec<Map<String, Value>>,
}

/// Validates `params` for `method`.
///
/// Reference `validate_wire` over the method's params model: every model
/// forbids fields it does not declare and reports every violation, each at
/// the path of the value that caused it.
pub fn parse(method: &str, params: &Map<String, Value>) -> Result<McpCatalogCall, McpCatalogError> {
    let Some(canonical) = canonical(method) else {
        return Err(McpCatalogError::refused(
            ProtocolErrorCode::MethodNotFound,
            format!("Method not found: {method}"),
        ));
    };
    let mut fields = Fields::new(params);
    let call = match canonical {
        "mcp_catalog/read" => {
            let session_id = fields.required_string("sessionId");
            fields.finish()?;
            McpCatalogCall::Read {
                session_id: session_id.unwrap_or_default(),
            }
        }
        "mcp_catalog/refresh" => {
            let session_id = fields.required_string("sessionId");
            fields.finish()?;
            McpCatalogCall::Refresh {
                session_id: session_id.unwrap_or_default(),
            }
        }
        "mcp_catalog/toggle" => {
            let session_id = fields.optional_string("sessionId");
            let name = fields.required_string("name");
            let source = fields.required_choice("source", &["server", "connector"]);
            let disabled = fields.required_bool("disabled");
            let tool_name = fields.optional_string("toolName");
            fields.finish()?;
            if source.as_deref() == Some("connector") {
                return Err(McpCatalogError::refused(
                    ProtocolErrorCode::NotImplemented,
                    "Connector sources are toggled through the connector catalog, not the MCP catalog",
                ));
            }
            McpCatalogCall::Toggle {
                session_id,
                name: name.unwrap_or_default(),
                disabled: disabled.unwrap_or_default(),
                tool_name,
            }
        }
        "mcp_catalog/add" => {
            let session_id = fields.optional_string("sessionId");
            let url = fields.required_string("url");
            let name = fields.optional_string("name");
            let scopes = fields.string_list("scopes");
            let transport = fields.optional_choice("transport", &["http", "streamable-http"]);
            let allow_insecure_http = fields.optional_bool("allowInsecureHttp");
            fields.finish()?;
            McpCatalogCall::Add {
                session_id,
                url: url.unwrap_or_default(),
                name,
                scopes,
                legacy_http: transport.as_deref() == Some("http"),
                allow_insecure_http: allow_insecure_http.unwrap_or(false),
            }
        }
        "mcp_catalog/remove" | "mcp_catalog/login" | "mcp_catalog/logout" => {
            let session_id = fields.optional_string("sessionId");
            let name = fields.required_string("name").unwrap_or_default();
            fields.finish()?;
            match canonical {
                "mcp_catalog/remove" => McpCatalogCall::Remove { session_id, name },
                "mcp_catalog/login" => McpCatalogCall::Login { session_id, name },
                _ => McpCatalogCall::Logout { session_id, name },
            }
        }
        _ => {
            return Err(McpCatalogError::refused(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {method}"),
            ));
        }
    };
    Ok(call)
}

/// Reference `_target` and `_mutation_target`: which catalog `call` acts on,
/// given the sessions attached to the connection.
///
/// A read or a refresh names its session, and a mutation that names one is
/// held to it too; a mutation that names none edits the sessionless
/// configuration, which a connection with a session attached may not do
/// implicitly.
pub fn target(
    call: &McpCatalogCall,
    attached: &dyn Fn(&str) -> bool,
    any_attached: bool,
) -> Result<McpCatalogTarget, McpCatalogError> {
    match call.session_id() {
        Some(session_id) if attached(session_id) => {
            Ok(McpCatalogTarget::Session(session_id.to_owned()))
        }
        Some(session_id) => Err(McpCatalogError::refused(
            ProtocolErrorCode::NotFound,
            format!("Session not found: {session_id}"),
        )),
        None if any_attached => Err(McpCatalogError::refused(
            ProtocolErrorCode::Conflict,
            "A catalog change naming no session cannot apply to the session attached here; \
             name the session",
        )),
        None => Ok(McpCatalogTarget::Sessionless),
    }
}

/// The validator one params model runs.
struct Fields<'a> {
    params: &'a Map<String, Value>,
    declared: Vec<&'static str>,
    issues: Vec<McpCatalogIssue>,
}

impl<'a> Fields<'a> {
    fn new(params: &'a Map<String, Value>) -> Self {
        Self {
            params,
            declared: Vec::new(),
            issues: Vec::new(),
        }
    }

    fn issue(&mut self, key: &str, message: &str) {
        self.issues.push(McpCatalogIssue {
            path: vec![PathSegment::Field(key.to_owned())],
            message: message.to_owned(),
        });
    }

    fn value(&mut self, key: &'static str) -> Option<&'a Value> {
        self.declared.push(key);
        self.params.get(key)
    }

    fn required_string(&mut self, key: &'static str) -> Option<String> {
        match self.value(key) {
            None => {
                self.issue(key, "This field is required");
                None
            }
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => {
                self.issue(key, "Expected a string");
                None
            }
        }
    }

    fn optional_string(&mut self, key: &'static str) -> Option<String> {
        match self.value(key) {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => {
                self.issue(key, "Expected a string or null");
                None
            }
        }
    }

    fn required_bool(&mut self, key: &'static str) -> Option<bool> {
        match self.value(key) {
            None => {
                self.issue(key, "This field is required");
                None
            }
            Some(Value::Bool(flag)) => Some(*flag),
            Some(_) => {
                self.issue(key, "Expected a boolean");
                None
            }
        }
    }

    fn optional_bool(&mut self, key: &'static str) -> Option<bool> {
        match self.value(key) {
            None => None,
            Some(Value::Bool(flag)) => Some(*flag),
            Some(_) => {
                self.issue(key, "Expected a boolean");
                None
            }
        }
    }

    fn choice(&mut self, key: &'static str, value: &Value, choices: &[&str]) -> Option<String> {
        match value.as_str().filter(|text| choices.contains(text)) {
            Some(text) => Some(text.to_owned()),
            None => {
                let listed = choices
                    .iter()
                    .map(|choice| format!("`{choice}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.issue(key, &format!("Expected one of {listed}"));
                None
            }
        }
    }

    fn required_choice(&mut self, key: &'static str, choices: &[&str]) -> Option<String> {
        match self.value(key) {
            None => {
                self.issue(key, "This field is required");
                None
            }
            Some(value) => self.choice(key, value, choices),
        }
    }

    fn optional_choice(&mut self, key: &'static str, choices: &[&str]) -> Option<String> {
        match self.value(key) {
            None => None,
            Some(value) => self.choice(key, value, choices),
        }
    }

    fn string_list(&mut self, key: &'static str) -> Vec<String> {
        match self.value(key) {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                let mut strings = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(text) => strings.push(text.clone()),
                        _ => self.issues.push(McpCatalogIssue {
                            path: vec![
                                PathSegment::Field(key.to_owned()),
                                PathSegment::Index(index),
                            ],
                            message: "Expected a string".to_owned(),
                        }),
                    }
                }
                strings
            }
            Some(_) => {
                self.issue(key, "Expected a list");
                Vec::new()
            }
        }
    }

    /// Adds a violation for every field the model does not declare, after the
    /// declared ones, as the reference orders them.
    fn finish(mut self) -> Result<(), McpCatalogError> {
        let extras = self
            .params
            .keys()
            .filter(|key| !self.declared.contains(&key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for key in extras {
            self.issue(&key, "This field is not accepted here");
        }
        if self.issues.is_empty() {
            Ok(())
        } else {
            Err(McpCatalogError::Params(self.issues))
        }
    }
}
