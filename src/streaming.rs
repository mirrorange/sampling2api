use std::convert::Infallible;

use axum::{
    Json,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::stream;
use serde_json::{Value, json};

use crate::anthropic::{MessagesResponse, OutputContentBlock};

#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub event: &'static str,
    pub data: Value,
}

pub fn messages_response_to_sse_frames(response: &MessagesResponse) -> Vec<SseFrame> {
    let mut frames = Vec::new();

    frames.push(SseFrame {
        event: "message_start",
        data: json!({
            "type": "message_start",
            "message": {
                "id": response.id,
                "type": response.object_type,
                "role": response.role,
                "content": [],
                "model": response.model,
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": response.usage,
            }
        }),
    });

    for (index, block) in response.content.iter().enumerate() {
        match block {
            OutputContentBlock::Text { text } => {
                frames.push(SseFrame {
                    event: "content_block_start",
                    data: json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "text",
                            "text": "",
                        }
                    }),
                });
                frames.push(SseFrame {
                    event: "content_block_delta",
                    data: json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "text_delta",
                            "text": text,
                        }
                    }),
                });
            }
            OutputContentBlock::ToolUse { id, name, input } => {
                frames.push(SseFrame {
                    event: "content_block_start",
                    data: json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {},
                        }
                    }),
                });
                frames.push(SseFrame {
                    event: "content_block_delta",
                    data: json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": serde_json::to_string(input).expect("tool input should serialize"),
                        }
                    }),
                });
            }
        }

        frames.push(SseFrame {
            event: "content_block_stop",
            data: json!({
                "type": "content_block_stop",
                "index": index,
            }),
        });
    }

    frames.push(SseFrame {
        event: "message_delta",
        data: json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": response.stop_reason,
                "stop_sequence": response.stop_sequence,
            },
            "usage": {
                "output_tokens": response.usage.output_tokens,
            }
        }),
    });
    frames.push(SseFrame {
        event: "message_stop",
        data: json!({
            "type": "message_stop",
        }),
    });

    frames
}

pub fn messages_response_to_sse_response(response: MessagesResponse) -> Response {
    let frames = messages_response_to_sse_frames(&response);
    let stream = stream::iter(frames.into_iter().map(|frame| {
        Ok::<Event, Infallible>(
            Event::default()
                .event(frame.event)
                .json_data(frame.data)
                .expect("SSE frame should serialize"),
        )
    }));

    Sse::new(stream).into_response()
}

pub fn messages_response_to_json_response(response: MessagesResponse) -> Response {
    Json(response).into_response()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::anthropic::{MessagesResponse, OutputContentBlock, Usage};

    #[test]
    fn streaming_expands_text_and_tool_use_into_standard_events() {
        let response = MessagesResponse {
            id: "msg_1".to_string(),
            object_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![
                OutputContentBlock::Text {
                    text: "Hello".to_string(),
                },
                OutputContentBlock::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "lookup_weather".to_string(),
                    input: json!({"city": "Paris"}),
                },
            ],
            model: "mock-model".to_string(),
            stop_reason: Some("tool_use".to_string()),
            stop_sequence: None,
            usage: Usage::default(),
        };

        let frames = messages_response_to_sse_frames(&response);
        let events = frames.iter().map(|frame| frame.event).collect::<Vec<_>>();

        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(frames[2].data["delta"]["type"], "text_delta");
        assert_eq!(frames[5].data["delta"]["type"], "input_json_delta");
        assert_eq!(
            frames[5].data["delta"]["partial_json"],
            "{\"city\":\"Paris\"}"
        );
    }
}
