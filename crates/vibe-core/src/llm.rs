//! The model backends.
//!
//! Reference `vibe/core/llm/`. A provider entry picks a backend: the Mistral
//! backend, which reproduces the Mistral Python client the reference drives,
//! or the generic backend, which speaks the dialect the provider's
//! `api_style` names (`openai`, `reasoning`, `anthropic`, `openai-responses`,
//! `vertex-anthropic`). Both take a [`ModelRequest`] made of provider-neutral
//! [`types`] and answer with [`types::Chunk`]s; [`call`] is the agent loop's
//! side of a call, and [`utility`] picks the model a background completion
//! runs on.
//!
//! Every wait, credential and access token comes from a [`BackendContext`],
//! so a caller can run a backend against a scripted server on a fake clock.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};

pub mod adapter;
pub mod anthropic;
pub mod call;
pub mod chat;
pub mod completion;
pub mod error;
pub mod generic;
pub mod mistral;
pub mod pump;
pub mod python_json;
pub mod responses;
pub mod retry;
pub mod sse;
pub mod transport;
pub mod types;
pub mod utility;
pub mod vertex;

#[cfg(test)]
mod backend_parity_tests;

use error::{BackendFailure, KeyOrigin, KeySource, LocalFailure};
use pump::ChunkStream;
use retry::{Clock, RetryObserver};
use types::{Chunk, Message, Tool, ToolChoice};

use crate::provider::config::{ApiSettings, BackendKind, ModelConfig, ProviderConfig};

/// Where a backend reads a key: the variable a provider names, then the
/// keyring under the same name.
pub trait Credentials: Send + Sync {
    fn resolve(&self, variable: &str) -> Option<(String, KeyOrigin)>;
}

/// The process environment, the global `.env` file and the system keyring.
pub struct AmbientCredentials {
    dotenv: crate::config::DotenvValues,
    keyring: crate::auth::KeyringStore,
}

impl AmbientCredentials {
    #[must_use]
    pub fn new(dotenv: crate::config::DotenvValues, keyring: crate::auth::KeyringStore) -> Self {
        Self { dotenv, keyring }
    }
}

impl Credentials for AmbientCredentials {
    fn resolve(&self, variable: &str) -> Option<(String, KeyOrigin)> {
        if variable.is_empty() {
            return None;
        }
        if let Some(value) = self
            .dotenv
            .variable(variable)
            .filter(|value| !value.is_empty())
        {
            return Some((
                value,
                KeyOrigin {
                    source: KeySource::Environment,
                    variable: variable.to_owned(),
                },
            ));
        }
        self.keyring
            .get_api_key(variable)
            .filter(|value| !value.is_empty())
            .map(|value| {
                (
                    value,
                    KeyOrigin {
                        source: KeySource::Keyring,
                        variable: variable.to_owned(),
                    },
                )
            })
    }
}

/// Keys from a fixed map, which is what a test or an embedding caller hands a
/// backend.
#[derive(Debug, Clone, Default)]
pub struct MapCredentials(pub BTreeMap<String, String>);

impl Credentials for MapCredentials {
    fn resolve(&self, variable: &str) -> Option<(String, KeyOrigin)> {
        if variable.is_empty() {
            return None;
        }
        self.0
            .get(variable)
            .filter(|value| !value.is_empty())
            .map(|value| {
                (
                    value.clone(),
                    KeyOrigin {
                        source: KeySource::Environment,
                        variable: variable.to_owned(),
                    },
                )
            })
    }
}

pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<String, LocalFailure>> + Send + 'a>>;

/// How a Vertex AI request reaches the service: the access token it carries
/// and the regional host it goes to.
pub trait VertexAccess: Send + Sync {
    fn access_token(&self) -> TokenFuture<'_>;

    fn base_url(&self, region: &str) -> String {
        vertex::base_url(region)
    }
}

/// What a backend runs with besides its provider entry.
#[derive(Clone)]
pub struct BackendContext {
    pub api: ApiSettings,
    pub clock: Arc<dyn Clock>,
    pub credentials: Arc<dyn Credentials>,
    pub vertex: Arc<dyn VertexAccess>,
}

impl BackendContext {
    /// The process clock, the ambient credentials and Application Default
    /// Credentials for Vertex AI.
    #[must_use]
    pub fn ambient(api: ApiSettings, credentials: Arc<dyn Credentials>) -> Self {
        Self {
            api,
            clock: Arc::new(retry::SystemClock::default()),
            credentials,
            vertex: vertex::shared_credentials(),
        }
    }
}

/// One call's request. Reference `BackendLike.complete`'s arguments.
#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    pub model: &'a ModelConfig,
    pub messages: &'a [Message],
    pub temperature: f64,
    pub tools: Option<&'a [Tool]>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<&'a ToolChoice>,
    pub extra_headers: &'a [(String, String)],
    pub metadata: Option<&'a Map<String, Value>>,
}

/// A backend for one provider entry.
pub enum Backend {
    Mistral(mistral::MistralBackend),
    Generic(generic::GenericBackend),
}

impl Backend {
    /// Reference `create_backend`.
    ///
    /// # Errors
    ///
    /// A provider entry the backend cannot run with.
    pub fn new(provider: ProviderConfig, context: BackendContext) -> Result<Self, LocalFailure> {
        match provider.backend {
            BackendKind::Mistral => {
                mistral::MistralBackend::new(provider, context).map(Self::Mistral)
            }
            BackendKind::Generic => {
                generic::GenericBackend::new(provider, context).map(Self::Generic)
            }
        }
    }

    /// One non-streaming call.
    ///
    /// # Errors
    ///
    /// The provider, the network or the answer failing the call.
    pub async fn complete(
        &self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<Chunk, BackendFailure> {
        match self {
            Self::Mistral(backend) => backend.complete(request, retries).await,
            Self::Generic(backend) => backend.complete(request, retries).await,
        }
    }

    /// One streaming call. A failure before the first piece is returned here;
    /// a later one arrives through the stream.
    ///
    /// # Errors
    ///
    /// As [`Backend::complete`].
    pub async fn complete_streaming<'a>(
        &'a self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<ChunkStream<'a>, BackendFailure> {
        match self {
            Self::Mistral(backend) => backend.complete_streaming(request, retries).await,
            Self::Generic(backend) => backend.complete_streaming(request, retries).await,
        }
    }
}
