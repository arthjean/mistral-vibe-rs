//! Describing images to a model that cannot read them.
//!
//! Reference `vibe/core/vision/` and `vibe/app_server/_vision.py`: a blind
//! session model is handed what a vision model of its configuration says an
//! image shows. Each image is described once per session, keyed by what it
//! is rather than where it was sent from, so history never stops matching
//! itself.

use std::collections::HashMap;
use std::sync::Mutex;

use futures_util::StreamExt;
use futures_util::stream;
use sha1::{Digest, Sha1};

use crate::llm::error::BackendFailure;
use crate::llm::retry::NoRetryObserver;
use crate::llm::types::{Image, Message};
use crate::llm::{Backend, BackendContext, ModelRequest};
use crate::provider::config::{BackendKind, ModelConfig, ProviderConfig, ThinkingLevel};
use crate::telemetry::TelemetryCallType;

/// Reference `MAX_DESCRIPTION_TOKENS`.
pub const MAX_DESCRIPTION_TOKENS: u64 = 2_048;
/// Reference `MAX_INSTRUCTION_CHARS`: how much of the prompt steers a
/// description.
const MAX_INSTRUCTION_CHARS: usize = 2_000;
/// Reference `MAX_CONCURRENT_DESCRIPTIONS`.
const MAX_CONCURRENT_DESCRIPTIONS: usize = 4;

/// What the vision model is told it is for.
const DESCRIBE_SYSTEM_PROMPT: &str = "You turn an image into text for an assistant that cannot see it. \
Report what the image shows as plainly and completely as you can: transcribe every legible piece of \
text verbatim, name the interface, chart or diagram it depicts and lay out how its parts relate. \
Report only what is visible; do not guess at what is not.";

/// What stands in for an image no description was produced for.
pub const UNREADABLE_IMAGE: &str = "This image could not be described. Ask the user what it shows.";

/// An image to describe, already read into base64.
#[derive(Debug, Clone)]
pub struct ImageToDescribe {
    pub alias: String,
    pub mime_type: String,
    pub data: String,
    /// Reference `attachment_key`.
    pub key: String,
}

impl ImageToDescribe {
    /// An image the request carried inline.
    #[must_use]
    pub fn inline(alias: &str, mime_type: &str, data: &str) -> Self {
        let hex = crate::text::hex_encode(&Sha1::digest(data.as_bytes()));
        Self {
            alias: alias.to_owned(),
            mime_type: mime_type.to_owned(),
            data: data.to_owned(),
            key: format!("inline:{hex}"),
        }
    }

    /// An image read from `path`, keyed by the file's stat.
    #[must_use]
    pub fn file(alias: &str, mime_type: &str, data: &str, path: &std::path::Path) -> Self {
        let key = std::fs::metadata(path).map_or_else(
            |_| format!("file:{}", path.display()),
            |metadata| {
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| elapsed.as_nanos());
                format!("file:{}:{mtime}:{}", path.display(), metadata.len())
            },
        );
        Self {
            alias: alias.to_owned(),
            mime_type: mime_type.to_owned(),
            data: data.to_owned(),
            key,
        }
    }
}

/// Where a description is asked for.
pub struct VisionRoute {
    pub model: ModelConfig,
    pub provider: ProviderConfig,
    pub context: BackendContext,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// One image no description came back for.
#[derive(Debug, Clone)]
pub struct FailedDescription {
    pub alias: String,
    pub reason: String,
}

/// Reference `ImageDescriber`: the descriptions one session has gathered.
#[derive(Debug, Default)]
pub struct ImageDescriber {
    cache: Mutex<HashMap<String, String>>,
}

impl ImageDescriber {
    /// The description `image` already has.
    #[must_use]
    pub fn cached(&self, image: &ImageToDescribe) -> Option<String> {
        self.cache.lock().ok()?.get(&image.key).cloned()
    }

    /// The text that replaces `image` in the turn input.
    #[must_use]
    pub fn image_text(&self, image: &ImageToDescribe) -> String {
        let body = self
            .cached(image)
            .unwrap_or_else(|| UNREADABLE_IMAGE.to_owned());
        format!("<image alias=\"{}\">\n{body}\n</image>", image.alias)
    }

