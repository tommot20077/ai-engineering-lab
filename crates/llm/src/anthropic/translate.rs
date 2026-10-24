//! Anthropic 的格式 ⇄ 中立格式(純邏輯,沒有 I/O)。
//!
//! - 送出:`request_body` 把 `Request` 轉成 Anthropic 的請求 body。
//! - 收回:`EventTranslator` 把 Anthropic 的 SSE 事件轉成中立的 `StreamEvent`。

use std::collections::HashMap;

use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::wire::{
    WireBlockStart, WireContent, WireDelta, WireError, WireMessage, WireRequest, WireStreamEvent,
    WireTool, WireUsage,
};
use crate::error::{LlmError, Result};
use crate::event::StreamEvent;
use crate::sse::SseEvent;
use crate::stop::{StopKind, StopReason};
use crate::types::{Content, ProviderMetadata, Request, Usage};

/// `ProviderMetadata` 裡屬於 Anthropic 的 key。
pub(super) const PROVIDER: &str = "anthropic";

/// 這些欄位由核心型別決定,provider options 不准覆蓋(寫入路徑 fail-loud)。
const CORE_FIELDS: [&str; 6] = [
    "model",
    "max_tokens",
    "stream",
    "system",
    "messages",
    "tools",
];

// ---- 送出 ----

/// 把 `Request` 轉成 Anthropic 的請求 body。
// 回傳的 `WireRequest<'_>` 借用了 `req`:`'_` 是「讓 compiler 自己推斷」的 lifetime,
// 意思一樣是「回傳值活得不能比 `req` 久」。
pub(super) fn request_body(req: &Request) -> Result<WireRequest<'_>> {
    let mut messages = Vec::with_capacity(req.messages.len());
    for message in &req.messages {
        let mut content = Vec::with_capacity(message.content.len());
        for block in &message.content {
            if let Some(wire) = content_to_wire(block)? {
                content.push(wire);
            }
        }
        messages.push(WireMessage {
            role: message.role,
            content,
        });
    }
    // `iter().map(...).collect()`:等於 Java 的 `stream().map(...).collect(toList())`。
    let tools = req
        .tools
        .iter()
        .map(|t| WireTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
        })
        .collect();
    Ok(WireRequest {
        model: &req.model,
        max_tokens: req.max_tokens,
        stream: true,
        // `as_deref`:`&Option<String>` → `Option<&str>`,不複製字串。
        system: req.system.as_deref(),
        messages,
        tools,
        extra: extra_fields(&req.provider_options)?,
    })
}

/// 一段內容 → Anthropic 的內容區塊。回傳 `None` 表示「這段不送」。
fn content_to_wire(content: &Content) -> Result<Option<WireContent<'_>>> {
    let wire = match content {
        Content::Text { text, .. } => WireContent::Text { text },
        // 沒有 Anthropic 簽章的思考內容(例如來自別家 provider),Anthropic 讀不懂也不接受,所以不送。
        Content::Thinking { text, meta } => match signature(meta) {
            Some(signature) => WireContent::Thinking {
                thinking: text,
                signature,
            },
            None => return Ok(None),
        },
        Content::ToolCall {
            id,
            name,
            args_json,
            ..
        } => WireContent::ToolUse {
            id,
            name,
            input: tool_input(args_json)?,
        },
        Content::ToolResult {
            call_id,
            content,
            is_error,
        } => WireContent::ToolResult {
            tool_use_id: call_id,
            content,
            is_error: *is_error,
        },
    };
    Ok(Some(wire))
}

fn signature(meta: &ProviderMetadata) -> Option<&str> {
    // 一連串 `?`:任何一步是 `None` 就直接回傳 `None`。等於 Java 的
    // `Optional.ofNullable(...).map(...).map(...)`,但寫起來像一般的程式。
    meta.get(PROVIDER)?.get("signature")?.as_str()
}

