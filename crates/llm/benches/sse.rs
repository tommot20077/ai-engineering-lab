//! SSE 解析器與累加器的 benchmark(design.md §9 第 9 步)。
//!
//! ```text
//! cargo bench -p llm --bench sse
//! ```
//!
//! 想回答的問題:一則串流回應在我們這邊的處理成本,跟網路比起來算不算瓶頸?
//! 先量,有數字才決定要不要優化(CLAUDE.md 原則)。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use llm::{Accumulator, ProviderMetadata, SseParser, StopKind, StopReason, StreamEvent, Usage};
use serde_json::json;

/// 一則「長回答」的 SSE 位元組:1000 個 text_delta,中英文混合,接近真實的 Anthropic 串流。
fn anthropic_stream() -> Vec<u8> {
    let mut wire = String::new();
    let mut push = |data: serde_json::Value| {
        wire.push_str(&format!(
            "event: {}\ndata: {data}\n\n",
            data["type"].as_str().unwrap()
        ));
    };
    push(
        json!({ "type": "message_start", "message": { "id": "msg_1", "model": "claude-sonnet-5-5", "usage": { "input_tokens": 1200, "output_tokens": 1 } } }),
    );
    push(
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
    );
    for i in 0..1000 {
        let text = if i % 2 == 0 {
            "Rust 的所有權規則"
        } else {
            " makes memory safety "
        };
        push(
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        );
    }
    push(json!({ "type": "content_block_stop", "index": 0 }));
    push(
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 1000 } }),
    );
    push(json!({ "type": "message_stop" }));
    wire.into_bytes()
}

fn parse_in_chunks(bytes: &[u8], chunk: usize) -> usize {
    let mut parser = SseParser::new();
    let mut events = 0;
    for piece in bytes.chunks(chunk) {
        events += parser.feed(piece).unwrap().len();
    }
    events
}

fn bench_sse(c: &mut Criterion) {
    let bytes = anthropic_stream();
    let mut group = c.benchmark_group("sse_parse");
    // 告訴 criterion 每次處理多少 bytes,報表就會直接算出 MB/s 或 GB/s。
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    // 一次全部餵進去;再模擬網路把資料切成 1 KB 和 16 bytes 的小塊(最壞情況)。
    for chunk in [bytes.len(), 1024, 16] {
        let name = if chunk == bytes.len() {
            "whole".to_owned()
        } else {
            format!("{chunk}B_chunks")
        };
        // `black_box`:阻止 compiler 發現「結果沒被用到」而把整段計算最佳化掉。
        group.bench_function(name, |b| {
            b.iter(|| parse_in_chunks(black_box(&bytes), chunk))
        });
    }
    group.finish();
}

fn bench_accumulate(c: &mut Criterion) {
    let meta = ProviderMetadata::default;
    let mut events = vec![
        StreamEvent::MessageStart {
            id: None,
            model: "m".into(),
        },
        StreamEvent::TextStart {
            index: 0,
            meta: meta(),
        },
    ];
    for _ in 0..1000 {
        events.push(StreamEvent::TextDelta {
            index: 0,
            text: "Rust 的所有權規則".into(),
        });
    }
    events.push(StreamEvent::TextEnd {
        index: 0,
        meta: meta(),
    });
    events.push(StreamEvent::Finish {
        stop: StopReason {
            kind: StopKind::EndTurn,
            raw: "end_turn".into(),
        },
        usage: Usage::default(),
        meta: meta(),
    });

    let mut group = c.benchmark_group("accumulate");
    group.throughput(Throughput::Elements(events.len() as u64));
    group.bench_function("1000_text_deltas", |b| {
        // `iter_batched`:每輪先複製一份事件(這段不計時),只量 push + finish 本身。
        b.iter_batched(
            || events.clone(),
            |events| {
                let mut acc = Accumulator::new();
                for ev in events {
                    acc.push(ev).unwrap();
                }
                acc.finish().unwrap()
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_sse, bench_accumulate);
criterion_main!(benches);