    /// Describes every image not described yet, `instruction` steering what
    /// each description dwells on. Answers the images that failed.
    pub async fn describe_all(
        &self,
        images: &[ImageToDescribe],
        route: &VisionRoute,
        instruction: &str,
    ) -> Vec<FailedDescription> {
        let mut pending: Vec<&ImageToDescribe> = Vec::new();
        for image in images {
            let known = self
                .cache
                .lock()
                .is_ok_and(|cache| cache.contains_key(&image.key));
            if !known && !pending.iter().any(|queued| queued.key == image.key) {
                pending.push(image);
            }
        }
        let steered: String = instruction
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_INSTRUCTION_CHARS)
            .collect();
        let calls: Vec<DescribeCall<'_>> = pending
            .iter()
            .map(|image| Box::pin(describe(image, route, &steered)) as DescribeCall<'_>)
            .collect();
        let results: Vec<_> = stream::iter(calls)
            .buffered(MAX_CONCURRENT_DESCRIPTIONS)
            .collect()
            .await;
        let mut failures = Vec::new();
        for (image, result) in pending.into_iter().zip(results) {
            match result {
                Ok(text) => {
                    if let Ok(mut cache) = self.cache.lock() {
                        cache.insert(image.key.clone(), text);
                    }
                }
                Err(reason) => failures.push(FailedDescription {
                    alias: image.alias.clone(),
                    reason,
                }),
            }
        }
        failures
    }
}

type DescribeCall<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;

/// Reference `complete_vision`: one call with reasoning off, at the model's
/// own temperature.
async fn describe(
    image: &ImageToDescribe,
    route: &VisionRoute,
    instruction: &str,
) -> Result<String, String> {
    let mut model = route.model.clone();
    model.thinking = ThinkingLevel::Low;
    if route.provider.backend != BackendKind::Mistral {
        model.thinking = ThinkingLevel::Off;
    }
    let backend = Backend::new(route.provider.clone(), route.context.clone())
        .map_err(|error| reason(&BackendFailure::Local(error)))?;
    let mut request = format!("Describe `{}`.", image.alias);
    if !instruction.is_empty() {
        request.push_str(
            "\n\nThe assistant was asked what follows; favor what it needs to answer, \
             and still transcribe every legible piece of text:\n\n",
        );
        request.push_str(instruction);
    }
    let mut user = Message::user(request);
    user.images = vec![Image {
        mime_type: image.mime_type.clone(),
        data: image.data.clone(),
    }];
    let messages = [Message::system(DESCRIBE_SYSTEM_PROMPT), user];
    let mut headers: Vec<(String, String)> = route
        .provider
        .extra_headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    headers.push((
        "user-agent".to_owned(),
        crate::llm::call::user_agent(route.provider.backend),
    ));
    let chunk = backend
        .complete(
            &ModelRequest {
                model: &model,
                messages: &messages,
                temperature: model.temperature,
                tools: None,
                max_tokens: Some(MAX_DESCRIPTION_TOKENS),
                tool_choice: None,
                extra_headers: &headers,
                metadata: Some(&route.metadata),
                unified: false,
            },
            &NoRetryObserver,
        )
        .await
        .map_err(|error| reason(&error))?;
    let text = chunk.message.content.unwrap_or_default().trim().to_owned();
    if text.is_empty() {
        let stop = chunk
            .stop
            .and_then(|stop| stop.reason)
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(format!(
            "Vision model '{}' returned no description (stop reason: {stop})",
            route.model.alias
        ));
    }
    Ok(text)
}

/// Reference `FailedDescription.reason`: the provider's own message, else
/// the first line of the failure.
fn reason(error: &BackendFailure) -> String {
    if let BackendFailure::Backend(error) = error
        && let Some(parsed) = error
            .parsed_error
            .as_deref()
            .filter(|parsed| !parsed.is_empty())
    {
        return parsed.to_owned();
    }
    let rendered = error.to_string();
    rendered
        .trim()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The metadata a description request carries.
#[must_use]
pub fn request_metadata(session_id: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    crate::llm::utility::request_metadata(None, session_id, TelemetryCallType::SecondaryCall)
}
