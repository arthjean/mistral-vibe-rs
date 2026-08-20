//! The `web_fetch` tool: one URL, rendered as text.
//!
//! Redirects are followed to a bound, because a redirect chain is a way to
//! reach a host the operator never approved. What comes back is reduced to text
//! by a small HTML reader rather than a parser dependency: the need is the
//! visible prose, and script, style and markup are what stands between the
//! model and it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

use super::{MAX_FETCH_REDIRECTS, declared_document};
use crate::schema::{ObjectSchema, Property};
use crate::tools::config::{ToolConfigResolver, WebFetchConfig};
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolSource, ToolSpec, reference_text,
};

/// Directive coverage for `web_fetch`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool retrieves the content of one URL | "Retrieve one web page" |
/// | HTML is converted to text before the model sees it | "HTML arrives as text" |
/// | Long pages are truncated | "a long page is truncated" |
/// | The timeout is optional and capped | the `timeout` description, "at most 120" |
pub(super) fn web_fetch_spec() -> ToolSpec {
    ToolSpec {
        name: "web_fetch".to_owned(),
        description: "Retrieve one web page over http or https. HTML arrives as text with the \
                      markup stripped, and a long page is truncated rather than flooding the \
                      conversation."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "url",
                Property::string().described("The URL whose content is retrieved"),
            )
            .optional(
                "timeout",
                Property::integer()
                    .described("How long to wait, in seconds, at most 120")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document("web_fetch"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

/// The target of a `web_fetch` call, refused before any network access when it
/// is empty or carries a scheme other than http.
pub(super) fn fetch_url(arguments: &Value) -> Result<Url, ToolError> {
    let raw = arguments["url"].as_str().unwrap_or_default().trim();
    if raw.is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/url".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    // A URL that already carries a scheme is judged on it. Anything else is a
    // protocol-relative or bare host, which the reference normalizes to https
    // rather than refusing.
    if let Ok(url) = Url::parse(raw) {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ToolError::Execution(format!(
                "`{}` is not an http or https scheme",
                url.scheme()
            )));
        }
        return Ok(url);
    }
    Url::parse(&format!("https://{}", raw.trim_start_matches('/')))
        .map_err(|error| ToolError::Execution(format!("`{raw}` is not a URL: {error}")))
}

/// How long one call may wait: what it asked for, bounded by the configured
/// ceiling, or the configured default when it asked for nothing.
pub(super) fn fetch_timeout(
    arguments: &Value,
    settings: &WebFetchConfig,
) -> Result<Duration, ToolError> {
    let Some(requested) = arguments["timeout"].as_i64() else {
        return Ok(Duration::from_secs(settings.default_timeout));
    };
    if requested <= 0 {
        return Err(ToolError::SchemaViolation {
            path: "/timeout".to_owned(),
            message: "must be a positive number of seconds".to_owned(),
        });
    }
    let seconds = u64::try_from(requested).unwrap_or(settings.max_timeout);
    Ok(Duration::from_secs(seconds.min(settings.max_timeout)))
}

pub(super) fn web_fetch_handler(config: ToolConfigResolver) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let arguments = invocation.arguments.clone();
            let settings: WebFetchConfig = config.view("web_fetch");
            Box::pin(async move { run_web_fetch(&arguments, &settings, &output).await })
        },
    )
}

