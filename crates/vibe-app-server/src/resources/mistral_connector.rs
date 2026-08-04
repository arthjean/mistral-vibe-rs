use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use vibe_core::integrations::{
    ConnectorAuthKind, ConnectorBackend, ConnectorDefinition, ConnectorFuture, ConnectorTool,
    IntegrationError,
};
use vibe_core::mcp::{
    DEFAULT_MCP_STARTUP_TIMEOUT_MS, DEFAULT_MCP_TOOL_TIMEOUT_MS, HttpMcpPeerFactory,
    McpPeerFactory, McpServerConfig, McpTransportConfig,
};
use vibe_core::tools::ToolOutputSink;

use super::{
    ConnectorAuthBackend, ConnectorCatalog, ConnectorCatalogBackend, ResourceError, ResourceFuture,
    bounded_json, redact,
};

/// Connector payloads carry tool schemas, so they get a larger budget than the
/// small OAuth documents.
const MAX_CONNECTOR_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct MistralConnectorClient {
    client: reqwest::Client,
    base_url: Url,
    credential: SecretString,
}

impl MistralConnectorClient {
    pub fn new(api_endpoint: &str, credential: String) -> Result<Self, ResourceError> {
        let mut base_url = Url::parse(api_endpoint).map_err(|_| {
            ResourceError::InvalidParams("Mistral API endpoint is not a valid URL".to_owned())
        })?;
        if !is_https_or_loopback_http(&base_url) {
            return Err(ResourceError::InvalidParams(
                "Mistral connector API requires HTTPS or loopback HTTP".to_owned(),
            ));
        }
        base_url.set_path("");
        base_url.set_query(None);
        base_url.set_fragment(None);
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| ResourceError::Unavailable(error.to_string()))?,
            base_url,
            credential: SecretString::from(credential),
        })
    }

    #[must_use]
    pub fn base_url(&self) -> Url {
        self.base_url.clone()
    }

    async fn bootstrap(&self) -> Result<BootstrapResponse, ResourceError> {
        let response = self
            .client
            .get(self.endpoint("v1/connectors/bootstrap")?)
            .query(&[("include_auth_actionable_connectors", "true")])
            .bearer_auth(self.credential.expose_secret())
            .send()
            .await
            .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
        if !response.status().is_success() {
            return Err(ResourceError::Unavailable(format!(
                "connector bootstrap returned HTTP {}",
                response.status()
            )));
        }
        bounded_json(
            response,
            "connector bootstrap",
            MAX_CONNECTOR_RESPONSE_BYTES,
        )
        .await
    }

    fn endpoint(&self, path: &str) -> Result<Url, ResourceError> {
        self.base_url
            .join(path)
            .map_err(|error| ResourceError::Unavailable(error.to_string()))
    }

    async fn connector_ready(&self, connector_id: &str) -> Result<bool, ResourceError> {
        Ok(self
            .bootstrap()
            .await?
            .connectors
            .into_iter()
            .find(|connector| connector.id == connector_id)
            .is_some_and(|connector| connector.status.is_ready))
    }
}

impl ConnectorCatalogBackend for MistralConnectorClient {
    fn catalog<'a>(&'a self) -> ResourceFuture<'a, ConnectorCatalog> {
        Box::pin(async move {
            let bootstrap = self.bootstrap().await?;
            let connected = bootstrap
                .connectors
                .iter()
                .filter(|connector| connector.status.is_ready)
                .map(|connector| connector.id.clone())
                .collect();
            let definitions = bootstrap
                .connectors
                .into_iter()
                .map(|connector| connector.definition(&self.base_url))
                .collect::<Result<_, _>>()?;
            Ok(ConnectorCatalog {
                definitions,
                connected,
            })
        })
    }
}