/// 驗證工具參數是 JSON 物件,並借出它的原始文字。
// `&RawValue` 只檢查 JSON 合不合法,不建出整棵樹,也不複製文字:比解析成 `Value` 再編碼回去省。
fn tool_input(args_json: &str) -> Result<&RawValue> {
    let raw: &RawValue = serde_json::from_str(args_json)
        .map_err(|_| LlmError::InvalidRequest("tool call arguments are not valid JSON".into()))?;
    if !raw.get().starts_with('{') {
        return Err(LlmError::InvalidRequest(
            "tool call arguments must be a JSON object".into(),
        ));
    }
    Ok(raw)
}

/// provider options 裡 Anthropic 那一份 → 請求 body 的額外欄位。
fn extra_fields(options: &ProviderMetadata) -> Result<Map<String, Value>> {
    let mut extra = Map::new();
    // 預設開啟 refusal fallback(design.md §8):被安全分類拒答時,由 API 自動換模型重試。
    extra.insert("fallbacks".into(), Value::from("default"));
    match options.get(PROVIDER) {
        None => {}
        Some(Value::Object(fields)) => {
            for (key, value) in fields {
                if CORE_FIELDS.contains(&key.as_str()) {
                    return Err(LlmError::InvalidRequest(format!(
                        "provider option `{key}` would override a core field"
                    )));
                }
                // 值是 `null` 代表「拿掉這個欄位」,例如 `{"fallbacks": null}` 關掉 fallback。
                if value.is_null() {
                    extra.remove(key);
                } else {
                    extra.insert(key.clone(), value.clone());
                }
            }
        }
        Some(_) => {
            return Err(LlmError::InvalidRequest(
                "anthropic provider options must be a JSON object".into(),
            ));
        }
    }
    Ok(extra)
}

// ---- 收回 ----

#[derive(Clone, Copy)]
enum Block {
    Text,
    Thinking,
    ToolCall,
}

/// 把 Anthropic 的串流事件逐一翻譯成中立事件。
///
/// 需要記住一點狀態:`content_block_stop` 沒說是哪一種區塊、簽章分好幾片送來、
/// stop reason 和 usage 在 `message_delta`,卻要等 `message_stop` 才能送出 `Finish`。
pub(super) struct EventTranslator {
    blocks: HashMap<usize, Block>,
    signatures: HashMap<usize, String>,
    usage: Usage,
    stop_reason: Option<String>,
    stop_details: Option<Value>,
}

