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
    http::{HeaderMap, StatusCode},
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
};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    model::{InitializeRequestParams, ServerCapabilities, ServerInfo},
    service::{NotificationContext, RequestContext, ServiceError},
};
use serde::Serialize;
use tokio::{net::TcpListener, sync::RwLock};

use crate::{
    anthropic::{MessagesRequest, MessagesResponse},
    conversion::{messages_request_to_sampling, sampling_result_to_messages_response},
    error::BridgeError,
};

pub const DEFAULT_STDIO_SESSION_KEY: &str = "stdio";
pub const SESSION_HEADER: &str = "x-mcp-session-id";

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
                "multiple MCP client sessions are connected; set the '{SESSION_HEADER}' header"
            ))),
        }
    }
}

#[derive(Clone)]
pub struct SamplingBridgeServer {
    registry: PeerRegistry,
    fixed_session_key: String,
}

impl SamplingBridgeServer {
    pub fn stdio(registry: PeerRegistry) -> Self {
        Self {
            registry,
            fixed_session_key: DEFAULT_STDIO_SESSION_KEY.to_string(),
        }
    }

    pub async fn run_stdio_http_bridge(self, listen_addr: SocketAddr) -> anyhow::Result<()> {
        let state = AppState {
            peers: self.registry.clone(),
            ..AppState::default()
        };
        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind HTTP listener on {listen_addr}"))?;
        let router = state.router();

        let http_task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .context("HTTP server exited unexpectedly")
        });

        let mcp = self
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
        let session_key = self.fixed_session_key.clone();
        let should_set_peer_info = peer.peer_info().is_none();
        let server_info = self.get_info();

        async move {
            if should_set_peer_info {
                peer.set_peer_info(request);
            }
            registry.register(session_key, peer).await;
            Ok(server_info)
        }
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let registry = self.registry.clone();
        let peer = context.peer.clone();
        let session_key = self.fixed_session_key.clone();

        async move {
            registry.register(session_key, peer).await;
        }
    }
}

async fn health_handler() -> &'static str {
    "ok"
}

async fn messages_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessagesRequest>,
) -> Result<Json<MessagesResponse>, ApiError> {
    if request.stream.unwrap_or(false) {
        return Err(ApiError::unsupported(
            "stream=true is not available yet in the stdio bridge stage".to_string(),
        ));
    }

    let peer = state
        .peers
        .select(
            headers
                .get(SESSION_HEADER)
                .and_then(|value| value.to_str().ok()),
        )
        .await?;
    let params = messages_request_to_sampling(request)?;
    let result = peer
        .create_message(params)
        .await
        .map_err(ApiError::from_service_error)?;
    let response = sampling_result_to_messages_response(state.allocate_message_id(), result)?;

    Ok(Json(response))
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
                object_type: "error",
                error: ErrorBody {
                    error_type: self.error_type,
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
    object_type: &'static str,
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::anthropic::{MessageContentInput, MessageParam, MessageRole, MessagesRequest};

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

                Ok(CreateMessageResult::new(
                    SamplingMessage::assistant_text(format!("echo: {prompt}")),
                    "mock-client".to_string(),
                )
                .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
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
            Some(crate::anthropic::OutputContentBlock::Text { text }) if text == "echo: hello"
        ));

        client.cancel().await.expect("client should cancel");
        server_task.await.expect("server task join");
        http_task.abort();
        let _ = http_task.await;
    }

    #[tokio::test]
    async fn stdio_bridge_rejects_stream_requests_before_streaming_stage() {
        let state = AppState::new();
        let response = messages_handler(
            State(state),
            HeaderMap::new(),
            Json(MessagesRequest {
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
            }),
        )
        .await
        .expect_err("streaming should be rejected before stage 4");

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(response.message.contains("stream=true"));
    }
}
