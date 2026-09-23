//! The `web_fetch` tool: one URL, rendered as text.
//!
//! Redirects are followed by hand, to a bound and only within the origin the
//! operator approved, because a redirect chain is a way to reach a host the
//! operator never approved. An HTML body is converted to Markdown the way the
//! reference converts it (see [`markdown`]), because the Markdown is what its
//! model reads.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

mod entities;
mod html;
mod markdown;
mod numeric;

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
/// | HTML is converted to Markdown before the model sees it | "HTML arrives as Markdown" |
/// | Long pages are truncated | "a long page is truncated" |
/// | The timeout is optional and capped | the `timeout` description, "at most 120" |
pub(super) fn web_fetch_spec() -> ToolSpec {
    ToolSpec {
        name: "web_fetch".to_owned(),
        description: "Retrieve one web page over http or https. HTML arrives as Markdown, \
                      and a long page is truncated rather than flooding the \
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
        // Reference `_validate_args`: every argument the tool refuses is a
        // `ToolError`, so the model reads one kind of refusal rather than two.
        return Err(ToolError::Execution("the URL must not be empty".to_owned()));
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

/// How long one call may wait: what it asked for, or the configured default
/// when it asked for nothing.
///
/// Reference `_validate_args`: a value outside the range is refused rather than
/// reduced, so an argument that cannot run is reported instead of quietly
/// becoming another one. Only what survives that check reaches
/// `_resolve_timeout`, which is why the ceiling appears here once.
pub(super) fn fetch_timeout(
    arguments: &Value,
    settings: &WebFetchConfig,
) -> Result<Duration, ToolError> {
    let Some(requested) = arguments["timeout"].as_i64() else {
        return Ok(Duration::from_secs(settings.default_timeout));
    };
    if requested <= 0 {
        return Err(ToolError::Execution(
            "the timeout must be a positive number of seconds".to_owned(),
        ));
    }
    let seconds = u64::try_from(requested).unwrap_or(u64::MAX);
    if seconds > settings.max_timeout {
        return Err(ToolError::Execution(format!(
            "the timeout cannot exceed {} seconds",
            settings.max_timeout
        )));
    }
    Ok(Duration::from_secs(seconds))
}

pub(super) fn web_fetch_handler(config: ToolConfigResolver) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let arguments = invocation.arguments.clone();
            let settings: WebFetchConfig = config.view("web_fetch");
            Box::pin(async move { run_web_fetch(&arguments, &settings).await })
        },
    )
}

/// What a browser asks for, which is what the reference sends alongside its
/// browser user agent: a page that varies its answer on the request reads these
/// two headers, so omitting them fetches a different document than the
/// reference fetched.
pub(super) const FETCH_ACCEPT: &str = concat!(
    "text/html,application/xhtml+xml,application/xml;q=0.9,",
    "image/avif,image/webp,image/apng,*/*;q=0.8"
);
pub(super) const FETCH_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// The agent the retry declares once a challenge answered the browser one.
///
/// Reference `_HONEST_USER_AGENT`: a host that fronts its pages behind a bot
/// challenge often serves them to a client that names itself, so the one retry
/// stops pretending rather than trying harder.
pub(super) const HONEST_USER_AGENT: &str = "vibe-cli";

/// The status a Cloudflare challenge answers with, paired with the header that
/// distinguishes it from an ordinary refusal.
const CHALLENGE_STATUS: u16 = 403;
const CHALLENGE_HEADER: &str = "cf-mitigated";
const CHALLENGE_VALUE: &str = "challenge";