impl EventTranslator {
    pub(super) fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            signatures: HashMap::new(),
            usage: Usage::default(),
            stop_reason: None,
            stop_details: None,
        }
    }

    /// 翻譯一個 SSE 事件。可能產生零個、一個或兩個中立事件。
    pub(super) fn translate(&mut self, sse: &SseEvent) -> Result<Vec<StreamEvent>> {
        // JSON 壞掉時,`?` 透過 `From<serde_json::Error>` 自動轉成 `LlmError::Protocol`(第 3 步)。
        let event: WireStreamEvent = serde_json::from_str(&sse.data)?;
        let events = match event {
            WireStreamEvent::MessageStart { message } => {
                self.merge_usage(&message.usage);
                vec![StreamEvent::MessageStart {
                    id: message.id,
                    model: message.model,
                }]
            }
            WireStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => self.block_start(index, content_block)?,
            // `Option` 也可以 `collect` 成 `Vec`:`Some(x)` 變成 `[x]`,`None` 變成 `[]`。
            WireStreamEvent::ContentBlockDelta { index, delta } => {
                self.delta(index, delta)?.into_iter().collect()
            }
            WireStreamEvent::ContentBlockStop { index } => vec![self.block_stop(index)?],
            WireStreamEvent::MessageDelta { delta, usage } => {
                self.merge_usage(&usage);
                if delta.stop_reason.is_some() {
                    self.stop_reason = delta.stop_reason;
                }
                if delta.stop_details.is_some() {
                    self.stop_details = delta.stop_details;
                }
                vec![]
            }
            WireStreamEvent::MessageStop => vec![self.finish()],
            WireStreamEvent::Ping | WireStreamEvent::Unknown => vec![],
            WireStreamEvent::Error { error } => return Err(api_error(error)),
        };
        Ok(events)
    }

    fn block_start(&mut self, index: usize, block: WireBlockStart) -> Result<Vec<StreamEvent>> {
        let meta = ProviderMetadata::default;
        let mut events = Vec::with_capacity(2);
        match block {
            WireBlockStart::Text { text } => {
                self.blocks.insert(index, Block::Text);
                events.push(StreamEvent::TextStart {
                    index,
                    meta: meta(),
                });
                // 開始事件就帶了文字的話,當成第一個 delta,不能丟。
                if !text.is_empty() {
                    events.push(StreamEvent::TextDelta { index, text });
                }
            }
            WireBlockStart::Thinking {
                thinking,
                signature,
            } => {
                self.blocks.insert(index, Block::Thinking);
                if !signature.is_empty() {
                    self.signatures.insert(index, signature);
                }
                events.push(StreamEvent::ThinkingStart {
                    index,
                    meta: meta(),
                });
                if !thinking.is_empty() {
                    events.push(StreamEvent::ThinkingDelta {
                        index,
                        text: thinking,
                    });
                }
            }
            WireBlockStart::ToolUse { id, name } => {
                self.blocks.insert(index, Block::ToolCall);
                events.push(StreamEvent::ToolCallStart {
                    index,
                    id,
                    name,
                    meta: meta(),
                });
            }
            WireBlockStart::Unsupported => {
                return Err(LlmError::protocol(format!(
                    "unsupported content block type at index {index}"
                )));
            }
        }
        Ok(events)
    }

    fn delta(&mut self, index: usize, delta: WireDelta) -> Result<Option<StreamEvent>> {
        let event = match delta {
            WireDelta::TextDelta { text } => StreamEvent::TextDelta { index, text },
            WireDelta::ThinkingDelta { thinking } => StreamEvent::ThinkingDelta {
                index,
                text: thinking,
            },
            WireDelta::InputJsonDelta { partial_json } => StreamEvent::ToolArgsDelta {
                index,
                partial_json,
            },
            // 簽章先收起來,等區塊結束時放進 `ThinkingEnd` 的 meta。
            WireDelta::SignatureDelta { signature } => {
                // `entry(...).or_default()`:沒有就先放一個空字串,再把片段接上去。
                self.signatures
                    .entry(index)
                    .or_default()
                    .push_str(&signature);
                return Ok(None);
            }
            WireDelta::Unsupported => {
                return Err(LlmError::protocol(format!(
                    "unsupported delta type at index {index}"
                )));
            }
        };
        Ok(Some(event))
    }

    fn block_stop(&mut self, index: usize) -> Result<StreamEvent> {
        let block = self.blocks.get(&index).copied().ok_or_else(|| {
            LlmError::protocol(format!("content_block_stop for unknown block {index}"))
        })?;
        let mut meta = ProviderMetadata::default();
        Ok(match block {
            Block::Text => StreamEvent::TextEnd { index, meta },
            Block::Thinking => {
                if let Some(signature) = self.signatures.remove(&index) {
                    meta.insert(PROVIDER, json!({ "signature": signature }));
                }
                StreamEvent::ThinkingEnd { index, meta }
            }
            Block::ToolCall => StreamEvent::ToolCallEnd { index, meta },
        })
    }

    fn finish(&mut self) -> StreamEvent {
        let raw = self.stop_reason.take().unwrap_or_default();
        let mut meta = ProviderMetadata::default();
        if let Some(details) = self.stop_details.take() {
            meta.insert(PROVIDER, json!({ "stop_details": details }));
        }
        StreamEvent::Finish {
            stop: stop_reason(raw),
            usage: self.usage,
            meta,
        }
    }

    /// 後來的值蓋掉先前的值:`message_delta` 的 `output_tokens` 是累計值,不是增量。
    fn merge_usage(&mut self, usage: &WireUsage) {
        if let Some(v) = usage.input_tokens {
            self.usage.input_tokens = v;
        }
        if let Some(v) = usage.output_tokens {
            self.usage.output_tokens = v;
        }
        if let Some(v) = usage.cache_read_input_tokens {
            self.usage.cache_read_tokens = v;
        }
        if let Some(v) = usage.cache_creation_input_tokens {
            self.usage.cache_write_tokens = v;
        }
    }
}

