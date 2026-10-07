//! `plugin_catalog/read` (alias `plugins/read`), `plugin/info` and
//! `plugin/reload`: the plugins a unified session runs.
//!
//! Reference `PluginCatalogService` (`vibe/app_server/plugin_catalog.py`) for
//! the catalog, and `_dispatch_plugin` with `_reload_plugins`
//! (`vibe/app_server/_unified_harness_backend_adapter.py`) for the other two.
//! Each answers about the connection's root only: any other identifier is a
//! session this backend does not hold.

use super::*;
use crate::plugins::{SessionPlugins, plugin_catalog, plugin_info, plugin_reload_notices};

impl ServerConnection {
    pub(super) fn plugin_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.answer_plugin(request))
    }

    fn answer_plugin(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.require_plugin_root(&session_id)?;
        match request.method.as_str() {
            "plugin_catalog/read" | "plugins/read" => {
                let plugins = self.server.session_plugins(&session_id).ok_or_else(|| {
                    ProtocolFault::plain(
                        ProtocolErrorCode::NotImplemented,
                        "The selected session backend resolves no plugins",
                    )
                })?;
                Ok(success_batch(
                    request.id,
                    result_map([("plugins", plugin_catalog(&plugins))]),
                ))
            }
            "plugin/info" => {
                let plugins = self.server.session_plugins(&session_id).ok_or_else(|| {
                    ProtocolFault::plain(
                        ProtocolErrorCode::NotImplemented,
                        "The selected session backend resolves no plugins",
                    )
                })?;
                Ok(success_batch(
                    request.id,
                    result_map([("info", plugin_info(&plugins))]),
                ))
            }
            "plugin/reload" => self.reload_plugins(request.id, &session_id),
            method => Err(route::method_not_found(method)),
        }
    }

    fn require_plugin_root(&self, session_id: &str) -> Result<(), ProtocolFault> {
        if self.root_id().as_deref() == Some(session_id) {
            return Ok(());
        }
        Err(ProtocolFault::plain(
            ProtocolErrorCode::NotFound,
            format!("Session not found: {session_id}"),
        ))
    }

    /// Rescans the installed roots, re-pins whatever moved, and adopts the new
    /// set: the session's skills and prompt are derived again, its plugin MCP
    /// servers are discovered again, and the remarks the rescan could not
    /// repair go out as warnings.
    fn reload_plugins(
        &mut self,
        request_id: RequestId,
        session_id: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let previous = self.server.session_plugins(session_id).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::NotImplemented,
                "This session has no plugin writer, so there is nothing to reload.",
            )
        })?;
        let reloaded = self.server.reload_session_plugins(session_id, &previous)?;
        // The remarks ride the session's event queue, which the reference
        // drains ahead of the response.
        let mut batch = success_batch(request_id, Default::default());
        let response = std::mem::take(&mut batch.outbound);
        batch.outbound = reload_warning_frames(&reloaded);
        batch.outbound.extend(response);
        Ok(batch)
    }
}

fn reload_warning_frames(plugins: &SessionPlugins) -> Vec<Vec<u8>> {
    plugin_reload_notices(plugins)
        .into_iter()
        .map(|message| {
            encode_notification(
                "warning",
                result_map([(
                    "warning",
                    json!({"message": message, "code": "warning", "details": null}),
                )]),
            )
        })
        .collect()
}
