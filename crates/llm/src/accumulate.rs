//! 把串流事件組回一份完整的 `Response`(純邏輯,沒有 I/O)。
//!
//! 它同時也是協定檢查員:provider 送來的事件順序不合理,就回 `Protocol` 錯誤,
//! 而不是猜測它的意思(讀取路徑 fail-closed)。

use std::collections::BTreeMap;

use crate::error::{LlmError, Result};
use crate::event::StreamEvent;
use crate::stop::{StopKind, StopReason};
use crate::types::{Content, ProviderMetadata, Response, Usage};

/// 一則回覆最多累積多少 bytes 的內容(design.md §7),防止記憶體被灌爆。
pub(crate) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// 一次餵一個事件,最後呼叫 `finish` 取得完整回應。
///
/// ```text
/// AwaitingStart ──MessageStart──→ Streaming ──Finish──→ Finished
/// ```
pub struct Accumulator {
    phase: Phase,
    id: Option<String>,
    model: String,
    // 用 BTreeMap 而不是 Vec:index 不一定從 0 開始、也不一定連續,BTreeMap 會依 index 排好順序。
    blocks: BTreeMap<usize, Block>,
    finished: Option<Finished>,
    bytes: usize,
    max_bytes: usize,
}

// `Copy`:只是一個標記,複製它跟複製一個 int 一樣便宜。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitingStart,
    Streaming,
    Finished,
}

struct Finished {
    stop: StopReason,
    usage: Usage,
    meta: ProviderMetadata,
}

/// 正在組裝中的一段內容。
struct Block {
    body: Body,
    open: bool,
    meta: ProviderMetadata,
}

enum Body {
    Text(String),
    Thinking(String),
    ToolCall {
        id: String,
        name: String,
        args: String,
    },
}

/// 只用來比對「事件的種類」和「區塊的種類」是否一致。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    Thinking,
    ToolCall,
}

impl Body {
    fn kind(&self) -> Kind {
        match self {
            Body::Text(_) => Kind::Text,
            Body::Thinking(_) => Kind::Thinking,
            Body::ToolCall { .. } => Kind::ToolCall,
        }
    }
}

impl Block {
    /// 把組好的區塊變成 `Content`。
    // 參數是 `self`(不是 `&self`):呼叫之後這個 Block 就不存在了,
    // 所以裡面的 String 可以直接「搬」進 Content,不必複製一份。
    fn into_content(self) -> Content {
        match self.body {
            Body::Text(text) => Content::Text {
                text,
                meta: self.meta,
            },
            Body::Thinking(text) => Content::Thinking {
                text,
                meta: self.meta,
            },
            Body::ToolCall { id, name, args } => Content::ToolCall {
                id,
                name,
                args_json: args,
                meta: self.meta,
            },
        }
    }
}

