use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post},
};
#[cfg(test)]
use rmcp::{
    ClientHandler, RoleClient,
    model::{
        ClientCapabilities, ClientInfo, CreateMessageRequestParams, CreateMessageResult,
        Implementation, SamplingMessage,
    },
    transport::StreamableHttpClientTransport,
};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    model::{InitializeRequestParams, ServerCapabilities, ServerInfo},
    service::{NotificationContext, RequestContext, ServiceError},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Serialize;
use tokio::{net::TcpListener, sync::RwLock};

use crate::{
    anthropic::MessagesRequest,
    conversion::{messages_request_to_sampling, sampling_result_to_messages_response},
    error::BridgeError,
    streaming::{messages_response_to_json_response, messages_response_to_sse_response},
};

pub const DEFAULT_STDIO_SESSION_KEY: &str = "stdio";
pub const MCP_SESSION_HEADER: &str = "mcp-session-id";
pub const API_SESSION_HEADER: &str = "x-mcp-session-id";

#[derive(Clone, Default)]
pub struct AppState {
    peers: PeerRegistry,
    next_message_id: Arc<AtomicU64>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn peers(&self) -> PeerRegistry {
        self.peers.clone()
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(health_handler))
            .route("/v1/messages", post(messages_handler))
            .with_state(self.clone())
    }

    fn allocate_message_id(&self) -> String {
        let id = self.next_message_id.fetch_add(1, Ordering::Relaxed);
        format!("msg_{id}")
    }
}

#[derive(Clone, Default)]
pub struct PeerRegistry {
    inner: Arc<RwLock<HashMap<String, Peer<RoleServer>>>>,
}

impl PeerRegistry {
    pub async fn register(&self, key: impl Into<String>, peer: Peer<RoleServer>) {
        self.inner.write().await.insert(key.into(), peer);
    }

    async fn select(&self, requested_key: Option<&str>) -> Result<Peer<RoleServer>, ApiError> {
        let peers = self.inner.read().await;

        if let Some(requested_key) = requested_key {
            return peers.get(requested_key).cloned().ok_or_else(|| {
                ApiError::service_unavailable(format!(
                    "no MCP client session registered for '{requested_key}'"
                ))
            });
        }

        if let Some(peer) = peers.get(DEFAULT_STDIO_SESSION_KEY) {
            return Ok(peer.clone());
        }

        match peers.len() {
            0 => Err(ApiError::service_unavailable(
                "no MCP client is connected yet".to_string(),
            )),
            1 => Ok(peers
                .values()
                .next()
                .expect("peer exists when len is 1")
                .clone()),
            _ => Err(ApiError::invalid_request(format!(
                "multiple MCP client sessions are connected; set the '{API_SESSION_HEADER}' header"
            ))),
        }
    }

    #[cfg(test)]
    async fn keys(&self) -> Vec<String> {
        let mut keys = self.inner.read().await.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        keys
    }
}

#[derive(Clone)]
enum RegistrationStrategy {
    Fixed(String),
    HttpSessionHeader,
}

impl RegistrationStrategy {
    fn session_key_from_request(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        match self {
            Self::Fixed(key) => Some(key.clone()),
            Self::HttpSessionHeader => session_key_from_extensions(&context.extensions),
        }
    }

    fn session_key_from_notification(
        &self,
        context: &NotificationContext<RoleServer>,
    ) -> Option<String> {
        match self {
            Self::Fixed(key) => Some(key.clone()),
            Self::HttpSessionHeader => session_key_from_extensions(&context.extensions),
        }
    }
}

#[derive(Clone)]
pub struct SamplingBridgeServer {
    registry: PeerRegistry,
    registration_strategy: RegistrationStrategy,
}

impl SamplingBridgeServer {
    pub fn stdio(registry: PeerRegistry) -> Self {
        Self {
            registry,
            registration_strategy: RegistrationStrategy::Fixed(
                DEFAULT_STDIO_SESSION_KEY.to_string(),
            ),
        }
    }

    pub fn http(registry: PeerRegistry) -> Self {
        Self {
            registry,
            registration_strategy: RegistrationStrategy::HttpSessionHeader,
        }
    }
}

impl ServerHandler for SamplingBridgeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::default()).with_instructions(
            "sampling2api exposes Anthropic-compatible HTTP endpoints backed by MCP client sampling.",
        )
    }

    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ServerInfo, McpError>> + rmcp::service::MaybeSendFuture + '_
    {
        let registry = self.registry.clone();
        let peer = context.peer.clone();
        let session_key = self
            .registration_strategy
            .session_key_from_request(&context);
        let should_set_peer_info = peer.peer_info().is_none();
        let server_info = self.get_info();

        async move {
            if should_set_peer_info {
                peer.set_peer_info(request);
            }
            if let Some(session_key) = session_key {
                registry.register(session_key, peer).await;
            }
            Ok(server_info)
        }
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let registry = self.registry.clone();
        let peer = context.peer.clone();
        let session_key = self
            .registration_strategy
            .session_key_from_notification(&context);

        async move {
            if let Some(session_key) = session_key {
                registry.register(session_key, peer).await;
            }
        }
    }
}