pub(super) async fn run_web_fetch(
    arguments: &Value,
    settings: &WebFetchConfig,
) -> Result<ToolExecutionOutput, ToolError> {
    let url = fetch_url(arguments)?;
    let timeout = fetch_timeout(arguments, settings)?;
    let host = url.host_str().unwrap_or("the requested host").to_owned();
    let approved = url_origin(&url)
        .ok_or_else(|| ToolError::Execution(format!("`{host}` names no host a fetch can reach")))?;
    // Reference `_do_fetch` turns the client's own redirect handling off and
    // follows each hop itself, so every hop is checked against the origin the
    // operator approved rather than trusted because the server named it.
    // httpx writes field names in title case, so the client does too.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_title_case_headers()
        .timeout(timeout)
        .build()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let send = |target: Url, agent: String| {
        let headers = request_headers(&target, &agent);
        let request = client.get(target);
        let host = host.clone();
        async move {
            request.headers(headers?).send().await.map_err(|error| {
                // A URL can carry credentials or a query string, so the failure
                // names the host and nothing else.
                if error.is_timeout() {
                    ToolError::Execution(format!(
                        "fetching from {host} timed out after {} seconds",
                        timeout.as_secs()
                    ))
                } else {
                    ToolError::Execution(format!("fetching from {host} failed"))
                }
            })
        }
    };
    // Reference `_normalize_url` prefixes `https://` to anything that names no
    // scheme, and httpx renders the result from that text.
    let raw = arguments["url"].as_str().unwrap_or_default().trim();
    let written = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{}", raw.trim_start_matches('/'))
    };
    let mut current = url.clone();
    let mut shown = httpx_rendering(&current, &written);
    // Reference `_fetch_url` hands its headers dictionary to every hop, so once
    // a challenge made it honest the later hops keep the honest agent.
    let mut agent = settings.user_agent.clone();
    let mut hop = 0_usize;
    let response = loop {
        let mut response = send(current.clone(), agent.clone()).await?;
        // Reference `_do_fetch`: one retry per hop and no more, so a host that
        // answers every agent with a challenge fails instead of looping.
        if is_challenge(&response) {
            agent = HONEST_USER_AGENT.to_owned();
            response = send(current.clone(), agent.clone()).await?;
        }
        let Some(location) = redirect_location(&response) else {
            break response;
        };
        if hop == MAX_FETCH_REDIRECTS {
            return Err(ToolError::Execution(format!(
                "fetching from {host} exceeded {MAX_FETCH_REDIRECTS} redirects"
            )));
        }
        let next = current.join(&location).map_err(|_| {
            ToolError::Execution(format!("fetching from {host} was redirected to no URL"))
        })?;
        match url_origin(&next) {
            Some(origin) if origin == approved => {}
            Some(origin) => {
                return Err(ToolError::Execution(format!(
                    "fetching from {approved} was redirected to {origin}, which needs its own \
                     web_fetch approval"
                )));
            }
            None => {
                return Err(ToolError::Execution(format!(
                    "fetching from {host} was redirected to no http or https URL"
                )));
            }
        }
        shown = httpx_rendering(&next, &location);
        current = next;
        hop += 1;
    };
    let status = response.status();
    // Reference `response.is_error`: only a 4xx or a 5xx fails the call, so a
    // redirect status that names no location answers with its own body.
    if status.is_client_error() || status.is_server_error() {
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
    let encodings = response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect::<Vec<_>>();
    let raw = response
        .bytes()
        .await
        .map_err(|_| ToolError::Execution(format!("fetching from {host} failed")))?;
    let decoded = decode_content(&raw, &encodings).map_err(|_| {
        ToolError::Execution(format!(
            "the body from {host} does not decode as its Content-Encoding says"
        ))
    })?;
    // Reference `response.text`: no charset names another codec here, and a
    // byte that is not UTF-8 is replaced rather than refused.
    let body = String::from_utf8_lossy(&decoded).into_owned();
    // Reference `run`: the converter is reached on `text/html` alone, so a
    // body that merely names an HTML-adjacent type keeps its own bytes, and a
    // page the converter fails on fails the call with the converter's message.
    let text = if content_type.contains("text/html") {
        markdown::html_to_markdown(&body).map_err(|error| ToolError::Execution(error.0))?
    } else {
        body
    };
    // The declared cap is the whole bound: a page cut anywhere else would be
    // cut at a length the configuration never states, and a body exactly at the
    // cap is not truncated.
    let limit = settings.max_content_bytes;
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
        ("url", shown.clone()),
        ("content", content.clone()),
        ("content_type", content_type.clone()),
        (
            "was_truncated",
            reference_text::boolean(truncated).to_owned(),
        ),
    ]);
    Ok(ToolExecutionOutput::new(model_text)
        .displayed_as(json!({"kind": "webFetch", "url": shown}))
        .typed(json!({
            "url": shown,
            "content": content,
            "content_type": content_type,
            "was_truncated": truncated,
        })))
}

/// The encodings the request offers, which are the ones httpx decodes with
/// `zstandard` installed, as the reference runtime ships it.
pub(super) const FETCH_ACCEPT_ENCODING: &str = "gzip, deflate, zstd";