impl ConnectorBackend for MistralConnectorClient {
    fn call<'a>(
        &'a self,
        connector_id: &'a str,
        tool: &'a str,
        arguments: Value,
        max_response_bytes: usize,
    ) -> ConnectorFuture<'a> {
        Box::pin(async move {
            let endpoint = self
                .endpoint(&format!("v1/connectors-gateway/{connector_id}/mcp"))
                .map_err(resource_integration_error)?;
            let config = McpServerConfig {
                alias: "connector_gateway".to_owned(),
                transport: McpTransportConfig::StreamableHttp {
                    url: endpoint,
                    headers: BTreeMap::from([(
                        "Authorization".to_owned(),
                        format!("Bearer {}", self.credential.expose_secret()),
                    )]),
                },
                enabled: true,
                disabled_tools: Default::default(),
                startup_timeout_ms: DEFAULT_MCP_STARTUP_TIMEOUT_MS,
                tool_timeout_ms: DEFAULT_MCP_TOOL_TIMEOUT_MS,
            };
            let peer = HttpMcpPeerFactory
                .connect(&config)
                .await
                .map_err(|error| IntegrationError::Tool(redact(&error.to_string())))?;
            let result = peer
                .call(
                    tool,
                    arguments,
                    max_response_bytes,
                    ToolOutputSink::discard(max_response_bytes),
                )
                .await
                .map_err(|error| IntegrationError::Tool(redact(&error.to_string())));
            let close = peer
                .close()
                .await
                .map_err(|error| IntegrationError::Tool(redact(&error.to_string())));
            match (result, close) {
                (Ok(result), Ok(())) => Ok(result),
                (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            }
        })
    }
}

impl ConnectorAuthBackend for MistralConnectorClient {
    fn auth_url<'a>(
        &'a self,
        _session_id: &'a str,
        connector_id: &'a str,
    ) -> ResourceFuture<'a, Option<String>> {
        Box::pin(async move {
            let response = self
                .client
                .get(self.endpoint(&format!("v1/connectors/{connector_id}/auth_url"))?)
                .bearer_auth(self.credential.expose_secret())
                .send()
                .await
                .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
            if !response.status().is_success() {
                return Err(ResourceError::Unavailable(format!(
                    "connector auth URL returned HTTP {}",
                    response.status()
                )));
            }
            let payload = bounded_json::<Value>(
                response,
                "connector auth response",
                MAX_CONNECTOR_RESPONSE_BYTES,
            )
            .await?;
            Ok(payload
                .get("auth_url")
                .or_else(|| payload.get("authUrl"))
                .and_then(Value::as_str)
                .map(str::to_owned))
        })
    }

    fn refresh<'a>(
        &'a self,
        _session_id: &'a str,
        connector_id: &'a str,
    ) -> ResourceFuture<'a, bool> {
        Box::pin(self.connector_ready(connector_id))
    }
}

fn is_https_or_loopback_http(url: &Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            }))
}

#[derive(Deserialize)]
struct BootstrapResponse {
    #[serde(default)]
    connectors: Vec<BootstrapConnector>,
}

#[derive(Deserialize)]
struct BootstrapConnector {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: BootstrapStatus,
    #[serde(default)]
    auth_action: Option<BootstrapAuthAction>,
    #[serde(default)]
    tools: Vec<BootstrapTool>,
}

impl BootstrapConnector {
    fn definition(self, base_url: &Url) -> Result<ConnectorDefinition, ResourceError> {
        if self.id.is_empty() {
            return Err(ResourceError::Unavailable(
                "connector bootstrap omitted an ID".to_owned(),
            ));
        }
        let auth_kind = match self.auth_action.as_ref().map(|action| action.kind.as_str()) {
            Some("oauth") => ConnectorAuthKind::OAuth,
            Some("credentials_setup") => ConnectorAuthKind::CredentialSetup,
            _ => ConnectorAuthKind::None,
        };
        let tools = if self.status.is_ready {
            self.tools
                .into_iter()
                .filter_map(BootstrapTool::definition)
                .collect()
        } else {
            Vec::new()
        };
        let id = self.id;
        Ok(ConnectorDefinition {
            id: id.clone(),
            name: if self.name.is_empty() {
                id.clone()
            } else {
                self.name
            },
            base_url: base_url
                .join(&format!("v1/connectors-gateway/{id}/mcp"))
                .map_err(|error| ResourceError::Unavailable(error.to_string()))?,
            auth_kind,
            tools,
        })
    }
}

