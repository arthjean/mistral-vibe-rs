//! The service routes the binaries contact besides chat completions, and the
//! neutral answer each gives when a scenario does not override it.
//!
//! The table follows the redirect table in the crate documentation: one world
//! points the provider, `vibe_base_url`, `console_base_url`,
//! `vibe_code_sessions_base_url`, and `[experiments] api_host` at the same
//! platform, so the paths below never collide. A neutral answer is the one
//! that leaves startup as a run without the route would: no managed
//! configuration, no account the console knows, no experiment, no connector,
//! no newer release.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

/// What a service route answers.
#[derive(Debug, Clone)]
pub struct RouteReply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
    pub(crate) delay: Duration,
}

impl RouteReply {
    /// A JSON body with `status`.
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }

    /// A body sent as given, with its content type.
    pub fn raw(status: u16, content_type: &str, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), content_type.to_owned())],
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    /// Waits before answering.
    #[must_use]
    pub fn delay(mut self, milliseconds: u64) -> Self {
        self.delay = Duration::from_millis(milliseconds);
        self
    }

    /// The answer a route gives when it is absent from a real service.
    pub fn not_found() -> Self {
        Self::json(404, &json!({"detail": "Not Found"}))
    }
}

pub(crate) struct Route {
    pub name: &'static str,
    pub default: fn(&str, &BTreeMap<String, String>) -> RouteReply,
}

/// The version every release route reports, so no subject sees a newer one:
/// the workspace version, which the reference's own version is not below.
const RELEASED: &str = env!("CARGO_PKG_VERSION");

/// The route that serves `method` on `path`, or [`None`].
pub(crate) fn resolve(method: &str, path: &str) -> Option<Route> {
    fn route(
        name: &'static str,
        default: fn(&str, &BTreeMap<String, String>) -> RouteReply,
    ) -> Option<Route> {
        Some(Route { name, default })
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["api", "v1", "code", "managed-config"]) => route("managed_config", |_, _| {
            RouteReply::json(200, &json!({"state": "disabled"}))
        }),
        ("GET", ["api", "vibe", "whoami"]) => route("whoami", |_, _| RouteReply::not_found()),
        ("GET", ["v1", "users", "me"]) => route("identity", |_, _| RouteReply::not_found()),
        ("POST", ["v1", "datalake", "events"]) => {
            route("datalake", |_, _| RouteReply::json(200, &json!({})))
        }
        ("POST", ["api", "eval", _]) => route("experiments", |_, _| {
            RouteReply::json(200, &json!({"features": {}}))
        }),
        ("GET", ["repos", _, _, "releases", "latest"]) => route("release_github", |_, _| {
            RouteReply::json(
                200,
                &json!({"tag_name": format!("v{RELEASED}"), "name": format!("v{RELEASED}"), "assets": []}),
            )
        }),
        ("GET", ["pypi", _, "json"]) => route("release_pypi", |_, _| {
            RouteReply::json(200, &json!({"info": {"version": RELEASED}, "releases": {}}))
        }),
        ("GET", ["v1", "connectors", "bootstrap"]) => route("connectors_bootstrap", |_, _| {
            RouteReply::json(200, &json!({"connectors": []}))
        }),
        (_, ["v1", "connectors-gateway", ..]) => {
            route("connectors_gateway", |_, _| RouteReply::not_found())
        }
        ("GET", ["pages", _]) => route("page", |path, pages| {
            let name = path.rsplit('/').next().unwrap_or_default();
            pages.get(name).map_or_else(
                || {
                    RouteReply::raw(
                        200,
                        "text/html; charset=utf-8",
                        "<html><head><title>e2e page</title></head><body><p>e2e page</p></body></html>",
                    )
                },
                |html| RouteReply::raw(200, "text/html; charset=utf-8", html.clone()),
            )
        }),
        _ => None,
    }
}

#[cfg(test)]
mod routes_tests;
