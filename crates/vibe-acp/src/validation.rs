//! Request validation in the shape the reference's router reports it.
//!
//! The reference validates every request against the ACP schema with pydantic
//! before its handler runs, and answers a failure with `Invalid params` and the
//! validator's error list under `data.errors`. That list is a client-visible
//! contract: an editor can point at the exact field. This module holds the
//! schema of the requests the agent routes, as the Agent Client Protocol
//! publishes it, and reproduces the validator's lax conversions, its error
//! vocabulary, its locations and its union reporting.

use serde_json::{Map, Value, json};

use crate::protocol::AcpError;

/// One accepted shape for a value.
pub(crate) enum Ty {
    Str,
    Int,
    Float,
    Bool,
    /// Anything at all, including a missing-but-required marker's input.
    Any,
    Dict,
    List(&'static Ty),
    Model(&'static Model),
    /// A plain union: the first member that validates wins, and a value that
    /// fits none reports every member's errors under the member's name.
    Union(&'static [&'static Model]),
    Literal(&'static str),
}

pub(crate) struct Field {
    alias: &'static str,
    required: bool,
    /// An optional field also accepts `null`.
    ty: Ty,
}

pub(crate) struct Model {
    name: &'static str,
    fields: &'static [Field],
}

const fn required(alias: &'static str, ty: Ty) -> Field {
    Field {
        alias,
        required: true,
        ty,
    }
}

const fn optional(alias: &'static str, ty: Ty) -> Field {
    Field {
        alias,
        required: false,
        ty,
    }
}

const META: Field = optional("_meta", Ty::Dict);
const STRINGS: Ty = Ty::Str;

static ANNOTATIONS: Model = Model {
    name: "Annotations",
    fields: &[
        optional("audience", Ty::List(&STRINGS)),
        optional("lastModified", Ty::Str),
        optional("priority", Ty::Float),
        META,
    ],
};

static TEXT_BLOCK: Model = Model {
    name: "TextContentBlock",
    fields: &[
        optional("annotations", Ty::Model(&ANNOTATIONS)),
        required("text", Ty::Str),
        META,
        required("type", Ty::Literal("text")),
    ],
};

static IMAGE_BLOCK: Model = Model {
    name: "ImageContentBlock",
    fields: &[
        optional("annotations", Ty::Model(&ANNOTATIONS)),
        required("data", Ty::Str),
        required("mimeType", Ty::Str),
        optional("uri", Ty::Str),
        META,
        required("type", Ty::Literal("image")),
    ],
};

static AUDIO_BLOCK: Model = Model {
    name: "AudioContentBlock",
    fields: &[
        optional("annotations", Ty::Model(&ANNOTATIONS)),
        required("data", Ty::Str),
        required("mimeType", Ty::Str),
        META,
        required("type", Ty::Literal("audio")),
    ],
};

static RESOURCE_LINK_BLOCK: Model = Model {
    name: "ResourceContentBlock",
    fields: &[
        optional("annotations", Ty::Model(&ANNOTATIONS)),
        optional("description", Ty::Str),
        optional("mimeType", Ty::Str),
        required("name", Ty::Str),
        optional("size", Ty::Int),
        optional("title", Ty::Str),
        required("uri", Ty::Str),
        META,
        required("type", Ty::Literal("resource_link")),
    ],
};

static TEXT_RESOURCE: Model = Model {
    name: "TextResourceContents",
    fields: &[
        optional("mimeType", Ty::Str),
        required("text", Ty::Str),
        required("uri", Ty::Str),
        META,
    ],
};

static BLOB_RESOURCE: Model = Model {
    name: "BlobResourceContents",
    fields: &[
        required("blob", Ty::Str),
        optional("mimeType", Ty::Str),
        required("uri", Ty::Str),
        META,
    ],
};

static EMBEDDED_RESOURCE_BLOCK: Model = Model {
    name: "EmbeddedResourceContentBlock",
    fields: &[
        optional("annotations", Ty::Model(&ANNOTATIONS)),
        required("resource", Ty::Union(&[&TEXT_RESOURCE, &BLOB_RESOURCE])),
        META,
        required("type", Ty::Literal("resource")),
    ],
};

static CONTENT_BLOCK: Ty = Ty::Union(&[
    &TEXT_BLOCK,
    &IMAGE_BLOCK,
    &AUDIO_BLOCK,
    &RESOURCE_LINK_BLOCK,
    &EMBEDDED_RESOURCE_BLOCK,
]);

static NAME_VALUE: Model = Model {
    name: "HttpHeader",
    fields: &[required("name", Ty::Str), required("value", Ty::Str), META],
};

static ENV_VARIABLE: Model = Model {
    name: "EnvVariable",
    fields: &[required("name", Ty::Str), required("value", Ty::Str), META],
};

static HEADER_LIST: Ty = Ty::Model(&NAME_VALUE);
static ENV_LIST: Ty = Ty::Model(&ENV_VARIABLE);

static HTTP_SERVER: Model = Model {
    name: "HttpMcpServer",
    fields: &[
        required("name", Ty::Str),
        required("url", Ty::Str),
        required("headers", Ty::List(&HEADER_LIST)),
        META,
        required("type", Ty::Literal("http")),
    ],
};

static SSE_SERVER: Model = Model {
    name: "SseMcpServer",
    fields: &[
        required("name", Ty::Str),
        required("url", Ty::Str),
        required("headers", Ty::List(&HEADER_LIST)),
        META,
        required("type", Ty::Literal("sse")),
    ],
};

static ACP_SERVER: Model = Model {
    name: "AcpMcpServer",
    fields: &[
        required("name", Ty::Str),
        required("id", Ty::Str),
        META,
        required("type", Ty::Literal("acp")),
    ],
};

static STDIO_SERVER: Model = Model {
    name: "McpServerStdio",
    fields: &[
        required("name", Ty::Str),
        required("command", Ty::Str),
        required("args", Ty::List(&STRINGS)),
        required("env", Ty::List(&ENV_LIST)),
        META,
    ],
};

static MCP_SERVER: Ty = Ty::Union(&[&HTTP_SERVER, &SSE_SERVER, &ACP_SERVER, &STDIO_SERVER]);

static FILE_SYSTEM: Model = Model {
    name: "FileSystemCapabilities",
    fields: &[
        optional("readTextFile", Ty::Bool),
        optional("writeTextFile", Ty::Bool),
        META,
    ],
};

static CLIENT_CAPABILITIES: Model = Model {
    name: "ClientCapabilities",
    fields: &[
        optional("fs", Ty::Model(&FILE_SYSTEM)),
        optional("terminal", Ty::Bool),
        optional("session", Ty::Any),
        optional("plan", Ty::Any),
        optional("auth", Ty::Any),
        optional("elicitation", Ty::Any),
        optional("nes", Ty::Any),
        optional("positionEncodings", Ty::List(&STRINGS)),
        META,
    ],
};

static IMPLEMENTATION: Model = Model {
    name: "Implementation",
    fields: &[
        required("name", Ty::Str),
        optional("title", Ty::Str),
        required("version", Ty::Str),
        META,
    ],
};

static INITIALIZE: Model = Model {
    name: "InitializeRequest",
    fields: &[
        // The schema coerces any value into a version it can answer with, so
        // only its absence is refused.
        required("protocolVersion", Ty::Any),
        optional("clientCapabilities", Ty::Model(&CLIENT_CAPABILITIES)),
        optional("clientInfo", Ty::Model(&IMPLEMENTATION)),
        META,
    ],
};

static AUTHENTICATE: Model = Model {
    name: "AuthenticateRequest",
    fields: &[required("methodId", Ty::Str), META],
};

static NEW_SESSION: Model = Model {
    name: "NewSessionRequest",
    fields: &[
        required("cwd", Ty::Str),
        optional("additionalDirectories", Ty::List(&STRINGS)),
        required("mcpServers", Ty::List(&MCP_SERVER)),
        META,
    ],
};

static LOAD_SESSION: Model = Model {
    name: "LoadSessionRequest",
    fields: &[
        required("mcpServers", Ty::List(&MCP_SERVER)),
        required("cwd", Ty::Str),
        optional("additionalDirectories", Ty::List(&STRINGS)),
        required("sessionId", Ty::Str),
        META,
    ],
};

static LIST_SESSIONS: Model = Model {
    name: "ListSessionsRequest",
    fields: &[optional("cwd", Ty::Str), optional("cursor", Ty::Str), META],
};

static SESSION_ONLY: Model = Model {
    name: "CloseSessionRequest",
    fields: &[required("sessionId", Ty::Str), META],
};

static SET_MODE: Model = Model {
    name: "SetSessionModeRequest",
    fields: &[
        required("sessionId", Ty::Str),
        required("modeId", Ty::Str),
        META,
    ],
};

static PROMPT: Model = Model {
    name: "PromptRequest",
    fields: &[
        required("sessionId", Ty::Str),
        required("prompt", Ty::List(&CONTENT_BLOCK)),
        META,
    ],
};

static SET_SELECT_OPTION: Model = Model {
    name: "SetSessionConfigOptionSelectRequest",
    fields: &[
        required("sessionId", Ty::Str),
        required("configId", Ty::Str),
        META,
        required("value", Ty::Str),
    ],
};

static SET_BOOLEAN_OPTION: Model = Model {
    name: "SetSessionConfigOptionBooleanRequest",
    fields: &[
        required("sessionId", Ty::Str),
        required("configId", Ty::Str),
        META,
        required("value", Ty::Bool),
        required("type", Ty::Literal("boolean")),
    ],
};

static FORK_OR_RESUME: Model = Model {
    name: "ForkSessionRequest",
    fields: &[
        required("sessionId", Ty::Str),
        required("cwd", Ty::Str),
        optional("additionalDirectories", Ty::List(&STRINGS)),
        optional("mcpServers", Ty::List(&MCP_SERVER)),
        META,
    ],
};

/// The schema one standard method validates its parameters against, or
/// `None` for a method the router does not serve.
fn request_model(method: &str, params: &Value) -> Option<&'static Model> {
    Some(match method {
        "initialize" => &INITIALIZE,
        "authenticate" => &AUTHENTICATE,
        "session/new" => &NEW_SESSION,
        "session/load" => &LOAD_SESSION,
        "session/list" => &LIST_SESSIONS,
        "session/close" | "session/cancel" => &SESSION_ONLY,
        "session/set_mode" => &SET_MODE,
        "session/prompt" => &PROMPT,
        "session/set_config_option" => {
            // The router picks the boolean form by its tag and every other
            // payload is read as a selection.
            if params.get("type").and_then(Value::as_str) == Some("boolean") {
                &SET_BOOLEAN_OPTION
            } else {
                &SET_SELECT_OPTION
            }
        }
        "session/fork" | "session/resume" => &FORK_OR_RESUME,
        _ => return None,
    })
}

/// Validates the parameters of a standard method, reporting every problem at
/// once the way the reference's validator does.
pub(crate) fn validate_request(method: &str, params: &Value) -> Result<(), AcpError> {
    let Some(model) = request_model(method, params) else {
        return Ok(());
    };
    let mut errors = Vec::new();
    validate_model(model, params, &[], &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AcpError::Validation(errors))
    }
}

