//! benchmark 用的薄包裝(`bench-internals` feature,不是公開 API)。
//!
//! `translate` 的函式是 `pub(super)`,只有 `anthropic` 這一層看得到;`benches/` 是另一個 crate,
//! 只能用 `pub` 的東西。這個檔案在 `anthropic` 底下,所以看得到它們,再用中立型別包一層交出去。
//! Java 對照:放在同一個 package 裡的 accessor,而且只有某個 Maven profile 才會編譯它。

use super::translate::EventTranslator;
use crate::error::Result;
use crate::event::StreamEvent;
use crate::sse::SseEvent;
use crate::types::Request;

/// `Request` → 送給 Anthropic 的 body。跟 `AnthropicClient` 送出前做的事一模一樣。
pub fn encode_request(request: &Request) -> Result<Vec<u8>> {
    super::translate::encode_request(request)
}

/// 一則回應的 Anthropic SSE 事件 → 中立的 `StreamEvent`。跟 `AnthropicClient` 收串流時做的事一樣。
pub fn translate_events(events: &[SseEvent]) -> Result<Vec<StreamEvent>> {
    let mut translator = EventTranslator::new();
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        // 跟 client 一樣:第一個錯誤就停下來。
        out.extend(translator.translate(event)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::error::LlmError;
    use crate::types::{Content, Message, ProviderMetadata, Role};

    fn sse(data: Value) -> SseEvent {
        SseEvent {
            event: data["type"].as_str().map(str::to_owned),
            data: data.to_string(),
        }
    }

    #[test]
    fn encode_request_produces_the_streaming_body_the_client_sends() {
        let request = Request {
            model: "claude-haiku-5-5".into(),
            system: Some("be brief".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".into(),
                    meta: ProviderMetadata::default(),
                }],
            }],
            tools: vec![],
            max_tokens: 64,
            provider_options: ProviderMetadata::default(),
        };

        let body: Value = serde_json::from_slice(&encode_request(&request).unwrap()).unwrap();

        assert_eq!(
            body,
            json!({
                "model": "claude-haiku-5-5",
                "max_tokens": 64,
                "stream": true,
                // 第一部分加的:伺服器過載時讓 Anthropic 自動改用備援模型(搭配 anthropic-beta header)。
                "fallbacks": "default",
                "system": "be brief",
                "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }]
            })
        );
    }

    #[test]
    fn translate_events_turns_one_answer_into_neutral_events() {
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_1", "model": "m", "usage": { "input_tokens": 9, "output_tokens": 1 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hi" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } }),
            json!({ "type": "message_stop" }),
        ]
        .map(sse);

        let names: Vec<&str> = translate_events(&events)
            .unwrap()
            .iter()
            .map(StreamEvent::type_name)
            .collect();

        // message_delta 不產生事件,它的資料併進 message_stop 的 finish。
        assert_eq!(
            names,
            [
                "message_start",
                "text_start",
                "text_delta",
                "text_end",
                "finish"
            ]
        );
    }

    #[test]
    fn translate_events_stops_at_the_first_protocol_error() {
        let events = [sse(json!({ "type": "content_block_stop", "index": 7 }))];

        let err = translate_events(&events).unwrap_err();

        assert!(matches!(err, LlmError::Protocol(_)), "{err:?}");
    }
}
