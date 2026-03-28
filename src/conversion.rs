use std::{borrow::Cow, sync::Arc};

use rmcp::model::{
    Content, CreateMessageRequestParams, CreateMessageResult, ModelHint, ModelPreferences,
    RawImageContent, Role, SamplingMessage, SamplingMessageContent, Tool,
    ToolChoice as McpToolChoice, ToolChoiceMode,
};
use serde_json::{Map, Value};

use crate::{
    anthropic::{
        InputContentBlock, MessageContentInput, MessageParam, MessageRole, MessagesRequest,
        MessagesResponse, OutputContentBlock, ToolChoice, ToolDefinition, Usage,
    },
    error::BridgeError,
};

pub fn messages_request_to_sampling(
    request: MessagesRequest,
) -> Result<CreateMessageRequestParams, BridgeError> {
    let mut params = CreateMessageRequestParams::new(
        request
            .messages
            .into_iter()
            .map(message_param_to_sampling)
            .collect::<Result<Vec<_>, _>>()?,
        request.max_tokens,
    )
    .with_model_preferences(
        ModelPreferences::new().with_hints(vec![ModelHint::new(request.model)]),
    );

    if let Some(system) = request.system {
        params = params.with_system_prompt(system.flatten_text());
    }

    if let Some(temperature) = request.temperature {
        params = params.with_temperature(temperature);
    }

    if let Some(stop_sequences) = request.stop_sequences {
        params = params.with_stop_sequences(stop_sequences);
    }

    if let Some(metadata) = request.metadata {
        params = params.with_metadata(metadata);
    }

    if let Some(tools) = request.tools {
        params = params.with_tools(
            tools
                .into_iter()
                .map(tool_definition_to_mcp)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }

    if let Some(tool_choice) = request.tool_choice {
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
        object_type: "message",
        role: "assistant",
        content,
        model: result.model,
        stop_reason: result.stop_reason.as_deref().map(map_stop_reason),
        stop_sequence: None,
        usage: Usage::default(),
    })
}

fn message_param_to_sampling(message: MessageParam) -> Result<SamplingMessage, BridgeError> {
    let role = match message.role {
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
    };

    let contents = match message.content {
        MessageContentInput::String(text) => vec![SamplingMessageContent::text(text)],
        MessageContentInput::Blocks(blocks) => blocks
            .into_iter()
            .map(input_block_to_sampling)
            .collect::<Result<Vec<_>, _>>()?,
    };

    Ok(SamplingMessage::new_multiple(role, contents))
}

fn input_block_to_sampling(
    block: InputContentBlock,
) -> Result<SamplingMessageContent, BridgeError> {
    match block {
        InputContentBlock::Text { text } => Ok(SamplingMessageContent::text(text)),
        InputContentBlock::Image { source } => {
            if source.source_type != "base64" {
                return Err(BridgeError::UnsupportedAnthropicFeature(format!(
                    "unsupported image source type '{}'",
                    source.source_type
                )));
            }

            Ok(SamplingMessageContent::Image(RawImageContent {
                data: source.data,
                mime_type: source.media_type,
                meta: None,
            }))
        }
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
        ToolChoice::Tool { name } => {
            return Err(BridgeError::UnsupportedAnthropicFeature(format!(
                "specific tool choice '{name}' cannot be represented by MCP sampling"
            )));
        }
    };

    let mut tool_choice = McpToolChoice::default();
    tool_choice.mode = Some(mode);
    Ok(tool_choice)
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
    use super::*;
    use crate::anthropic::{ImageSource, SystemPrompt, ToolResultContentBlock, ToolResultTextType};
    use serde_json::json;

    #[test]
    fn protocol_request_maps_to_sampling_with_tools_and_system() {
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

        let params = messages_request_to_sampling(request).expect("request should convert");

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

    #[test]
    fn protocol_request_supports_text_and_images() {
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
                        source: ImageSource {
                            source_type: "base64".to_string(),
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

        let params = messages_request_to_sampling(request).expect("request should convert");

        let contents = params.messages[0].content.iter().collect::<Vec<_>>();
        assert!(matches!(contents[0], SamplingMessageContent::Text(_)));
        assert!(matches!(contents[1], SamplingMessageContent::Image(_)));
    }

    #[test]
    fn protocol_request_rejects_specific_tool_choice() {
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

        let error = messages_request_to_sampling(request).expect_err("request should fail");
        assert!(matches!(
            error,
            BridgeError::UnsupportedAnthropicFeature(message)
                if message.contains("specific tool choice")
        ));
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
}
