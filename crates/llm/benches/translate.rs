//! Anthropic 翻譯的 benchmark(design-part2.md §19;第一部分 retro 留下的項目)。
//!
//! ```text
//! cargo bench -p llm --features bench-internals --bench translate
//! ```
//!
//! 想回答的問題跟 `sse.rs` 一樣:跟網路比起來,我們這邊的處理算不算瓶頸?
//! SSE 解析(428 MiB/s)已經量過,這裡量它的下一站:每個事件的 JSON 反序列化 + 翻譯,
//! 以及送出前把 `Request` 組成 Anthropic 的 body。
//! 翻譯函式不是公開 API,透過 `bench-internals` feature 的 `llm::bench` 呼叫(見 Cargo.toml)。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use llm::bench::{encode_request, translate_events};
use llm::{Content, Message, ProviderMetadata, Request, Role, SseEvent, ToolDef};
use serde_json::{Value, json};

fn sse(data: Value) -> SseEvent {
    SseEvent {
        event: data["type"].as_str().map(str::to_owned),
        data: data.to_string(),
    }
}

/// 跟 `sse.rs` 同一則「長回答」:1000 個 text_delta,中英文混合。
fn long_answer() -> Vec<SseEvent> {
    let mut events = vec![
        sse(
            json!({ "type": "message_start", "message": { "id": "msg_1", "model": "claude-sonnet-5-5", "usage": { "input_tokens": 1200, "output_tokens": 1 } } }),
        ),
        sse(
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        ),
    ];
    for i in 0..1000 {
        let text = if i % 2 == 0 {
            "Rust 的所有權規則"
        } else {
            " makes memory safety "
        };
        events.push(sse(
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        ));
    }
    events.push(sse(json!({ "type": "content_block_stop", "index": 0 })));
    events.push(sse(
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 1000 } }),
    ));
    events.push(sse(json!({ "type": "message_stop" })));
    events
}

/// 一次工具呼叫,參數 JSON 分成 500 片送來(模型寫一大段程式碼當參數時就是這樣)。
fn tool_call_answer() -> Vec<SseEvent> {
    let mut events = vec![
        sse(
            json!({ "type": "message_start", "message": { "id": "msg_2", "model": "claude-sonnet-5-5", "usage": { "input_tokens": 800, "output_tokens": 1 } } }),
        ),
        sse(
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "write_file", "input": {} } }),
        ),
    ];
    for i in 0..500 {
        let piece = if i == 0 {
            r#"{"path": "src/main.rs", "content": ""#.to_owned()
        } else {
            "fn main() { println!(\\\"hi\\\"); }\\n".to_owned()
        };
        events.push(sse(
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": piece } }),
        ));
    }
    events.push(sse(
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"}" } }),
    ));
    events.push(sse(json!({ "type": "content_block_stop", "index": 0 })));
    events.push(sse(
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 600 } }),
    ));
    events.push(sse(json!({ "type": "message_stop" })));
    events
}

/// 一段 20 輪的對話,有 system prompt、兩個工具、一次工具呼叫與結果:agent 每一步都要送一次這樣的請求。
fn conversation() -> Request {
    let text = |role, text: &str| Message {
        role,
        content: vec![Content::Text {
            text: text.into(),
            meta: ProviderMetadata::default(),
        }],
    };
    let mut messages = vec![];
    for i in 0..10 {
        messages.push(text(
            Role::User,
            &format!("第 {i} 個問題:請解釋 Rust 的 borrow checker 在這段程式碼裡做了什麼。"),
        ));
        messages.push(text(
            Role::Assistant,
            &"borrow checker 確認每個參考都活得比它指向的資料短。 ".repeat(20),
        ));
    }
    messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall {
            id: "toolu_1".into(),
            name: "read_file".into(),
            args_json: r#"{"path": "src/lib.rs"}"#.into(),
            meta: ProviderMetadata::default(),
        }],
    });
    messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            call_id: "toolu_1".into(),
            content: "pub fn add(a: i32, b: i32) -> i32 { a + b }\n".repeat(50),
            is_error: false,
        }],
    });
    let tool = |name: &str| ToolDef {
        name: name.into(),
        description: format!("{name} a file in the workspace"),
        input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
    };
    Request {
        model: "claude-sonnet-5-5".into(),
        system: Some("You are a careful Rust tutor. Answer in Traditional Chinese.".into()),
        messages,
        tools: vec![tool("read_file"), tool("write_file")],
        max_tokens: 4096,
        provider_options: ProviderMetadata::default(),
    }
}

fn data_bytes(events: &[SseEvent]) -> u64 {
    events.iter().map(|e| e.data.len() as u64).sum()
}

fn bench_translate_events(c: &mut Criterion) {
    let mut group = c.benchmark_group("translate_events");
    for (name, events) in [
        ("1000_text_deltas", long_answer()),
        ("500_tool_arg_deltas", tool_call_answer()),
    ] {
        // 以 `data:` 的位元組數算吞吐量,才能直接跟 SSE 解析的 MiB/s 比。
        group.throughput(Throughput::Bytes(data_bytes(&events)));
        group.bench_function(name, |b| {
            b.iter(|| translate_events(black_box(&events)).unwrap())
        });
    }
    group.finish();
}

fn bench_encode_request(c: &mut Criterion) {
    let request = conversation();
    let size = encode_request(&request).unwrap().len();
    let mut group = c.benchmark_group("encode_request");
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_function("20_turn_conversation", |b| {
        b.iter(|| encode_request(black_box(&request)).unwrap())
    });
    group.finish();
}

criterion_group!(benches, bench_translate_events, bench_encode_request);
criterion_main!(benches);