pub async fn run_stdio_bridge(listen_addr: SocketAddr) -> anyhow::Result<()> {
    let state = AppState::new();
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {listen_addr}"))?;
    let router = state.router();

    let http_task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .context("HTTP server exited unexpectedly")
    });

    let bridge = SamplingBridgeServer::stdio(state.peers());
    let mcp = bridge
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .context("failed to start stdio MCP service")?;
    mcp.waiting()
        .await
        .context("stdio MCP service exited unexpectedly")?;

    http_task.abort();
    let _ = http_task.await;
    Ok(())
}

pub async fn run_http_bridge(listen_addr: SocketAddr, mcp_path: &str) -> anyhow::Result<()> {
    let state = AppState::new();
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {listen_addr}"))?;
    let router = build_http_router(state, mcp_path);

    axum::serve(listener, router)
        .await
        .context("Streamable HTTP bridge exited unexpectedly")?;
    Ok(())
}

fn build_http_router(state: AppState, mcp_path: &str) -> Router {
    let registry = state.peers();
    let service = StreamableHttpService::new(
        move || Ok(SamplingBridgeServer::http(registry.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    state.router().nest_service(mcp_path, service)
}

fn session_key_from_extensions(extensions: &rmcp::model::Extensions) -> Option<String> {
    extensions
        .get::<Parts>()
        .and_then(|parts| parts.headers.get(MCP_SESSION_HEADER))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn health_handler() -> &'static str {
    "ok"
}

async fn messages_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessagesRequest>,
) -> Response {
    match messages_handler_inner(state, headers, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn messages_handler_inner(
    state: AppState,
    headers: HeaderMap,
    request: MessagesRequest,
) -> Result<Response, ApiError> {
    let peer = state
        .peers
        .select(
            headers
                .get(API_SESSION_HEADER)
                .and_then(|value| value.to_str().ok()),
        )
        .await?;
    let stream = request.stream.unwrap_or(false);
    let params = messages_request_to_sampling(request)?;
    let result = peer
        .create_message(params)
        .await
        .map_err(ApiError::from_service_error)?;
    let response = sampling_result_to_messages_response(state.allocate_message_id(), result)?;

    Ok(if stream {
        messages_response_to_sse_response(response)
    } else {
        messages_response_to_json_response(response)
    })
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    error_type: &'static str,
    message: String,
}

impl ApiError {
    fn invalid_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message,
        }
    }

    fn unsupported(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message,
        }
    }

    fn service_unavailable(message: String) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "service_unavailable_error",
            message,
        }
    }

    fn upstream_error(message: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message,
        }
    }

    fn from_service_error(error: ServiceError) -> Self {
        match error {
            ServiceError::McpError(error) => {
                Self::upstream_error(format!("MCP client rejected sampling request: {error}"))
            }
            other => Self::upstream_error(format!("failed to reach MCP client: {other}")),
        }
    }
}