fn validate_model(model: &Model, value: &Value, loc: &[Value], errors: &mut Vec<Value>) {
    let Some(object) = value.as_object() else {
        errors.push(error(
            "model_type",
            loc,
            &format!(
                "Input should be a valid dictionary or instance of {}",
                model.name
            ),
            value,
            Some(json!({"class_name": model.name})),
        ));
        return;
    };
    for field in model.fields {
        let mut field_loc = loc.to_vec();
        field_loc.push(json!(field.alias));
        match object.get(field.alias) {
            None if field.required => {
                errors.push(error("missing", &field_loc, "Field required", value, None));
            }
            None => {}
            Some(Value::Null) if !field.required => {}
            Some(item) => validate_value(&field.ty, item, &field_loc, errors),
        }
    }
}

fn validate_value(ty: &Ty, value: &Value, loc: &[Value], errors: &mut Vec<Value>) {
    match ty {
        Ty::Any => {}
        Ty::Str => {
            if !value.is_string() {
                errors.push(error(
                    "string_type",
                    loc,
                    "Input should be a valid string",
                    value,
                    None,
                ));
            }
        }
        Ty::Int => validate_int(value, loc, errors),
        Ty::Float => validate_float(value, loc, errors),
        Ty::Bool => validate_bool(value, loc, errors),
        Ty::Dict => {
            if !value.is_object() {
                errors.push(error(
                    "dict_type",
                    loc,
                    "Input should be a valid dictionary",
                    value,
                    None,
                ));
            }
        }
        Ty::List(item) => match value.as_array() {
            Some(items) => {
                for (index, element) in items.iter().enumerate() {
                    let mut item_loc = loc.to_vec();
                    item_loc.push(json!(index));
                    validate_value(item, element, &item_loc, errors);
                }
            }
            None => errors.push(error(
                "list_type",
                loc,
                "Input should be a valid list",
                value,
                None,
            )),
        },
        Ty::Model(model) => validate_model(model, value, loc, errors),
        Ty::Union(members) => {
            let mut collected = Vec::new();
            for member in *members {
                let mut member_errors = Vec::new();
                let mut member_loc = loc.to_vec();
                member_loc.push(json!(member.name));
                validate_model(member, value, &member_loc, &mut member_errors);
                if member_errors.is_empty() {
                    return;
                }
                collected.extend(member_errors);
            }
            errors.extend(collected);
        }
        Ty::Literal(expected) => {
            if value.as_str() != Some(*expected) {
                errors.push(error(
                    "literal_error",
                    loc,
                    &format!("Input should be '{expected}'"),
                    value,
                    Some(json!({"expected": format!("'{expected}'")})),
                ));
            }
        }
    }
}

