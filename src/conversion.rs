use std::{borrow::Cow, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rmcp::model::{
    Content, CreateMessageRequestParams, CreateMessageResult, ModelHint, ModelPreferences,
    RawImageContent, Role, SamplingMessage, SamplingMessageContent, Tool,
    ToolChoice as McpToolChoice, ToolChoiceMode,
};
use serde_json::{Map, Value};

use crate::{
    anthropic::{
        ImageSource, InputContentBlock, MessageContentInput, MessageParam, MessageRole,
        MessagesRequest, MessagesResponse, OutputContentBlock, ToolChoice, ToolDefinition, Usage,
    },
    error::BridgeError,
};

pub async fn messages_request_to_sampling(
    request: MessagesRequest,
) -> Result<CreateMessageRequestParams, BridgeError> {
    let MessagesRequest {
        model,
        max_tokens,
        mut messages,
        system,
        metadata,
        stop_sequences,
        temperature,
        tools,
        tool_choice,
        stream: _,
    } = request;

    if let Some(tool_name) = required_tool_name(&tool_choice) {
        append_required_tool_hint(&mut messages, tool_name);
    }

    let mut sampling_messages = Vec::with_capacity(messages.len());
    for message in messages {
        sampling_messages.push(message_param_to_sampling(message).await?);
    }

    let mut params = CreateMessageRequestParams::new(sampling_messages, max_tokens)
        .with_model_preferences(ModelPreferences::new().with_hints(vec![ModelHint::new(model)]));

    if let Some(system) = system {
        params = params.with_system_prompt(system.flatten_text());
    }

    if let Some(temperature) = temperature {
        params = params.with_temperature(temperature);
    }

    if let Some(stop_sequences) = stop_sequences {
        params = params.with_stop_sequences(stop_sequences);
    }

    if let Some(metadata) = metadata {
        params = params.with_metadata(metadata);
    }

    if let Some(tools) = tools {
        params = params.with_tools(
            tools
                .into_iter()
                .map(tool_definition_to_mcp)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }

    if let Some(tool_choice) = tool_choice {
        params = params.with_tool_choice(tool_choice_to_mcp(tool_choice)?);
    }

    params
        .validate()
        .map_err(BridgeError::InvalidAnthropicRequest)?;

    Ok(params)
}

pub fn sampling_result_to_messages_response(
    id: impl Into<String>,
    result: CreateMessageResult,
) -> Result<MessagesResponse, BridgeError> {
    result
        .validate()
        .map_err(BridgeError::InvalidAnthropicRequest)?;

    let content = result
        .message
        .content
        .into_vec()
        .into_iter()
        .map(output_content_block_from_sampling)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(MessagesResponse {
        id: id.into(),
        object_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model: result.model,
        stop_reason: result.stop_reason.as_deref().map(map_stop_reason),
        stop_sequence: None,
        usage: Usage::default(),
    })
}

async fn message_param_to_sampling(message: MessageParam) -> Result<SamplingMessage, BridgeError> {
    let role = match message.role {
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
    };

    let contents = match message.content {
        MessageContentInput::String(text) => vec![SamplingMessageContent::text(text)],
        MessageContentInput::Blocks(blocks) => {
            let mut contents = Vec::with_capacity(blocks.len());
            for block in blocks {
                contents.push(input_block_to_sampling(block).await?);
            }
            contents
        }
    };

    Ok(SamplingMessage::new_multiple(role, contents))
}

async fn input_block_to_sampling(
    block: InputContentBlock,
) -> Result<SamplingMessageContent, BridgeError> {
    match block {
        InputContentBlock::Text { text } => Ok(SamplingMessageContent::text(text)),
        InputContentBlock::Image { source } => image_source_to_sampling(source).await,
        InputContentBlock::ToolUse { id, name, input } => Ok(SamplingMessageContent::tool_use(
            id,
            name,
            value_to_object(input)?,
        )),
        InputContentBlock::ToolResult {
            tool_use_id,
            is_error,
            content,
        } => {
            let content = content
                .into_texts()
                .into_iter()
                .map(Content::text)
                .collect::<Vec<_>>();
            let mut result = rmcp::model::ToolResultContent::new(tool_use_id, content);
            result.is_error = is_error;
            Ok(SamplingMessageContent::ToolResult(result))
        }
    }
}

async fn image_source_to_sampling(
    source: ImageSource,
) -> Result<SamplingMessageContent, BridgeError> {
    match source {
        ImageSource::Base64 { media_type, data } => {
            Ok(SamplingMessageContent::Image(RawImageContent {
                data,
                mime_type: media_type,
                meta: None,
            }))
        }
        ImageSource::Url { url, media_type } => {
            let parsed_url = reqwest::Url::parse(&url).map_err(|error| {
                BridgeError::InvalidAnthropicRequest(format!("invalid image URL '{url}': {error}"))
            })?;

            match parsed_url.scheme() {
                "http" | "https" => {}
                scheme => {
                    return Err(BridgeError::InvalidAnthropicRequest(format!(
                        "unsupported image URL scheme '{scheme}' for '{url}'"
                    )));
                }
            }

            let response = reqwest::get(parsed_url).await.map_err(|error| {
                BridgeError::InvalidAnthropicRequest(format!(
                    "failed to download image URL '{url}': {error}"
                ))
            })?;
            let status = response.status();
            if !status.is_success() {
                return Err(BridgeError::InvalidAnthropicRequest(format!(
                    "image URL '{url}' returned HTTP {status}"
                )));
            }

            let response_media_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let mime_type = media_type.or(response_media_type).ok_or_else(|| {
                BridgeError::InvalidAnthropicRequest(format!(
                    "image URL '{url}' did not provide a media type; set source.media_type explicitly"
                ))
            })?;
            if !mime_type.starts_with("image/") {
                return Err(BridgeError::InvalidAnthropicRequest(format!(
                    "image URL '{url}' resolved to non-image media type '{mime_type}'"
                )));
            }

            let data = BASE64_STANDARD.encode(response.bytes().await.map_err(|error| {
                BridgeError::InvalidAnthropicRequest(format!(
                    "failed to read image bytes from '{url}': {error}"
                ))
            })?);

            Ok(SamplingMessageContent::Image(RawImageContent {
                data,
                mime_type,
                meta: None,
            }))
        }
    }
}

fn tool_definition_to_mcp(tool: ToolDefinition) -> Result<Tool, BridgeError> {
    Ok(Tool::new_with_raw(
        Cow::Owned(tool.name),
        tool.description.map(Cow::Owned),
        Arc::new(value_to_object(tool.input_schema)?),
    ))
}

fn tool_choice_to_mcp(tool_choice: ToolChoice) -> Result<McpToolChoice, BridgeError> {
    let mode = match tool_choice {
        ToolChoice::Auto => ToolChoiceMode::Auto,
        ToolChoice::Any => ToolChoiceMode::Required,
        ToolChoice::None => ToolChoiceMode::None,
        ToolChoice::Tool { .. } => ToolChoiceMode::Required,
    };

    let mut tool_choice = McpToolChoice::default();
    tool_choice.mode = Some(mode);
    Ok(tool_choice)
}

fn required_tool_name(tool_choice: &Option<ToolChoice>) -> Option<&str> {
    match tool_choice {
        Some(ToolChoice::Tool { name }) => Some(name.as_str()),
        _ => None,
    }
}

fn append_required_tool_hint(messages: &mut [MessageParam], tool_name: &str) {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.role == MessageRole::User)
    else {
        return;
    };

    let hint = format!("(Please call the {tool_name} tool.)");
    match &mut message.content {
        MessageContentInput::String(text) => append_hint_to_text(text, &hint),
        MessageContentInput::Blocks(blocks) => append_hint_to_blocks(blocks, hint),
    }
}

fn append_hint_to_text(text: &mut String, hint: &str) {
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(hint);
}

fn append_hint_to_blocks(blocks: &mut Vec<InputContentBlock>, hint: String) {
    if let Some(InputContentBlock::Text { text }) = blocks.last_mut() {
        append_hint_to_text(text, &hint);
        return;
    }

    blocks.push(InputContentBlock::Text { text: hint });
}

fn output_content_block_from_sampling(
    block: SamplingMessageContent,
) -> Result<OutputContentBlock, BridgeError> {
    match block {
        SamplingMessageContent::Text(text) => Ok(OutputContentBlock::Text { text: text.text }),
        SamplingMessageContent::ToolUse(tool_use) => Ok(OutputContentBlock::ToolUse {
            id: tool_use.id,
            name: tool_use.name,
            input: Value::Object(tool_use.input),
        }),
        SamplingMessageContent::Image(_)
        | SamplingMessageContent::Audio(_)
        | SamplingMessageContent::ToolResult(_) => Err(
            BridgeError::UnsupportedAnthropicFeature(
                "sampling response contained a content block that Anthropic Messages output does not support"
                    .to_string(),
            ),
        ),
    }
}

fn value_to_object(value: Value) -> Result<Map<String, Value>, BridgeError> {
    match value {
        Value::Object(object) => Ok(object),
        other => Err(BridgeError::InvalidAnthropicRequest(format!(
            "expected JSON object but received {other}"
        ))),
    }
}

fn map_stop_reason(reason: &str) -> String {
    match reason {
        "endTurn" => "end_turn".to_string(),
        "stopSequence" => "stop_sequence".to_string(),
        "maxTokens" => "max_tokens".to_string(),
        "toolUse" => "tool_use".to_string(),
        "pauseTurn" => "pause_turn".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::anthropic::{ImageSource, SystemPrompt, ToolResultContentBlock, ToolResultTextType};
    use axum::{Router, http::header::CONTENT_TYPE, routing::get};
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use serde_json::json;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn protocol_request_maps_to_sampling_with_tools_and_system() {
        let request = MessagesRequest {
            model: "claude-sonnet-4-0".to_string(),
            max_tokens: 256,
            messages: vec![
                MessageParam {
                    role: MessageRole::Assistant,
                    content: MessageContentInput::Blocks(vec![InputContentBlock::ToolUse {
                        id: "call_123".to_string(),
                        name: "lookup_weather".to_string(),
                        input: json!({"city": "Paris"}),
                    }]),
                },
                MessageParam {
                    role: MessageRole::User,
                    content: MessageContentInput::Blocks(vec![InputContentBlock::ToolResult {
                        tool_use_id: "call_123".to_string(),
                        is_error: Some(false),
                        content: crate::anthropic::ToolResultContentInput::Blocks(vec![
                            ToolResultContentBlock {
                                block_type: ToolResultTextType::Text,
                                text: "18C and sunny".to_string(),
                            },
                        ]),
                    }]),
                },
            ],
            system: Some(SystemPrompt::Blocks(vec![
                crate::anthropic::SystemTextBlock {
                    block_type: ToolResultTextType::Text,
                    text: "You are a helpful assistant.".to_string(),
                },
                crate::anthropic::SystemTextBlock {
                    block_type: ToolResultTextType::Text,
                    text: "Prefer concise answers.".to_string(),
                },
            ])),
            metadata: Some(json!({"trace_id": "abc"})),
            stop_sequences: Some(vec!["END".to_string()]),
            temperature: Some(0.3),
            tools: Some(vec![ToolDefinition {
                name: "lookup_weather".to_string(),
                description: Some("Fetch weather".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"]
                }),
            }]),
            tool_choice: Some(ToolChoice::Any),
            stream: Some(false),
        };

        let params = messages_request_to_sampling(request)
            .await
            .expect("request should convert");

        assert_eq!(params.max_tokens, 256);
        assert_eq!(
            params.system_prompt.as_deref(),
            Some("You are a helpful assistant.\n\nPrefer concise answers.")
        );
        assert_eq!(params.temperature, Some(0.3));
        assert_eq!(params.stop_sequences, Some(vec!["END".to_string()]));
        assert_eq!(params.metadata, Some(json!({"trace_id": "abc"})));
        assert_eq!(
            params
                .model_preferences
                .and_then(|prefs| prefs.hints)
                .and_then(|mut hints| hints.pop())
                .and_then(|hint| hint.name),
            Some("claude-sonnet-4-0".to_string())
        );
        assert_eq!(
            params
                .tool_choice
                .as_ref()
                .and_then(|choice| choice.mode.clone()),
            Some(ToolChoiceMode::Required)
        );
        assert_eq!(params.messages.len(), 2);
    }

    #[tokio::test]
    async fn protocol_request_supports_text_and_images() {
        let request = MessagesRequest {
            model: "claude".to_string(),
            max_tokens: 32,
            messages: vec![MessageParam {
                role: MessageRole::User,
                content: MessageContentInput::Blocks(vec![
                    InputContentBlock::Text {
                        text: "Describe this image".to_string(),
                    },
                    InputContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".to_string(),
                            data: "abc123".to_string(),
                        },
                    },
                ]),
            }],
            system: None,
            metadata: None,
            stop_sequences: None,
            temperature: None,
            tools: None,
            tool_choice: None,
            stream: None,
        };

        let params = messages_request_to_sampling(request)
            .await
            .expect("request should convert");

        let contents = params.messages[0].content.iter().collect::<Vec<_>>();
        assert!(matches!(contents[0], SamplingMessageContent::Text(_)));
        assert!(matches!(contents[1], SamplingMessageContent::Image(_)));
    }

    #[tokio::test]
    async fn protocol_request_downloads_url_images_and_converts_to_base64() {
        let (addr, _server) = spawn_image_server().await;
        let request = MessagesRequest {
            model: "claude".to_string(),
            max_tokens: 32,
            messages: vec![MessageParam {
                role: MessageRole::User,
                content: MessageContentInput::Blocks(vec![InputContentBlock::Image {
                    source: ImageSource::Url {
                        url: format!("http://{addr}/image.png"),
                        media_type: None,
                    },
                }]),
            }],
            system: None,
            metadata: None,
            stop_sequences: None,
            temperature: None,
            tools: None,
            tool_choice: None,
            stream: None,
        };

        let params = messages_request_to_sampling(request)
            .await
            .expect("request should convert");

        let contents = params.messages[0].content.iter().collect::<Vec<_>>();
        match contents[0] {
            SamplingMessageContent::Image(image) => {
                assert_eq!(image.mime_type, "image/png");
                assert_eq!(image.data, BASE64_STANDARD.encode("png-bytes"));
            }
            other => panic!("expected image content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn protocol_request_maps_specific_tool_choice_to_required_mode_with_hint() {
        let request = MessagesRequest {
            model: "claude".to_string(),
            max_tokens: 16,
            messages: vec![MessageParam {
                role: MessageRole::User,
                content: MessageContentInput::String("hello".to_string()),
            }],
            system: None,
            metadata: None,
            stop_sequences: None,
            temperature: None,
            tools: None,
            tool_choice: Some(ToolChoice::Tool {
                name: "only_this".to_string(),
            }),
            stream: None,
        };

        let params = messages_request_to_sampling(request)
            .await
            .expect("request should convert");

        assert_eq!(
            params
                .tool_choice
                .as_ref()
                .and_then(|choice| choice.mode.clone()),
            Some(ToolChoiceMode::Required)
        );
        assert_eq!(params.messages.len(), 1);
        assert_eq!(params.messages[0].role, Role::User);
        let contents = params.messages[0].content.iter().collect::<Vec<_>>();
        assert_eq!(contents.len(), 1);
        match contents[0] {
            SamplingMessageContent::Text(text) => {
                assert_eq!(text.text, "hello (Please call the only_this tool.)");
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn protocol_request_appends_specific_tool_choice_hint_to_last_user_text_block() {
        let request = MessagesRequest {
            model: "claude".to_string(),
            max_tokens: 32,
            messages: vec![
                MessageParam {
                    role: MessageRole::User,
                    content: MessageContentInput::String("first".to_string()),
                },
                MessageParam {
                    role: MessageRole::Assistant,
                    content: MessageContentInput::String("working on it".to_string()),
                },
                MessageParam {
                    role: MessageRole::User,
                    content: MessageContentInput::Blocks(vec![
                        InputContentBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: "image/png".to_string(),
                                data: "abc123".to_string(),
                            },
                        },
                        InputContentBlock::Text {
                            text: "Describe this image".to_string(),
                        },
                    ]),
                },
            ],
            system: None,
            metadata: None,
            stop_sequences: None,
            temperature: None,
            tools: Some(vec![ToolDefinition {
                name: "describe_image".to_string(),
                description: None,
                input_schema: json!({
                    "type": "object",
                    "properties": { "detail": { "type": "string" } }
                }),
            }]),
            tool_choice: Some(ToolChoice::Tool {
                name: "describe_image".to_string(),
            }),
            stream: None,
        };

        let params = messages_request_to_sampling(request)
            .await
            .expect("request should convert");

        let contents = params.messages[2].content.iter().collect::<Vec<_>>();
        assert_eq!(contents.len(), 2);
        match contents[1] {
            SamplingMessageContent::Text(text) => {
                assert_eq!(
                    text.text,
                    "Describe this image (Please call the describe_image tool.)"
                );
            }
            other => panic!("expected trailing text content, got {other:?}"),
        }
    }

    #[test]
    fn protocol_response_maps_sampling_tool_use_and_stop_reason() {
        let result = CreateMessageResult::new(
            SamplingMessage::new_multiple(
                Role::Assistant,
                vec![
                    SamplingMessageContent::text("Need to call a tool."),
                    SamplingMessageContent::tool_use(
                        "toolu_1",
                        "lookup_weather",
                        value_to_object(json!({"city": "Paris"})).expect("object"),
                    ),
                ],
            ),
            "client-model".to_string(),
        )
        .with_stop_reason(CreateMessageResult::STOP_REASON_TOOL_USE);

        let response =
            sampling_result_to_messages_response("msg_test", result).expect("response converts");

        assert_eq!(response.id, "msg_test");
        assert_eq!(response.object_type, "message");
        assert_eq!(response.role, "assistant");
        assert_eq!(response.model, "client-model");
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(response.content.len(), 2);
        assert!(matches!(
            response.content[0],
            OutputContentBlock::Text { .. }
        ));
        assert!(matches!(
            response.content[1],
            OutputContentBlock::ToolUse { .. }
        ));
    }

    async fn spawn_image_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let router = Router::new().route(
            "/image.png",
            get(|| async { ([(CONTENT_TYPE, "image/png")], "png-bytes") }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("local addr available");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("server should run");
        });

        (addr, server)
    }
}