impl From<BridgeError> for ApiError {
    fn from(value: BridgeError) -> Self {
        match value {
            BridgeError::InvalidAnthropicRequest(message) => Self::invalid_request(message),
            BridgeError::UnsupportedAnthropicFeature(message) => Self::unsupported(message),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorEnvelope {
                object_type: "error".to_string(),
                error: ErrorBody {
                    error_type: self.error_type.to_string(),
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    #[serde(rename = "type")]
    object_type: String,
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::header::CONTENT_TYPE;

    use super::*;
    use crate::anthropic::{
        MessageContentInput, MessageParam, MessageRole, MessagesResponse, OutputContentBlock,
    };

    #[derive(Clone)]
    struct MockSamplingClient;

    impl ClientHandler for MockSamplingClient {
        fn create_message(
            &self,
            params: CreateMessageRequestParams,
            _context: rmcp::service::RequestContext<RoleClient>,
        ) -> impl Future<Output = Result<CreateMessageResult, McpError>> + Send + '_ {
            async move {
                let prompt = params
                    .messages
                    .first()
                    .and_then(|message| message.content.first())
                    .and_then(|content| match content {
                        rmcp::model::SamplingMessageContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "missing prompt".to_string());

                if prompt == "tool" {
                    Ok(CreateMessageResult::new(
                        SamplingMessage::assistant_tool_use(
                            "toolu_1",
                            "lookup_weather",
                            serde_json::from_value(serde_json::json!({"city": "Paris"}))
                                .expect("object"),
                        ),
                        "mock-client".to_string(),
                    )
                    .with_stop_reason(CreateMessageResult::STOP_REASON_TOOL_USE))
                } else {
                    Ok(CreateMessageResult::new(
                        SamplingMessage::assistant_text(format!("echo: {prompt}")),
                        "mock-client".to_string(),
                    )
                    .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
                }
            }
        }

        fn get_info(&self) -> ClientInfo {
            ClientInfo::new(
                ClientCapabilities::builder().enable_sampling().build(),
                Implementation::new("mock-client", "1.0.0"),
            )
        }
    }

    #[tokio::test]
    async fn stdio_bridge_round_trips_non_streaming_request() {
        let state = AppState::new();
        let router = state.router();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("local addr available");

        let http_task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("HTTP server should run");
        });

        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let bridge = SamplingBridgeServer::stdio(state.peers());
        let server_task = tokio::spawn(async move {
            let server = bridge
                .serve(server_transport)
                .await
                .expect("server should start");
            server.waiting().await.expect("server should stop cleanly");
        });

        let client = MockSamplingClient
            .serve(client_transport)
            .await
            .expect("client should connect");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .json(&MessagesRequest {
                model: "claude-sonnet-4-0".to_string(),
                max_tokens: 64,
                messages: vec![MessageParam {
                    role: MessageRole::User,
                    content: MessageContentInput::String("hello".to_string()),
                }],
                system: None,
                metadata: None,
                stop_sequences: None,
                temperature: None,
                tools: None,
                tool_choice: None,
                stream: Some(false),
            })
            .send()
            .await
            .expect("request should succeed");

        assert_eq!(response.status(), StatusCode::OK);

        let body: MessagesResponse = response.json().await.expect("valid response body");
        assert_eq!(body.model, "mock-client");
        assert_eq!(body.stop_reason.as_deref(), Some("end_turn"));
        assert!(matches!(
            body.content.first(),
            Some(OutputContentBlock::Text { text }) if text == "echo: hello"
        ));

        client.cancel().await.expect("client should cancel");
        server_task.await.expect("server task join");
        http_task.abort();
        let _ = http_task.await;
    }

    #[tokio::test]
    async fn streaming_endpoint_returns_anthropic_sse_events() {
        let state = AppState::new();
        let router = state.router();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("local addr available");

        let http_task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("HTTP server should run");
        });

        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let bridge = SamplingBridgeServer::stdio(state.peers());
        let server_task = tokio::spawn(async move {
            let server = bridge
                .serve(server_transport)
                .await
                .expect("server should start");
            server.waiting().await.expect("server should stop cleanly");
        });

        let client = MockSamplingClient
            .serve(client_transport)
            .await
            .expect("client should connect");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .json(&MessagesRequest {
                model: "claude-sonnet-4-0".to_string(),
                max_tokens: 64,
                messages: vec![MessageParam {
                    role: MessageRole::User,
                    content: MessageContentInput::String("hello".to_string()),
                }],
                system: None,
                metadata: None,
                stop_sequences: None,
                temperature: None,
                tools: None,
                tool_choice: None,
                stream: Some(true),
            })
            .send()
            .await
            .expect("request should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
        );

        let body = response.text().await.expect("SSE body");
        assert!(body.contains("event: message_start"));
        assert!(body.contains("event: content_block_delta"));
        assert!(body.contains("event: message_stop"));
        assert!(body.contains("\"text_delta\""));

        client.cancel().await.expect("client should cancel");
        server_task.await.expect("server task join");
        http_task.abort();
        let _ = http_task.await;
    }

    #[tokio::test]
    async fn http_bridge_registers_session_and_routes_by_header() {
        let state = AppState::new();
        let router = build_http_router(state.clone(), "/mcp");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("local addr available");

        let http_task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("HTTP server should run");
        });

        let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
        let client = MockSamplingClient
            .serve(transport)
            .await
            .expect("streamable HTTP client should connect");

        let session_key = wait_for_http_session_key(&state)
            .await
            .expect("session should register");

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .header(API_SESSION_HEADER, &session_key)
            .json(&MessagesRequest {
                model: "claude-sonnet-4-0".to_string(),
                max_tokens: 64,
                messages: vec![MessageParam {
                    role: MessageRole::User,
                    content: MessageContentInput::String("hello".to_string()),
                }],
                system: None,
                metadata: None,
                stop_sequences: None,
                temperature: None,
                tools: None,
                tool_choice: None,
                stream: Some(false),
            })
            .send()
            .await
            .expect("request should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        let body: MessagesResponse = response.json().await.expect("valid response body");
        assert!(matches!(
            body.content.first(),
            Some(OutputContentBlock::Text { text }) if text == "echo: hello"
        ));

        client.cancel().await.expect("client should cancel");
        http_task.abort();
        let _ = http_task.await;
    }

    async fn wait_for_http_session_key(state: &AppState) -> Option<String> {
        for _ in 0..20 {
            let keys = state.peers().keys().await;
            if let Some(key) = keys
                .into_iter()
                .find(|key| key != DEFAULT_STDIO_SESSION_KEY)
            {
                return Some(key);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        None
    }
}