/// The lax integer conversion: an integral number, a float without a
/// fractional part, a boolean, or a string that parses as one.
fn validate_int(value: &Value, loc: &[Value], errors: &mut Vec<Value>) {
    match value {
        Value::Bool(_) => {}
        Value::Number(number) => {
            if number.is_f64() && number.as_f64().is_some_and(|float| float.fract() != 0.0) {
                errors.push(error(
                    "int_from_float",
                    loc,
                    "Input should be a valid integer, got a number with a fractional part",
                    value,
                    None,
                ));
            }
        }
        Value::String(text) => {
            if text.trim().parse::<i64>().is_err() {
                errors.push(error(
                    "int_parsing",
                    loc,
                    "Input should be a valid integer, unable to parse string as an integer",
                    value,
                    None,
                ));
            }
        }
        _ => errors.push(error(
            "int_type",
            loc,
            "Input should be a valid integer",
            value,
            None,
        )),
    }
}

fn validate_float(value: &Value, loc: &[Value], errors: &mut Vec<Value>) {
    match value {
        Value::Bool(_) | Value::Number(_) => {}
        Value::String(text) => {
            if text.trim().parse::<f64>().is_err() {
                errors.push(error(
                    "float_parsing",
                    loc,
                    "Input should be a valid number, unable to parse string as a number",
                    value,
                    None,
                ));
            }
        }
        _ => errors.push(error(
            "float_type",
            loc,
            "Input should be a valid number",
            value,
            None,
        )),
    }
}