fn stop_reason(raw: String) -> StopReason {
    let kind = match raw.as_str() {
        "end_turn" => StopKind::EndTurn,
        "max_tokens" => StopKind::MaxTokens,
        "stop_sequence" => StopKind::StopSequence,
        "tool_use" => StopKind::ToolUse,
        "refusal" => StopKind::Refusal,
        "pause_turn" => StopKind::PauseTurn,
        // 沒看過的值不猜意思,交給上層決定(fail-closed),原值保留在 `raw`。
        _ => StopKind::Unknown,
    };
    StopReason { kind, raw }
}

/// 串流中途的 `error` 事件 → 對應的 HTTP 狀態碼,這樣「可不可以重試」只需要一套規則。
fn api_error(error: WireError) -> LlmError {
    let status = match error.kind.as_str() {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        // `api_error` 與沒看過的類型都當成伺服器端錯誤。
        _ => 500,
    };
    LlmError::http(status, None, &format!("{}: {}", error.kind, error.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Accumulator, Content, Message, ProviderMetadata, Request, Role, StopKind, ToolDef,
    };
    use serde_json::json;

    // ---- 小幫手 ----

    fn meta() -> ProviderMetadata {
        ProviderMetadata::default()
    }

    fn signed(sig: &str) -> ProviderMetadata {
        let mut m = ProviderMetadata::default();
        m.insert(PROVIDER, json!({ "signature": sig }));
        m
    }

    fn text(s: &str) -> Content {
        Content::Text {
            text: s.into(),
            meta: meta(),
        }
    }

    fn base_request(messages: Vec<Message>) -> Request {
        Request {
            model: "claude-sonnet-5-5".into(),
            system: None,
            messages,
            tools: vec![],
            max_tokens: 1024,
            provider_options: meta(),
        }
    }

    fn user(content: Vec<Content>) -> Message {
        Message {
            role: Role::User,
            content,
        }
    }

    fn sse(data: serde_json::Value) -> SseEvent {
        SseEvent {
            event: data["type"].as_str().map(str::to_owned),
            data: data.to_string(),
        }
    }

    /// 把一串 Anthropic 原始事件翻譯成中立事件(遇到錯誤就停)。
    fn translate_all(raw: Vec<serde_json::Value>) -> Result<Vec<StreamEvent>> {
        let mut t = EventTranslator::new();
        let mut out = Vec::new();
        for data in raw {
            out.extend(t.translate(&sse(data))?);
        }
        Ok(out)
    }

    fn message_start() -> serde_json::Value {
        json!({ "type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5",
            "content": [], "stop_reason": null,
            "usage": { "input_tokens": 25, "output_tokens": 1, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 0 }
        }})
    }

    fn message_end(stop_reason: &str) -> Vec<serde_json::Value> {
        vec![
            json!({ "type": "message_delta", "delta": { "stop_reason": stop_reason, "stop_sequence": null }, "usage": { "output_tokens": 12 } }),
            json!({ "type": "message_stop" }),
        ]
    }

    // ---- 送出:Request → Anthropic 的 JSON ----

    #[test]
    fn request_body_snapshot() {
        let mut options = meta();
        options.insert(PROVIDER, json!({ "output_config": { "effort": "low" } }));
        options.insert("openai", json!({ "temperature": 0.2 }));
        let req = Request {
            model: "claude-sonnet-5-5".into(),
            system: Some("You are terse.".into()),
            messages: vec![
                user(vec![text("Read a.toml")]),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Thinking {
                            text: "".into(),
                            meta: signed("sig-abc"),
                        },
                        text("Reading it."),
                        Content::ToolCall {
                            id: "toolu_1".into(),
                            name: "read_file".into(),
                            args_json: r#"{"path":"a.toml"}"#.into(),
                            meta: meta(),
                        },
                    ],
                },
                user(vec![Content::ToolResult {
                    call_id: "toolu_1".into(),
                    content: "not found".into(),
                    is_error: true,
                }]),
            ],
            tools: vec![ToolDef {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            }],
            max_tokens: 64000,
            provider_options: options,
        };
        // 先用 serde_json 轉成「真正會送出去的 JSON」再拍 snapshot。
        // 直接交給 insta 的話,它用自己的序列化方式,不認得 `RawValue`,snapshot 會跟實際送出的內容不一樣
        // (第一次審查 snapshot 時發現的)。
        let wire = serde_json::to_value(request_body(&req).unwrap()).unwrap();
        insta::assert_json_snapshot!(wire);
    }

    #[test]
    fn fallbacks_can_be_turned_off() {
        let mut req = base_request(vec![user(vec![text("hi")])]);
        req.provider_options
            .insert(PROVIDER, json!({ "fallbacks": null }));
        let body = serde_json::to_value(request_body(&req).unwrap()).unwrap();
        assert!(body.get("fallbacks").is_none(), "{body}");
    }

    #[test]
    fn provider_options_cannot_override_core_fields() {
        let mut req = base_request(vec![user(vec![text("hi")])]);
        req.provider_options
            .insert(PROVIDER, json!({ "model": "something-else" }));
        assert!(matches!(
            request_body(&req),
            Err(LlmError::InvalidRequest(_))
        ));
    }

    #[test]
    fn thinking_without_an_anthropic_signature_is_not_sent() {
        // 別家 provider 的思考內容,Anthropic 讀不懂也不接受。
        let req = base_request(vec![
            user(vec![text("q")]),
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Thinking {
                        text: "x".into(),
                        meta: meta(),
                    },
                    text("a"),
                ],
            },
        ]);
        let body = serde_json::to_value(request_body(&req).unwrap()).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([{ "type": "text", "text": "a" }])
        );
    }

    #[test]
    fn tool_args_must_be_a_json_object() {
        for bad in ["{not json", "[1, 2]", "\"str\""] {
            let req = base_request(vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall {
                    id: "t".into(),
                    name: "n".into(),
                    args_json: bad.into(),
                    meta: meta(),
                }],
            }]);
            assert!(
                matches!(request_body(&req), Err(LlmError::InvalidRequest(_))),
                "{bad}"
            );
        }
    }

    // ---- 收回:Anthropic 的串流事件 → 中立事件 ----

    #[test]
    fn text_stream_translates_and_ignores_pings_and_unknown_events() {
        let mut raw = vec![
            message_start(),
            json!({ "type": "ping" }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hi" } }),
            json!({ "type": "some_future_event", "foo": 1 }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ];
        raw.extend(message_end("end_turn"));
        let events = translate_all(raw).unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    id: Some("msg_1".into()),
                    model: "claude-sonnet-5-5".into()
                },
                StreamEvent::TextStart {
                    index: 0,
                    meta: meta()
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "Hi".into()
                },
                StreamEvent::TextEnd {
                    index: 0,
                    meta: meta()
                },
                StreamEvent::Finish {
                    stop: StopReason {
                        kind: StopKind::EndTurn,
                        raw: "end_turn".into()
                    },
                    usage: Usage {
                        input_tokens: 25,
                        output_tokens: 12,
                        cache_read_tokens: 5,
                        cache_write_tokens: 0
                    },
                    meta: meta(),
                },
            ]
        );
    }

    #[test]
    fn tool_use_stream_translates() {
        let mut raw = vec![
            message_start(),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"pa" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "th\":1}" } }),
            json!({ "type": "content_block_stop", "index": 1 }),
        ];
        raw.extend(message_end("tool_use"));
        let events = translate_all(raw).unwrap();
        assert_eq!(
            events[1],
            StreamEvent::ToolCallStart {
                index: 1,
                id: "toolu_1".into(),
                name: "read_file".into(),
                meta: meta()
            }
        );
        assert_eq!(
            events[2],
            StreamEvent::ToolArgsDelta {
                index: 1,
                partial_json: "{\"pa".into()
            }
        );
        assert_eq!(
            events[4],
            StreamEvent::ToolCallEnd {
                index: 1,
                meta: meta()
            }
        );
        assert!(
            matches!(&events[5], StreamEvent::Finish { stop, .. } if stop.kind == StopKind::ToolUse)
        );
    }

    #[test]
    fn thinking_signature_arrives_on_thinking_end() {
        let mut raw = vec![
            message_start(),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "hmm" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig-" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "abc" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ];
        raw.extend(message_end("end_turn"));
        let events = translate_all(raw).unwrap();
        assert_eq!(
            events[3],
            StreamEvent::ThinkingEnd {
                index: 0,
                meta: signed("sig-abc")
            }
        );
    }

    #[test]
    fn unknown_stop_reason_keeps_raw_and_refusal_keeps_details() {
        let mut raw = vec![message_start()];
        raw.extend(message_end("brand_new"));
        let events = translate_all(raw).unwrap();
        assert!(matches!(&events[1], StreamEvent::Finish { stop, .. }
            if stop.kind == StopKind::Unknown && stop.raw == "brand_new"));

        let raw = vec![
            message_start(),
            json!({ "type": "message_delta",
                    "delta": { "stop_reason": "refusal", "stop_details": { "type": "refusal", "category": "cyber" } },
                    "usage": { "output_tokens": 0 } }),
            json!({ "type": "message_stop" }),
        ];
        let events = translate_all(raw).unwrap();
        let StreamEvent::Finish { stop, meta, .. } = &events[1] else {
            panic!("expected Finish")
        };
        assert_eq!(stop.kind, StopKind::Refusal);
        assert_eq!(
            meta.get(PROVIDER).unwrap()["stop_details"]["category"],
            "cyber"
        );
    }

    #[test]
    fn error_event_becomes_a_classified_error() {
        let r = translate_all(vec![
            message_start(),
            json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
        ]);
        let err = r.unwrap_err();
        assert!(matches!(err, LlmError::Http { status: 529, .. }), "{err:?}");
        assert!(err.is_retryable());
    }

    #[test]
    fn unsupported_blocks_and_deltas_are_rejected() {
        let block = translate_all(vec![
            message_start(),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "server_tool_use", "id": "x", "name": "web_search" } }),
        ]);
        assert!(matches!(block, Err(LlmError::Protocol(_))));

        let delta = translate_all(vec![
            message_start(),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "citations_delta", "citation": {} } }),
        ]);
        assert!(matches!(delta, Err(LlmError::Protocol(_))));
    }

    #[test]
    fn stop_for_unknown_block_and_malformed_json_are_rejected() {
        let stop = translate_all(vec![
            message_start(),
            json!({ "type": "content_block_stop", "index": 9 }),
        ]);
        assert!(matches!(stop, Err(LlmError::Protocol(_))));

        let mut t = EventTranslator::new();
        let bad = SseEvent {
            event: Some("message_start".into()),
            data: "{oops".into(),
        };
        assert!(matches!(t.translate(&bad), Err(LlmError::Protocol(_))));
    }

    #[test]
    fn thinking_signature_survives_a_full_round_trip() {
        // 收回 → 組成 Response → 放回對話 → 送出:簽章必須一路原樣保留。
        let mut raw = vec![
            message_start(),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig-xyz" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ];
        raw.extend(message_end("end_turn"));
        let mut acc = Accumulator::new();
        for ev in translate_all(raw).unwrap() {
            acc.push(ev).unwrap();
        }
        let resp = acc.finish().unwrap();

        let req = base_request(vec![
            user(vec![text("q")]),
            Message {
                role: Role::Assistant,
                content: resp.content,
            },
            user(vec![text("next")]),
        ]);
        let body = serde_json::to_value(request_body(&req).unwrap()).unwrap();
        assert_eq!(body["messages"][1]["content"][0]["signature"], "sig-xyz");
    }
}