/// The request head the reference sends, field for field and in its order.
///
/// httpx starts from its client defaults (`Host`, `Accept-Encoding`,
/// `Connection`), then applies the call's own three headers. `Host` is set
/// here rather than left to the client, which would otherwise append it after
/// the others; it names a port only when the scheme's default is not the one
/// in use, as httpx writes it.
fn request_headers(target: &Url, agent: &str) -> Result<reqwest::header::HeaderMap, ToolError> {
    use reqwest::header::{
        ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONNECTION, HOST, HeaderMap, HeaderValue,
        USER_AGENT,
    };
    let invalid = |field: &str| ToolError::Execution(format!("the {field} header is not valid"));
    let authority = match (target.host_str(), target.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => String::new(),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        HOST,
        HeaderValue::from_str(&authority).map_err(|_| invalid("Host"))?,
    );
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static(FETCH_ACCEPT_ENCODING),
    );
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(agent).map_err(|_| invalid("User-Agent"))?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static(FETCH_ACCEPT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(FETCH_ACCEPT_LANGUAGE),
    );
    Ok(headers)
}

/// The body with every `Content-Encoding` the response names undone.
///
/// httpx `Response._get_content_decoder`: the values are split on commas and
/// lowered, an encoding it does not know is skipped rather than refused, and
/// the ones it knows are undone last applied first. `deflate` reads the zlib
/// wrapper and falls back to a raw stream, as its `DeflateDecoder` does, and
/// an empty body decodes to nothing whatever it claims.
pub(super) fn decode_content(body: &[u8], encodings: &[String]) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let applied = encodings
        .iter()
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| matches!(value.as_str(), "gzip" | "deflate" | "zstd"))
        .collect::<Vec<_>>();
    let mut data = body.to_vec();
    for encoding in applied.iter().rev() {
        if data.is_empty() {
            break;
        }
        let mut decoded = Vec::new();
        match encoding.as_str() {
            "gzip" => {
                flate2::read::GzDecoder::new(data.as_slice()).read_to_end(&mut decoded)?;
            }
            "deflate" => {
                if flate2::read::ZlibDecoder::new(data.as_slice())
                    .read_to_end(&mut decoded)
                    .is_err()
                {
                    decoded.clear();
                    flate2::read::DeflateDecoder::new(data.as_slice()).read_to_end(&mut decoded)?;
                }
            }
            _ => decoded = zstd::stream::decode_all(data.as_slice())?,
        }
        data = decoded;
    }
    Ok(data)
}

/// The origin a URL belongs to, `scheme://host[:port]` with the scheme's
/// default port left out, or [`None`] for a URL no fetch can reach.
///
/// Reference `_url_origin`: this is what an approval names and what every
/// redirect hop has to stay on. `host_str` already brackets an IPv6 literal the
/// way the reference does.
pub(super) fn url_origin(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str().filter(|host| !host.is_empty())?;
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// Where a response redirects to, when it is a redirect that names one.
///
/// httpx `Response.has_redirect_location`: the five redirect statuses with a
/// `Location` header. A `300` or a `304` is an answer, not a hop.
fn redirect_location(response: &reqwest::Response) -> Option<String> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
}

/// A URL as `str(httpx.URL)` renders it, given the text it was parsed from.
///
/// Both parsers normalize the same way except for one thing: `url` always
/// writes a `/` path after the authority, while httpx keeps an empty path when
/// the text had none, so `https://example.com?q` stays without the slash.
pub(super) fn httpx_rendering(url: &Url, written: &str) -> String {
    let after_scheme = match written.find("://") {
        Some(index) => &written[index + 3..],
        None => match written.strip_prefix("//") {
            Some(rest) => rest,
            // A relative reference inherits the base's path, which is never
            // empty once a request was made with it.
            None => return url.as_str().to_owned(),
        },
    };
    let path_is_empty = !after_scheme
        .find(['/', '?', '#'])
        .is_some_and(|index| after_scheme[index..].starts_with('/'));
    if path_is_empty && url.path() == "/" {
        format!(
            "{}{}",
            &url[..url::Position::BeforePath],
            &url[url::Position::AfterPath..]
        )
    } else {
        url.as_str().to_owned()
    }
}

/// Whether a response is the bot challenge the reference retries once.
fn is_challenge(response: &reqwest::Response) -> bool {
    response.status().as_u16() == CHALLENGE_STATUS
        && response
            .headers()
            .get(CHALLENGE_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == CHALLENGE_VALUE)
}