pub(super) async fn run_web_fetch(
    arguments: &Value,
    settings: &WebFetchConfig,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let url = fetch_url(arguments)?;
    let timeout = fetch_timeout(arguments, settings)?;
    let host = url.host_str().unwrap_or("the requested host").to_owned();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_FETCH_REDIRECTS))
        .timeout(timeout)
        .user_agent(settings.user_agent.clone())
        .build()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let response = client.get(url.clone()).send().await.map_err(|error| {
        // A URL can carry credentials or a query string, so the failure names
        // the host and nothing else.
        if error.is_timeout() {
            ToolError::Execution(format!(
                "fetching from {host} timed out after {} seconds",
                timeout.as_secs()
            ))
        } else if error.is_redirect() {
            ToolError::Execution(format!(
                "fetching from {host} exceeded {MAX_FETCH_REDIRECTS} redirects"
            ))
        } else {
            ToolError::Execution(format!("fetching from {host} failed"))
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "fetching from {host} returned HTTP {}",
            status.as_u16()
        )));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_owned();
    let body = response
        .text()
        .await
        .map_err(|_| ToolError::Execution(format!("the body from {host} is not text")))?;
    let text = if content_type.contains("html") {
        html_to_text(&body)
    } else {
        body
    };
    // The sink owns the turn's output budget, so the page is bounded by the
    // smaller of its own limit and what the sink still has room for.
    let limit = settings.max_content_bytes.min(output.remaining_bytes());
    let truncated = text.len() > limit;
    let content = if truncated {
        let mut boundary = limit;
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!(
            "{}\n\n[content truncated at {boundary} bytes]",
            &text[..boundary]
        )
    } else {
        text
    };
    // `WebFetchResult` declares `url`, `content`, `content_type` and
    // `was_truncated` in that order, and the agent loop renders one field per
    // line from it, so both the typed result and the text the model reads
    // follow the declaration rather than the body alone.
    let model_text = reference_text::joined(&[
        ("url", url.as_str().to_owned()),
        ("content", content.clone()),
        ("content_type", content_type.clone()),
        (
            "was_truncated",
            reference_text::boolean(truncated).to_owned(),
        ),
    ]);
    Ok(ToolExecutionOutput::new(model_text)
        .displayed_as(json!({"kind": "webFetch", "url": url.as_str()}))
        .typed(json!({
            "url": url.as_str(),
            "content": content,
            "content_type": content_type,
            "was_truncated": truncated,
        })))
}

/// Strips markup so an HTML page reaches the model as prose.
///
/// The reference runs `markdownify`; there is no equivalent in this workspace
/// and the non-goals exclude execution-trace parity, so this drops the elements
/// that carry no prose and then the tags, which is what makes the body
/// readable.
pub(super) fn html_to_text(html: &str) -> String {
    let without_blocks = ["script", "style", "noscript", "iframe", "svg"]
        .into_iter()
        .fold(html.to_owned(), |document, tag| {
            drop_element(&document, tag)
        });
    let mut text = String::with_capacity(without_blocks.len());
    let mut tag = String::new();
    let mut inside_tag = false;
    for character in without_blocks.chars() {
        match character {
            '<' => {
                inside_tag = true;
                tag.clear();
            }
            '>' if inside_tag => {
                inside_tag = false;
                // A block-level tag ends a line; an inline one only separates
                // words, so `a<b>bold</b>c` does not become three lines.
                text.push(if is_block_tag(&tag) { '\n' } else { ' ' });
            }
            _ if inside_tag => tag.push(character),
            _ => text.push(character),
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a tag body names an element that breaks the line around its text.
pub(super) fn is_block_tag(tag: &str) -> bool {
    let name = tag
        .trim_start_matches('/')
        .split([' ', '\t', '\n', '\r', '/'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "div"
            | "dd"
            | "dl"
            | "dt"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

/// Removes every `<tag ...> ... </tag>` span, case-insensitively.
///
/// The case fold is ASCII-only on purpose: element names are ASCII, and a full
/// Unicode fold changes the byte length of characters such as U+0130, which
/// would slide every offset found in the folded copy off its counterpart in the
/// original and slice a fetched page mid-codepoint.
pub(super) fn drop_element(document: &str, tag: &str) -> String {
    let lowered = document.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut result = String::with_capacity(document.len());
    let mut cursor = 0;
    while let Some(start) = lowered[cursor..].find(&open) {
        let start = cursor + start;
        result.push_str(&document[cursor..start]);
        cursor = match lowered[start..].find(&close) {
            Some(end) => start + end + close.len(),
            None => document.len(),
        };
    }
    result.push_str(&document[cursor..]);
    result
}