#[derive(Default, Deserialize)]
struct BootstrapStatus {
    #[serde(default)]
    is_ready: bool,
}

#[derive(Deserialize)]
struct BootstrapAuthAction {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct BootstrapTool {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
    #[serde(default, rename = "outputSchema")]
    output_schema: Option<Value>,
}

impl BootstrapTool {
    fn definition(self) -> Option<ConnectorTool> {
        (!self.name.is_empty()).then(|| ConnectorTool {
            name: self.name,
            description: self.description,
            input_schema: self
                .input_schema
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            output_schema: self.output_schema,
        })
    }
}

fn resource_integration_error(error: ResourceError) -> IntegrationError {
    IntegrationError::Tool(redact(&error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn catalog_and_auth_use_canonical_connector_ids_and_bearer_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let (request_sender, request_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("request");
                let request = read_request(&mut stream);
                request_sender
                    .send(request.clone())
                    .expect("capture request");
                let body = if request.starts_with("GET /v1/connectors/bootstrap?") {
                    json!({
                        "connectors": [{
                            "id": "drive-id",
                            "name": "Drive",
                            "status": {"is_ready": true},
                            "auth_action": {"type": "oauth"},
                            "tools": [{
                                "name": "search",
                                "description": "Search files",
                                "inputSchema": {"type": "object"},
                            }],
                        }]
                    })
                } else {
                    json!({"auth_url": "https://auth.example/drive"})
                };
                write_json(&mut stream, &body);
            }
        });
        let client = MistralConnectorClient::new(&endpoint, "secret-token".to_owned())
            .expect("connector client");

        let catalog = client.catalog().await.expect("catalog");
        assert_eq!(catalog.connected, BTreeSet::from(["drive-id".to_owned()]));
        assert_eq!(catalog.definitions[0].id, "drive-id");
        assert_eq!(catalog.definitions[0].tools[0].name, "search");
        assert_eq!(
            client
                .auth_url("session", "drive-id")
                .await
                .expect("auth URL")
                .as_deref(),
            Some("https://auth.example/drive")
        );
        let requests = [
            request_receiver.recv().expect("bootstrap request"),
            request_receiver.recv().expect("auth request"),
        ];
        assert!(requests[0].starts_with(
            "GET /v1/connectors/bootstrap?include_auth_actionable_connectors=true HTTP/1.1"
        ));
        assert!(requests[1].starts_with("GET /v1/connectors/drive-id/auth_url HTTP/1.1"));
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-token")
        }));
    }

    #[tokio::test]
    async fn gateway_failure_uses_the_exact_id_and_redacts_the_credential() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let (request_sender, request_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("gateway request");
            request_sender
                .send(read_request(&mut stream))
                .expect("capture request");
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("gateway failure");
        });
        let client = MistralConnectorClient::new(&endpoint, "never-leak".to_owned())
            .expect("connector client");
        let error = client
            .call("drive-id", "search", json!({"query": "rust"}), 64 * 1024)
            .await
            .expect_err("gateway fails");
        assert!(!error.to_string().contains("never-leak"));
        let request = request_receiver.recv().expect("gateway request");
        assert!(request.starts_with("POST /v1/connectors-gateway/drive-id/mcp HTTP/1.1"));
    }

    #[tokio::test]
    async fn bootstrap_rejects_an_oversized_payload_before_reading_it() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("bootstrap request");
            let _ = read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4194305\r\nConnection: close\r\n\r\n",
                )
                .expect("oversized response");
        });
        let client =
            MistralConnectorClient::new(&endpoint, "token".to_owned()).expect("connector client");
        let error = client
            .catalog()
            .await
            .err()
            .expect("oversized bootstrap must be rejected");
        assert!(error.to_string().contains("byte budget"));
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).expect("read request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).expect("request UTF-8")
    }

    fn write_json(stream: &mut TcpStream, value: &Value) {
        let body = serde_json::to_vec(value).expect("JSON response");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).expect("headers");
        stream.write_all(&body).expect("body");
    }
}