/// The lax boolean conversion the validator applies: the two integers and the
/// spellings it recognizes.
pub(crate) fn lax_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => match number.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
            _ => None,
        },
        _ => None,
    }
}

fn validate_bool(value: &Value, loc: &[Value], errors: &mut Vec<Value>) {
    if lax_bool(value).is_some() {
        return;
    }
    let (kind, message) = match value {
        Value::String(_) | Value::Number(_) => (
            "bool_parsing",
            "Input should be a valid boolean, unable to interpret input",
        ),
        _ => ("bool_type", "Input should be a valid boolean"),
    };
    errors.push(error(kind, loc, message, value, None));
}

fn error(kind: &str, loc: &[Value], message: &str, input: &Value, ctx: Option<Value>) -> Value {
    let mut fields = Map::new();
    fields.insert("type".to_owned(), json!(kind));
    fields.insert("loc".to_owned(), Value::Array(loc.to_vec()));
    fields.insert("msg".to_owned(), json!(message));
    fields.insert("input".to_owned(), input.clone());
    if let Some(ctx) = ctx {
        fields.insert("ctx".to_owned(), ctx);
    }
    fields.insert(
        "url".to_owned(),
        json!(format!("https://errors.pydantic.dev/2.13/v/{kind}")),
    );
    Value::Object(fields)
}

#[cfg(test)]
mod validation_tests;
