//! MCP sampling answered from the turn's own provider, and the preamble a
//! persisted session keeps across cycles.

use super::*;
use vibe_core::mcp::SamplingMessage;

/// A sampling request reaches the provider as an engine turn: the system
/// prompt leads, the roles map across, and the request's own budget and
/// temperature travel with it.
#[tokio::test]
async fn a_sampling_request_reaches_the_provider_as_a_completion() {
    struct SamplingProbe {
        seen: Arc<Mutex<Option<ProviderInput>>>,
    }

    impl CompletionProvider for SamplingProbe {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                *self.seen.lock().map_err(|_| {
                    vibe_core::provider::ProviderError::MalformedStream(
                        "test lock poisoned".to_owned(),
                    )
                })? = Some(input.clone());
                Ok(AssistantMessage {
                    text: "sampled answer".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let driver = LiveTurnDriver::from_provider_for_tests(
        Arc::new(SamplingProbe {
            seen: Arc::clone(&seen),
        }),
        "system",
    );
    let handler = driver.sampling_handler("probe-model");
    let answer = handler
        .complete(SamplingRequest {
            messages: vec![
                SamplingMessage {
                    role: SamplingRole::System,
                    content: "be brief".to_owned(),
                },
                SamplingMessage {
                    role: SamplingRole::User,
                    content: "ping".to_owned(),
                },
                SamplingMessage {
                    role: SamplingRole::Assistant,
                    content: "pong".to_owned(),
                },
            ],
            max_tokens: Some(64),
            temperature_millis: Some(250),
        })
        .await
        .expect("the completion answers");
    assert_eq!(answer.text, "sampled answer");
    assert_eq!(answer.model, "probe-model");

    let input = seen
        .lock()
        .expect("probe lock")
        .clone()
        .expect("the provider was asked");
    assert_eq!(
        input.messages,
        vec![
            ModelMessage::System {
                content: "be brief".to_owned(),
            },
            ModelMessage::user("ping".to_owned()),
            ModelMessage::Assistant {
                content: "pong".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
        ]
    );
    assert_eq!(input.limits.max_tokens, 64);
    assert_eq!(input.limits.temperature_millis, Some(250));
    assert!(!input.stream, "a sampling request is not streamed");
    assert!(
        input.tools.is_empty(),
        "a sampling request carries no tools"
    );
}

/// A backend failure is reported as an error rather than as an empty
/// completion, so no partial answer reaches the server that asked.
#[tokio::test]
async fn a_failing_provider_fails_the_sampling_request() {
    struct FailingProvider;

    impl CompletionProvider for FailingProvider {
        fn complete<'a>(
            &'a self,
            _input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async {
                Err(vibe_core::provider::ProviderError::MalformedStream(
                    "the backend refused".to_owned(),
                ))
            })
        }
    }

    let driver = LiveTurnDriver::from_provider_for_tests(Arc::new(FailingProvider), "system");
    let failure = driver
        .sampling_handler("probe-model")
        .complete(SamplingRequest {
            messages: Vec::new(),
            max_tokens: None,
            temperature_millis: None,
        })
        .await
        .expect_err("a failing backend fails the request");
    assert!(
        failure.to_string().contains("the backend refused"),
        "{failure}"
    );
}