// clippy 要求:有無參數的 `new()`,就要同時實作 `Default`(Rust 慣例,等於 Java 的無參數建構子)。
impl Default for Accumulator {
    fn default() -> Self {
        Self::with_max_bytes(MAX_RESPONSE_BYTES)
    }
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 自訂內容大小上限。
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            phase: Phase::AwaitingStart,
            id: None,
            model: String::new(),
            blocks: BTreeMap::new(),
            finished: None,
            bytes: 0,
            max_bytes,
        }
    }

    /// 餵入下一個事件。
    // `event: StreamEvent`(不是 `&StreamEvent`):呼叫者把事件「交給」我們。
    // 事件裡的文字片段直接搬進我們的緩衝區,整個過程沒有任何複製。
    pub fn push(&mut self, event: StreamEvent) -> Result<()> {
        match self.phase {
            Phase::AwaitingStart => match event {
                StreamEvent::MessageStart { id, model } => {
                    self.id = id;
                    self.model = model;
                    self.phase = Phase::Streaming;
                    Ok(())
                }
                // 錯誤訊息直接用事件的 Debug:它已經遮罩過內容,可以安全地放進錯誤裡。
                other => Err(LlmError::protocol(format!(
                    "{other:?} before message_start"
                ))),
            },
            Phase::Streaming => self.on_streaming(event),
            Phase::Finished => Err(LlmError::protocol(format!("{event:?} after finish"))),
        }
    }

    fn on_streaming(&mut self, event: StreamEvent) -> Result<()> {
        use StreamEvent as E;
        match event {
            E::MessageStart { .. } => Err(LlmError::protocol("duplicate message_start")),

            E::TextStart { index, meta } => self.open(index, Body::Text(String::new()), meta),
            E::ThinkingStart { index, meta } => {
                self.open(index, Body::Thinking(String::new()), meta)
            }
            E::ToolCallStart {
                index,
                id,
                name,
                meta,
            } => {
                let body = Body::ToolCall {
                    id,
                    name,
                    args: String::new(),
                };
                self.open(index, body, meta)
            }

            E::TextDelta { index, text } => self.append(index, Kind::Text, &text),
            E::ThinkingDelta { index, text } => self.append(index, Kind::Thinking, &text),
            E::ToolArgsDelta {
                index,
                partial_json,
            } => self.append(index, Kind::ToolCall, &partial_json),

            E::TextEnd { index, meta } => self.close(index, Kind::Text, meta),
            E::ThinkingEnd { index, meta } => self.close(index, Kind::Thinking, meta),
            E::ToolCallEnd { index, meta } => self.close(index, Kind::ToolCall, meta),

            E::Finish { stop, usage, meta } => {
                // `find` 回傳第一個符合條件的元素(Option),等於 Java Stream 的 `filter(...).findFirst()`。
                if let Some((index, _)) = self.blocks.iter().find(|(_, b)| b.open) {
                    return Err(LlmError::protocol(format!(
                        "block {index} still open at finish"
                    )));
                }
                self.finished = Some(Finished { stop, usage, meta });
                self.phase = Phase::Finished;
                Ok(())
            }
        }
    }

    fn open(&mut self, index: usize, body: Body, meta: ProviderMetadata) -> Result<()> {
        if self.blocks.contains_key(&index) {
            return Err(LlmError::protocol(format!("block {index} started twice")));
        }
        self.blocks.insert(
            index,
            Block {
                body,
                open: true,
                meta,
            },
        );
        Ok(())
    }

    fn append(&mut self, index: usize, kind: Kind, piece: &str) -> Result<()> {
        // `saturating_add`:加到上限就停在上限,不會溢位。這裡的數字來自外部輸入,不能假設它很小。
        let total = self.bytes.saturating_add(piece.len());
        if total > self.max_bytes {
            return Err(LlmError::protocol(format!(
                "response exceeds {} bytes",
                self.max_bytes
            )));
        }
        let block = self.open_block(index, kind)?;
        // 一個 `match` 分支可以用 `|` 同時比對好幾種,只要它們綁定的變數型別一樣。
        let buffer = match &mut block.body {
            Body::Text(s) | Body::Thinking(s) => s,
            Body::ToolCall { args, .. } => args,
        };
        buffer.push_str(piece);
        self.bytes = total;
        Ok(())
    }

    fn close(&mut self, index: usize, kind: Kind, meta: ProviderMetadata) -> Result<()> {
        let block = self.open_block(index, kind)?;
        block.open = false;
        block.meta.merge(meta);
        Ok(())
    }

    /// 找出 `index` 的區塊,並確認它還開著、種類也對。
    // 回傳 `&mut Block`:借給呼叫者修改,但所有權還是在 `self.blocks` 手上。
    // Java 裡回傳物件參考,呼叫者想改就改;Rust 裡「可以改」必須寫進型別(`&mut`)。
    fn open_block(&mut self, index: usize, kind: Kind) -> Result<&mut Block> {
        // `ok_or_else` 把 `Option`(有或沒有)轉成 `Result`(成功或錯誤),再用 `?` 往外傳。
        let block = self.blocks.get_mut(&index).ok_or_else(|| {
            LlmError::protocol(format!("{kind:?} event for unknown block {index}"))
        })?;
        if !block.open {
            return Err(LlmError::protocol(format!(
                "{kind:?} event for closed block {index}"
            )));
        }
        if block.body.kind() != kind {
            let actual = block.body.kind();
            return Err(LlmError::protocol(format!(
                "{kind:?} event for {actual:?} block {index}"
            )));
        }
        Ok(block)
    }

    /// 是否已經收到 `Finish`。
    pub(crate) fn is_finished(&self) -> bool {
        self.phase == Phase::Finished
    }

    /// 串流中途斷掉時,把目前收到的部分組成一份不完整的回應(包含還沒結束的區塊)。
    pub(crate) fn into_partial(self) -> Response {
        Response {
            id: self.id,
            model: self.model,
            content: self.blocks.into_values().map(Block::into_content).collect(),
            stop: StopReason {
                kind: StopKind::Unknown,
                raw: "interrupted".into(),
            },
            usage: Usage::default(),
            meta: ProviderMetadata::default(),
        }
    }

    /// 串流結束,取出完整回應。
    // `self`(不是 `&self`):呼叫之後 Accumulator 就被消耗掉了,compiler 不准你再 `push`。
    // 「結束後不能再用」這條規則由型別系統保證,不必在執行時檢查。Java 做不到這一點。
    pub fn finish(self) -> Result<Response> {
        // `let ... else`:模式不符合就執行 else(而且 else 必須離開函式)。
        let Some(finished) = self.finished else {
            return Err(LlmError::protocol("stream ended before finish"));
        };
        let content = self.blocks.into_values().map(Block::into_content).collect();
        Ok(Response {
            id: self.id,
            model: self.model,
            content,
            stop: finished.stop,
            usage: finished.usage,
            meta: finished.meta,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Content, StopKind, StopReason, Usage};
    use serde_json::json;

    fn meta() -> ProviderMetadata {
        ProviderMetadata::default()
    }

    fn start() -> StreamEvent {
        StreamEvent::MessageStart {
            id: Some("msg_1".into()),
            model: "m".into(),
        }
    }

    fn finish(kind: StopKind, raw: &str) -> StreamEvent {
        StreamEvent::Finish {
            stop: StopReason {
                kind,
                raw: raw.into(),
            },
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
            meta: meta(),
        }
    }

    fn run(events: Vec<StreamEvent>) -> Result<Response> {
        let mut acc = Accumulator::new();
        for ev in events {
            acc.push(ev)?;
        }
        acc.finish()
    }

    /// 前面的事件都必須被接受,只有最後一個被拒絕(而且是 `Protocol` 錯誤)。
    // 只檢查「最後有沒有錯」不夠:規則壞掉時,錯誤可能改在 `finish()` 才冒出來,
    // 測試會用錯誤的理由通過。mutation 檢查抓到過這個問題,所以要精確到「是哪一個事件被拒絕」。
    fn rejects_last(events: Vec<StreamEvent>) -> bool {
        let last = events.len() - 1;
        let mut acc = Accumulator::new();
        for (i, ev) in events.into_iter().enumerate() {
            let result = acc.push(ev);
            if i < last {
                if result.is_err() {
                    return false;
                }
            } else {
                return matches!(result, Err(LlmError::Protocol(_)));
            }
        }
        false
    }

    #[test]
    fn text_deltas_accumulate_into_one_block() {
        let resp = run(vec![
            start(),
            StreamEvent::TextStart {
                index: 0,
                meta: meta(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Hel".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "lo".into(),
            },
            StreamEvent::TextEnd {
                index: 0,
                meta: meta(),
            },
            finish(StopKind::EndTurn, "end_turn"),
        ])
        .unwrap();
        assert_eq!(resp.id.as_deref(), Some("msg_1"));
        assert_eq!(
            resp.content,
            vec![Content::Text {
                text: "Hello".into(),
                meta: meta()
            }]
        );
        assert_eq!(resp.stop.kind, StopKind::EndTurn);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn tool_args_are_concatenated_but_never_parsed() {
        // 參數是壞掉的 JSON 也照收:這一層只負責拼接,驗證是工具層的事。
        let resp = run(vec![
            start(),
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "read".into(),
                meta: meta(),
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                partial_json: "{\"pa".into(),
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                partial_json: "th\": [bad".into(),
            },
            StreamEvent::ToolCallEnd {
                index: 0,
                meta: meta(),
            },
            finish(StopKind::ToolUse, "tool_use"),
        ])
        .unwrap();
        assert_eq!(
            resp.content,
            vec![Content::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                args_json: "{\"path\": [bad".into(),
                meta: meta(),
            }]
        );
    }

    #[test]
    fn interleaved_parallel_tool_calls_are_kept_apart_by_index() {
        let resp = run(vec![
            start(),
            StreamEvent::ToolCallStart {
                index: 1,
                id: "a".into(),
                name: "read".into(),
                meta: meta(),
            },
            StreamEvent::ToolCallStart {
                index: 2,
                id: "b".into(),
                name: "read".into(),
                meta: meta(),
            },
            StreamEvent::ToolArgsDelta {
                index: 1,
                partial_json: "A1".into(),
            },
            StreamEvent::ToolArgsDelta {
                index: 2,
                partial_json: "B1".into(),
            },
            StreamEvent::ToolArgsDelta {
                index: 1,
                partial_json: "A2".into(),
            },
            StreamEvent::ToolArgsDelta {
                index: 2,
                partial_json: "B2".into(),
            },
            StreamEvent::ToolCallEnd {
                index: 2,
                meta: meta(),
            },
            StreamEvent::ToolCallEnd {
                index: 1,
                meta: meta(),
            },
            finish(StopKind::ToolUse, "tool_use"),
        ])
        .unwrap();
        let args: Vec<&str> = resp
            .content
            .iter()
            .map(|c| match c {
                Content::ToolCall { args_json, .. } => args_json.as_str(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(args, ["A1A2", "B1B2"]);
    }

    #[test]
    fn end_meta_is_merged_into_the_block() {
        // thinking 簽章在區塊結束時才送來,必須留在最後的 Content 裡。
        let mut sig = ProviderMetadata::default();
        sig.insert("anthropic", json!({ "signature": "sig" }));
        let resp = run(vec![
            start(),
            StreamEvent::ThinkingStart {
                index: 0,
                meta: meta(),
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                text: "hmm".into(),
            },
            StreamEvent::ThinkingEnd {
                index: 0,
                meta: sig.clone(),
            },
            finish(StopKind::EndTurn, "end_turn"),
        ])
        .unwrap();
        assert_eq!(
            resp.content,
            vec![Content::Thinking {
                text: "hmm".into(),
                meta: sig
            }]
        );
    }

    #[test]
    fn event_before_message_start_is_rejected() {
        assert!(rejects_last(vec![StreamEvent::TextStart {
            index: 0,
            meta: meta()
        }]));
    }

    #[test]
    fn delta_for_unknown_block_is_rejected() {
        assert!(rejects_last(vec![
            start(),
            StreamEvent::TextDelta {
                index: 7,
                text: "x".into()
            },
        ]));
    }

    #[test]
    fn delta_of_the_wrong_kind_is_rejected() {
        assert!(rejects_last(vec![
            start(),
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c".into(),
                name: "n".into(),
                meta: meta()
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "x".into()
            },
        ]));
    }

    #[test]
    fn duplicate_block_start_is_rejected() {
        assert!(rejects_last(vec![
            start(),
            StreamEvent::TextStart {
                index: 0,
                meta: meta()
            },
            StreamEvent::TextStart {
                index: 0,
                meta: meta()
            },
        ]));
    }

    #[test]
    fn delta_after_block_end_is_rejected() {
        assert!(rejects_last(vec![
            start(),
            StreamEvent::TextStart {
                index: 0,
                meta: meta()
            },
            StreamEvent::TextEnd {
                index: 0,
                meta: meta()
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "late".into()
            },
        ]));
    }

    #[test]
    fn finish_with_an_open_block_is_rejected() {
        assert!(rejects_last(vec![
            start(),
            StreamEvent::TextStart {
                index: 0,
                meta: meta()
            },
            finish(StopKind::EndTurn, "end_turn"),
        ]));
    }

    #[test]
    fn event_after_finish_is_rejected() {
        assert!(rejects_last(vec![
            start(),
            finish(StopKind::EndTurn, "end_turn"),
            StreamEvent::TextStart {
                index: 0,
                meta: meta()
            },
        ]));
    }

    #[test]
    fn stream_without_finish_cannot_be_completed() {
        assert!(matches!(run(vec![start()]), Err(LlmError::Protocol(_))));
    }

    #[test]
    fn output_beyond_the_size_limit_is_rejected() {
        let mut acc = Accumulator::with_max_bytes(8);
        acc.push(start()).unwrap();
        acc.push(StreamEvent::TextStart {
            index: 0,
            meta: meta(),
        })
        .unwrap();
        acc.push(StreamEvent::TextDelta {
            index: 0,
            text: "12345".into(),
        })
        .unwrap();
        let r = acc.push(StreamEvent::TextDelta {
            index: 0,
            text: "6789".into(),
        });
        assert!(matches!(r, Err(LlmError::Protocol(_))));
    }
}
