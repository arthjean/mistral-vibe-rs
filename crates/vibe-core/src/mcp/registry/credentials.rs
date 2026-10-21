//! How a discovery or a call of one server is authorized, and what the
//! registry records when it cannot be.

use super::*;

/// What an authorized operation came to.
pub(super) enum Authorized<T> {
    /// The server's answer, and the snapshot whose credential it accepted.
    Done(T, McpAuthorizationSnapshot),
    /// The server refused every credential it was offered.
    Refused(McpAuthorizationRequired),
    /// The operation failed for another reason.
    Failed(McpError),
}

/// Reference `_resolve_authorization`: the headers a discovery or a call of
/// this server uses, or why it has none.
pub(super) async fn resolve_authorization(
    config: &McpServerConfig,
    setup: Option<&AuthorizationSetup>,
) -> Result<(McpAuthorizationSnapshot, McpAuthorizationRef), McpAuthorizationRequired> {
    if let Some(setup) = setup
        && let Some(reference) = setup.references.get(&config.alias)
    {
        return match setup.service.resolve(reference).await {
            McpAuthorization::Required(required) => Err(required),
            McpAuthorization::Snapshot(snapshot)
                if snapshot.descriptor_revision != reference.descriptor_revision =>
            {
                Err(McpAuthorizationRequired {
                    reason: McpAuthorizationReason::Invalid,
                    descriptor_revision: snapshot.descriptor_revision,
                    observed_connection_revision: None,
                })
            }
            McpAuthorization::Snapshot(snapshot) => Ok((snapshot, reference.clone())),
        };
    }
    // A registry nobody configured connects with what the entry declares, as
    // the reference's does, and has nothing to sign an OAuth entry in with.
    let kind = authorization_kind(config);
    let reference = McpAuthorizationRef {
        server_name: config.alias.clone(),
        server_fingerprint: authorization::server_fingerprint(config),
        kind,
        descriptor_revision: "legacy-static".to_owned(),
    };
    if kind == McpAuthorizationKind::Oauth {
        return Err(McpAuthorizationRequired {
            reason: McpAuthorizationReason::Missing,
            descriptor_revision: "legacy-oauth-unconfigured".to_owned(),
            observed_connection_revision: None,
        });
    }
    Ok((
        McpAuthorizationSnapshot {
            headers: authorization::http_headers(config),
            connection_revision: "legacy-static".to_owned(),
            descriptor_revision: "legacy-static".to_owned(),
        },
        reference,
    ))
}

/// Runs `operation` with `snapshot`'s headers, and when the server refuses
/// them, rejects the credential and retries once with its replacement:
/// reference `_reject_and_retry_discovery` for a discovery and
/// `_call_authorized` for a call.
///
/// Without a setup there is nothing to reject the credential to, so the one
/// refused is what the requirement reports.
pub(super) async fn authorized<'a, T>(
    setup: Option<&AuthorizationSetup>,
    reference: &McpAuthorizationRef,
    snapshot: McpAuthorizationSnapshot,
    operation: impl Fn(BTreeMap<String, String>) -> McpFuture<'a, T>,
) -> Authorized<T> {
    match operation(snapshot.headers.clone()).await {
        Ok(answer) => return Authorized::Done(answer, snapshot),
        Err(McpError::AuthRequired) => {}
        Err(error) => return Authorized::Failed(error),
    }
    let mut replacement = match setup {
        Some(setup) => {
            setup
                .service
                .reject(reference, &snapshot.connection_revision)
                .await
        }
        None => McpAuthorization::Snapshot(snapshot.clone()),
    };
    if let Some(setup) = setup
        && let McpAuthorization::Snapshot(retry) = &replacement
        && retry.connection_revision != snapshot.connection_revision
    {
        let retry = retry.clone();
        match operation(retry.headers.clone()).await {
            Ok(answer) => return Authorized::Done(answer, retry),
            Err(McpError::AuthRequired) => {
                replacement = setup
                    .service
                    .reject(reference, &retry.connection_revision)
                    .await;
            }
            Err(error) => return Authorized::Failed(error),
        }
    }
    Authorized::Refused(required_result(replacement, &snapshot.connection_revision))
}

/// Reference `authorization_required_result`.
fn required_result(
    result: McpAuthorization,
    observed_connection_revision: &str,
) -> McpAuthorizationRequired {
    match result {
        McpAuthorization::Required(required) => required,
        McpAuthorization::Snapshot(snapshot) => McpAuthorizationRequired {
            reason: McpAuthorizationReason::Rejected,
            descriptor_revision: snapshot.descriptor_revision,
            observed_connection_revision: Some(observed_connection_revision.to_owned()),
        },
    }
}

/// Reference `_publish_authorization_required`, from a discovery or a call.
pub(super) async fn publish_required(
    state: &Mutex<McpRegistryState>,
    setup: Option<&AuthorizationSetup>,
    alias: &str,
    required: &McpAuthorizationRequired,
) {
    {
        let mut state = state.lock().await;
        let server = state.server(alias);
        server.memory.clear();
        server.needs_auth = true;
        server.descriptor_revision = Some(required.descriptor_revision.clone());
    }
    if let Some(sink) = setup.and_then(|setup| setup.sink.as_ref()) {
        sink(alias, required);
    }
}
