//! 中立的串流事件(design.md §3、D-010)。
//!
//! 每個 provider adapter 把自家的串流格式翻譯成這些事件;上層只認得這些事件。
//! `index` 標示「這一片屬於回覆裡的第幾段」:平行的工具呼叫會交錯送來,靠它才拼得回去。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::stop::StopReason;
use crate::types::{ProviderMetadata, Usage};

/// 串流中的一個事件。
///
/// 每種段落(文字、思考、工具呼叫)都有 start / delta / end 三種事件。
// 一樣不 derive `Debug`:delta 事件帶的就是模型輸出的內容。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// 一則回覆開始。一定是第一個事件,而且只有一次。
    MessageStart {
        id: Option<String>,
        model: String,
    },

    TextStart {
        index: usize,
        meta: ProviderMetadata,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    TextEnd {
        index: usize,
        meta: ProviderMetadata,
    },

    ThinkingStart {
        index: usize,
        meta: ProviderMetadata,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    /// thinking 簽章之類只在結束時才知道的資料,放在 `meta` 裡。
    ThinkingEnd {
        index: usize,
        meta: ProviderMetadata,
    },

    ToolCallStart {
        index: usize,
        id: String,
        name: String,
        meta: ProviderMetadata,
    },
    /// 工具參數的一個片段。片段本身通常不是合法的 JSON,要全部接起來才是(或者也不是)。
    ToolArgsDelta {
        index: usize,
        partial_json: String,
    },
    ToolCallEnd {
        index: usize,
        meta: ProviderMetadata,
    },

    /// 回覆結束。一定是最後一個事件。
    Finish {
        stop: StopReason,
        usage: Usage,
        meta: ProviderMetadata,
    },
}

impl StreamEvent {
    /// 事件種類的名稱,跟序列化後的 `"type"` 欄位一樣(例如 `"text_delta"`)。
    ///
    /// gateway 的 SSE 用它當 `event:` 的名稱,瀏覽器才能用 `addEventListener("text_delta", ...)`(D-023)。
    // 沒有 `_ =>`:新增事件種類時,compiler 會逼我們在這裡替它取名字。
    // 跟 serde 的名字一致,由下面的測試保證。
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::TextStart { .. } => "text_start",
            Self::TextDelta { .. } => "text_delta",
            Self::TextEnd { .. } => "text_end",
            Self::ThinkingStart { .. } => "thinking_start",
            Self::ThinkingDelta { .. } => "thinking_delta",
            Self::ThinkingEnd { .. } => "thinking_end",
            Self::ToolCallStart { .. } => "tool_call_start",
            Self::ToolArgsDelta { .. } => "tool_args_delta",
            Self::ToolCallEnd { .. } => "tool_call_end",
            Self::Finish { .. } => "finish",
        }
    }
}

impl fmt::Debug for StreamEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 只有三種 delta 帶模型輸出的內容,只把它們換成長度;其他事件照常印出。
        match self {
            Self::TextDelta { index, text } => delta(f, "TextDelta", *index, text),
            Self::ThinkingDelta { index, text } => delta(f, "ThinkingDelta", *index, text),
            Self::ToolArgsDelta {
                index,
                partial_json,
            } => delta(f, "ToolArgsDelta", *index, partial_json),
            Self::MessageStart { id, model } => f
                .debug_struct("MessageStart")
                .field("id", id)
                .field("model", model)
                .finish(),
            Self::TextStart { index, meta } => block(f, "TextStart", *index, meta),
            Self::TextEnd { index, meta } => block(f, "TextEnd", *index, meta),
            Self::ThinkingStart { index, meta } => block(f, "ThinkingStart", *index, meta),
            Self::ThinkingEnd { index, meta } => block(f, "ThinkingEnd", *index, meta),
            Self::ToolCallStart {
                index,
                id,
                name,
                meta,
            } => f
                .debug_struct("ToolCallStart")
                .field("index", index)
                .field("id", id)
                .field("name", name)
                .field("meta", meta)
                .finish(),
            Self::ToolCallEnd { index, meta } => block(f, "ToolCallEnd", *index, meta),
            Self::Finish { stop, usage, meta } => f
                .debug_struct("Finish")
                .field("stop", stop)
                .field("usage", usage)
                .field("meta", meta)
                .finish(),
        }
    }
}

// 兩個小幫手,避免上面每個分支都重複一樣的四行。
fn delta(f: &mut fmt::Formatter<'_>, name: &str, index: usize, content: &str) -> fmt::Result {
    f.debug_struct(name)
        .field("index", &index)
        .field("bytes", &content.len())
        .finish()
}

fn block(
    f: &mut fmt::Formatter<'_>,
    name: &str,
    index: usize,
    meta: &ProviderMetadata,
) -> fmt::Result {
    f.debug_struct(name)
        .field("index", &index)
        .field("meta", meta)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_serializes_with_type_tag() {
        let ev = StreamEvent::TextDelta {
            index: 0,
            text: "hi".into(),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "text_delta", "index": 0, "text": "hi" })
        );
    }

    #[test]
    fn type_name_matches_the_serialized_type_tag_for_every_variant() {
        let meta = ProviderMetadata::default;
        let all = [
            StreamEvent::MessageStart {
                id: None,
                model: "m".into(),
            },
            StreamEvent::TextStart {
                index: 0,
                meta: meta(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "t".into(),
            },
            StreamEvent::TextEnd {
                index: 0,
                meta: meta(),
            },
            StreamEvent::ThinkingStart {
                index: 0,
                meta: meta(),
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                text: "t".into(),
            },
            StreamEvent::ThinkingEnd {
                index: 0,
                meta: meta(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "i".into(),
                name: "n".into(),
                meta: meta(),
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                partial_json: "{".into(),
            },
            StreamEvent::ToolCallEnd {
                index: 0,
                meta: meta(),
            },
            StreamEvent::Finish {
                stop: crate::StopReason {
                    kind: crate::StopKind::EndTurn,
                    raw: "end_turn".into(),
                },
                usage: Usage::default(),
                meta: meta(),
            },
        ];
        for event in all {
            let tag = serde_json::to_value(&event).unwrap()["type"].clone();
            assert_eq!(tag, event.type_name(), "{event:?}");
        }
    }

    #[test]
    fn debug_hides_streamed_content() {
        let events = [
            StreamEvent::TextDelta {
                index: 0,
                text: "TEXT-CANARY".into(),
            },
            StreamEvent::ThinkingDelta {
                index: 1,
                text: "THINK-CANARY".into(),
            },
            StreamEvent::ToolArgsDelta {
                index: 2,
                partial_json: "ARGS-CANARY".into(),
            },
        ];
        for ev in &events {
            let out = format!("{ev:?}");
            assert!(!out.contains("CANARY"), "content leaked: {out}");
        }
    }
}
