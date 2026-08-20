//! The `web_search` tool: a query answered by the configured search backend.
//!
//! The backend is reached with the session's own credentials, so the tool is
//! published only where those resolve. The response carries sources alongside
//! the prose, and both travel to the model.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret as _;
use serde_json::{Value, json};

use super::{SEARCH_USER_AGENT, WebSearchAccess, declared_document};
use crate::schema::{ObjectSchema, Property};
use crate::tools::config::{ToolConfigResolver, WebSearchConfig};
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolSource, ToolSpec, reference_text,
};

/// Directive coverage for `web_search`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool answers a query from live web results | "Answer a question from live web results" |
/// | It is reached for when the answer may have changed since training | "reach for it when the answer may have moved since training" |
/// | The answer comes back with its sources | "The answer comes back with the pages it rests on" |
pub(super) fn web_search_spec() -> ToolSpec {
    ToolSpec {
        name: "web_search".to_owned(),
        description: "Answer a question from live web results: reach for it when the answer may \
                      have moved since training rather than guessing. The answer comes back with \
                      the pages it rests on."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "query",
                Property::string()
                    .constrained("minLength", 1)
                    .described("The search query"),
            )
            .build(),
        output_schema: None,
        config: declared_document("web_search"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

pub(super) fn web_search_handler(
    access: WebSearchAccess,
    config: ToolConfigResolver,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let access = access.clone();
            let settings: WebSearchConfig = config.view("web_search");
            let query = invocation.arguments["query"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { run_web_search(&access, &settings, &query).await })
        },
    )
}

pub(super) async fn run_web_search(
    access: &WebSearchAccess,
    settings: &WebSearchConfig,
    query: &str,
) -> Result<ToolExecutionOutput, ToolError> {
    if query.trim().is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/query".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    let endpoint = format!("{}/v1/conversations", access.endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(settings.timeout))
        .build()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let response = client
        .post(&endpoint)
        .bearer_auth(access.api_key.expose_secret())
        // The reference sends the SDK's own user agent for this call and tags
        // the request as a secondary one, which is how the endpoint tells a
        // tool-issued search from a turn. The product name stays this port's:
        // the prefix is what the endpoint routes on, not the identity.
        .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
        .json(&json!({
            "model": settings.model,
            "instructions": "Always use the web_search tool to answer the query. Never answer \
                             from memory alone.",
            "tools": [{"type": "web_search"}],
            "inputs": query,
            "store": false,
            "metadata": search_request_metadata(),
        }))
        .send()
        .await
        .map_err(|_| ToolError::Execution("the web search request failed".to_owned()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "the web search endpoint returned HTTP {}",
            status.as_u16()
        )));
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| ToolError::Execution("the web search response is not JSON".to_owned()))?;
    let (answer, sources) = parse_search_response(&payload);
    if answer.is_empty() {
        return Err(ToolError::Execution(
            "the web search response carries no text".to_owned(),
        ));
    }
    // `WebSearchResult` declares `query`, `answer` and `sources`, and the agent
    // loop renders one field per line from it. `sources` is a list of models,
    // so it reaches the model as Python's repr of a list of dictionaries, empty
    // list included.
    let rendered_sources = sources
        .iter()
        .map(|source| {
            vec![
                (
                    "title",
                    reference_text::string_repr(source["title"].as_str().unwrap_or_default()),
                ),
                (
                    "url",
                    reference_text::string_repr(source["url"].as_str().unwrap_or_default()),
                ),
            ]
        })
        .collect::<Vec<_>>();
    let model_text = reference_text::joined(&[
        ("query", query.to_owned()),
        ("answer", answer.clone()),
        (
            "sources",
            reference_text::dictionary_list(&rendered_sources),
        ),
    ]);
    Ok(ToolExecutionOutput::new(model_text)
        .displayed_as(json!({"kind": "webSearch", "query": query}))
        .typed(json!({"query": query, "answer": answer, "sources": sources})))
}

/// The request metadata reference `build_request_metadata` attaches, with the
/// fields this port can answer for.
///
/// `exclude_none` upstream means an absent field is left out rather than sent
/// as null, so the same fields are omitted here.
pub(super) fn search_request_metadata() -> Value {
    json!({
        "os": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        "call_type": "secondary_call",
    })
}

/// Pulls the answer text and the cited pages out of a conversations response.
///
/// `content` is a plain string for a short answer and a chunk list otherwise,
/// and the citations arrive as `tool_reference` chunks carrying a URL.
pub(super) fn parse_search_response(payload: &Value) -> (String, Vec<Value>) {
    let mut answer = String::new();
    let mut sources = Vec::new();
    let mut seen = BTreeSet::new();
    let outputs = payload["outputs"].as_array().cloned().unwrap_or_default();
    for entry in outputs {
        match &entry["content"] {
            Value::String(text) => answer.push_str(text),
            Value::Array(chunks) => {
                for chunk in chunks {
                    match chunk["type"].as_str() {
                        Some("text") => {
                            answer.push_str(chunk["text"].as_str().unwrap_or_default());
                        }
                        Some("tool_reference") => {
                            let Some(url) = chunk["url"].as_str() else {
                                continue;
                            };
                            if seen.insert(url.to_owned()) {
                                sources.push(json!({
                                    "title": chunk["title"].as_str().unwrap_or(url),
                                    "url": url,
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (answer.trim().to_owned(), sources)
}
