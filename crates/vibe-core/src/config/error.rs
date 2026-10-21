//! Every way loading or writing a layered configuration fails.

use std::path::PathBuf;

use thiserror::Error;

use super::{ConfigTarget, PatchError, ProxyKey};
use crate::atomic_file::AtomicWriteError;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration state lock is poisoned")]
    LockPoisoned,
    #[error("secure randomness is unavailable for configuration persistence")]
    RandomUnavailable,
    #[error("configuration path has no parent: `{0}`")]
    InvalidPath(PathBuf),
    #[error("configuration file exceeds the 4 MiB limit: `{0}`")]
    TooLarge(PathBuf),
    #[error("invalid TOML at `{path}`")]
    InvalidToml {
        path: PathBuf,
        /// Boxed so a Windows `PathBuf` beside it keeps every
        /// `Result<_, ConfigError>` under Clippy's large-error bound.
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("configuration serialization failed: {0}")]
    Serialize(toml::ser::Error),
    #[error("configuration JSON serialization failed: {0}")]
    Json(serde_json::Error),
    #[error("configuration transaction journal is corrupt at `{path}`: {source}")]
    CorruptJournal {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "configuration transaction journal contains paths outside its configured targets: `{0}`"
    )]
    UnsafeJournal(PathBuf),
    #[error("configuration I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration target `{0:?}` appears more than once in the batch")]
    DuplicateTarget(ConfigTarget),
    #[error("project configuration is unavailable because workspace trust was revoked")]
    UntrustedProject,
    #[error("invalid provider entry: {0}")]
    InvalidProvider(String),
    /// `system_prompt_id` names no prompt. Reference `_check_system_prompt`
    /// refuses the configuration rather than the first session using it.
    #[error(transparent)]
    SystemPrompt(crate::prompt::library::PromptFileError),
    /// `compaction_prompt_id` names no prompt. Reference
    /// `_check_compaction_prompt` refuses the configuration rather than the
    /// first compaction. Boxed so the error stays as small as the variant
    /// beside it that carries the same failure.
    #[error(transparent)]
    CompactionPrompt(Box<crate::prompt::library::PromptFileError>),
    /// The session did not enable the source backing this target, so writing to
    /// it would persist to a file the caller opted out of. Reference
    /// `persist_allowed`.
    #[error("configuration target `{0:?}` is not an enabled source for this session")]
    PersistenceDisabled(ConfigTarget),
    #[error("configuration changed concurrently for target `{target:?}`")]
    ConcurrentEdit { target: ConfigTarget },
    #[error(
        "configuration commit failed (`{commit}`) and rollback also failed (`{rollback}`); recovery journal retained"
    )]
    RollbackFailed { commit: String, rollback: String },
    #[error(transparent)]
    Patch(#[from] PatchError),
    /// The merged-configuration preflight refused the request, so nothing was
    /// written. Reference `ConfigPatchValidationError`.
    #[error("the configuration change was rejected: {0}")]
    PatchRejected(String),
    /// A target's write failed past the preflight. The reference reports these
    /// as the failures `apply_patch` returns.
    #[error("a configuration write failed: {0}")]
    WriteFailed(String),
    #[error("invalid VIBE environment key `{0}`")]
    InvalidEnvironmentKey(String),
    #[error("environment variable `{variable}` is not a valid {expected} for `{field}`")]
    InvalidEnvironmentValue {
        variable: String,
        field: String,
        expected: &'static str,
    },
    #[error("`{field}` entries require a `{merge_key}`")]
    MergeKeyMissing { field: String, merge_key: String },
    #[error("`{field}` cannot be composed by the `{strategy}` strategy across these layers")]
    MergeType {
        field: String,
        strategy: &'static str,
    },
    #[error("proxy value for `{0}` contains a forbidden control character")]
    InvalidProxyValue(ProxyKey),
    /// Reference `set_proxy_var`: a proxy URL needs an `http://` or `https://`
    /// scheme, which is what `httpx` accepts at startup.
    #[error("{0} needs an http:// or https:// URL, not '{1}'")]
    InvalidProxyScheme(ProxyKey, String),
    #[error("invalid MCP configuration: {0}")]
    InvalidMcp(String),
    #[error("invalid integration configuration: {0}")]
    InvalidIntegration(String),
    #[error("invalid URL in sensitive configuration field `{path}`")]
    InvalidSensitiveUrl { path: String },
    #[error("credentials are forbidden in sensitive configuration field `{path}`")]
    SensitiveUrlCredentials { path: String },
    #[error("`models` entries require an `alias` or a `name`")]
    ModelEntryWithoutAlias,
    #[error("`models` key `{key}` does not match the entry alias `{alias}`")]
    ModelAliasMismatch { key: String, alias: String },
    #[error("no model is configured; define at least one entry under `[[models]]`")]
    NoConfiguredModel,
    #[error(
        "compaction model `{alias}` names provider `{provider}`, which is not configured under `[[providers]]`"
    )]
    CompactionModelProviderMissing { alias: String, provider: String },
    /// An allowlist the organization's managed layer enforces admits none of
    /// the configured models. Reference `validate_merged`, which fails closed
    /// rather than letting a model the operator configured run.
    #[error("the enforced `allowed_models` matches none of the configured models")]
    AdminAllowlistMatchesNothing,
    #[error("vision model `{alias}` must set `supports_images = true`")]
    VisionModelCannotSee { alias: String },
    #[error(
        "vision model `{alias}` names provider `{provider}`, which is not configured under `[[providers]]`"
    )]
    VisionModelProviderMissing { alias: String, provider: String },
    #[error("`{field}` cannot be resolved to an absolute path")]
    UnresolvablePath { field: &'static str },
}

impl From<AtomicWriteError> for ConfigError {
    fn from(error: AtomicWriteError) -> Self {
        Self::Io {
            path: error.path,
            source: error.source,
        }
    }
}
