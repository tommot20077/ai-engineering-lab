//! Server-Sent Events(SSE)解析器:把網路送來的位元組切成一個個事件。
//!
//! 跟任何 provider 都無關,只照 WHATWG 的 SSE 規格走。純邏輯,沒有 I/O。
//!
//! ```text
//! event: content_block_delta        ← 事件名稱(可省略)
//! data: {"type":"text_delta",...}   ← 資料,可以有好幾行
//!                                   ← 空行 = 這個事件結束
//! ```
//!
//! 網路不保證一次送來一整個事件:一塊資料可能只有半行,甚至半個中文字。
//! 所以解析器要把「還沒收完的那一行」留著,等下一塊資料來了再接著處理(增量解析)。

use std::fmt;

use crate::error::{LlmError, Result};

/// 單一事件最多多少 bytes(design.md §7)。惡意或壞掉的串流可能永遠不送換行,不能無限累積。
pub(crate) const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// UTF-8 的 BOM。有些伺服器會在串流最開頭送它,規格要求忽略。
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// 解析出來的一個事件。
#[derive(Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 欄位;沒有就是 `None`。
    pub event: Option<String>,
    /// 所有 `data:` 行,用 `\n` 接起來。
    pub data: String,
}

/// 增量式的 SSE 解析器:一次餵一塊位元組,吐出這塊資料湊滿的事件。
pub struct SseParser {
    /// 還沒遇到換行的那一行(原始位元組)。
    line: Vec<u8>,
    event_type: String,
    data: String,
    /// 這個事件有沒有出現過 `data:`。光看 `data.is_empty()` 不夠:`data:` 後面可以是空的。
    has_data: bool,
    /// 上一個位元組是 `\r`。`\r\n` 是一個換行,不是兩個;而且 `\r` 和 `\n` 可能被切在兩塊資料裡。
    after_cr: bool,
    at_start: bool,
    max_event_bytes: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_max_event_bytes(MAX_EVENT_BYTES)
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_event_bytes(max_event_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            event_type: String::new(),
            data: String::new(),
            has_data: false,
            after_cr: false,
            at_start: true,
            max_event_bytes,
        }
    }

    /// 餵入一塊位元組,回傳這塊資料湊滿的所有事件(可能是零個)。
    // `chunk: &[u8]` 是借來的一段位元組(Java 的 `byte[]`,但只能讀、不能留著)。
    // 需要跨呼叫保存的部分,我們自己複製進 `self.line`。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>> {
        let mut events = Vec::new();
        // `&b`:迭代器給的是每個位元組的參考,`&b` 這個模式直接把它解開成 `u8` 的值。
        for &b in chunk {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut events)?;
                }
                _ => {
                    self.line.push(b);
                    if self.line.len() + self.data.len() > self.max_event_bytes {
                        return Err(LlmError::protocol(format!(
                            "SSE event exceeds {} bytes",
                            self.max_event_bytes
                        )));
                    }
                }
            }
        }
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> Result<()> {
        // `mem::take` 把 `self.line` 的內容拿走,原地換成一個空的 Vec。
        // 為什麼不直接用 `&self.line`?因為下面要修改 `self.data`:
        // 借用 `self` 的一部分來讀、同時又修改 `self` 的另一部分,在這裡會讓 borrow checker 很難確認安全。
        // 先把那一行「搬」成區域變數,就沒有衝突了。順便,原本的緩衝區也不必複製。
        let raw = std::mem::take(&mut self.line);
        let mut bytes = raw.as_slice();
        if self.at_start {
            self.at_start = false;
            bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
        }
        // 換行字元一定是單一 byte,所以「先切行、再解碼」不會切壞任何多 byte 的字。
        let line = std::str::from_utf8(bytes)
            .map_err(|_| LlmError::protocol("SSE line is not valid UTF-8"))?;

        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(()); // 註解行,常用來保持連線(keep-alive)
        }
        // 沒有冒號的行:整行都是欄位名稱,值是空的。
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => value.clone_into(&mut self.event_type),
            // `id` 和 `retry` 是斷線重連用的,LLM API 不支援從中斷處續傳,所以忽略;未知欄位依規格也忽略。
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = std::mem::take(&mut self.event_type);
        let data = std::mem::take(&mut self.data);
        // `mem::replace` 放進新值、取回舊值:一行同時做到「讀出旗標」和「重設旗標」。
        if std::mem::replace(&mut self.has_data, false) {
            // `bool::then_some`:true 就是 `Some(event)`,false 就是 `None`。
            events.push(SseEvent {
                event: (!event.is_empty()).then_some(event),
                data,
            });
        }
    }
}

impl fmt::Debug for SseEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseEvent")
            .field("event", &self.event)
            .field("data_bytes", &self.data.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmError;
    use proptest::prelude::*;

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    fn parse(input: &str) -> Vec<SseEvent> {
        SseParser::new().feed(input.as_bytes()).unwrap()
    }

    #[test]
    fn named_event_with_data() {
        let got = parse("event: message_start\ndata: {\"a\":1}\n\n");
        assert_eq!(got, vec![ev(Some("message_start"), "{\"a\":1}")]);
    }

    #[test]
    fn multiple_data_lines_join_with_newline() {
        assert_eq!(parse("data: a\ndata: b\n\n"), vec![ev(None, "a\nb")]);
    }

    #[test]
    fn crlf_and_cr_line_endings_work() {
        assert_eq!(
            parse("data: a\r\n\r\ndata: b\r\r"),
            vec![ev(None, "a"), ev(None, "b")]
        );
    }

    #[test]
    fn crlf_split_across_chunks_is_one_line_ending() {
        // 若把第二塊開頭的 \n 當成空行,會在 "a" 就提早送出事件,變成兩個事件。
        let mut p = SseParser::new();
        let mut got = p.feed(b"data: a\r").unwrap();
        got.extend(p.feed(b"\ndata: b\n\n").unwrap());
        assert_eq!(got, vec![ev(None, "a\nb")]);
    }

    #[test]
    fn comments_ids_retries_and_unknown_fields_are_ignored() {
        let got = parse(": ping\nid: 7\nretry: 100\nfoo: bar\ndata: x\n\n");
        assert_eq!(got, vec![ev(None, "x")]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        assert_eq!(
            parse("data:  two\n\ndata:none\n\n"),
            vec![ev(None, " two"), ev(None, "none")]
        );
    }

    #[test]
    fn blank_lines_without_data_dispatch_nothing() {
        assert_eq!(parse("\n\nevent: x\n\n"), vec![]);
    }

    #[test]
    fn unfinished_event_is_not_emitted() {
        assert_eq!(parse("data: half"), vec![]);
    }

    #[test]
    fn utf8_split_byte_by_byte_still_decodes() {
        let input = "data: 你好\n\n".as_bytes();
        let mut p = SseParser::new();
        let mut got = Vec::new();
        for b in input {
            got.extend(p.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got, vec![ev(None, "你好")]);
    }

    #[test]
    fn leading_bom_is_stripped() {
        assert_eq!(parse("\u{feff}data: x\n\n"), vec![ev(None, "x")]);
    }

    #[test]
    fn invalid_utf8_is_a_protocol_error() {
        let r = SseParser::new().feed(b"data: \xff\n\n");
        assert!(matches!(r, Err(LlmError::Protocol(_))));
    }

    #[test]
    fn oversized_event_is_rejected_even_without_a_newline() {
        let mut p = SseParser::with_max_event_bytes(16);
        let r = p.feed(&[b'a'; 64]);
        assert!(matches!(r, Err(LlmError::Protocol(_))));
    }

    #[test]
    fn oversized_event_across_many_data_lines_is_rejected() {
        let mut p = SseParser::with_max_event_bytes(16);
        let r = p.feed(b"data: 123456\ndata: 123456\ndata: 123456\n");
        assert!(matches!(r, Err(LlmError::Protocol(_))));
    }

    #[test]
    fn debug_hides_event_data() {
        let out = format!("{:?}", ev(Some("x"), "DATA-CANARY"));
        assert!(!out.contains("DATA-CANARY"), "{out}");
    }

    // ---- property tests:讓 proptest 自動產生幾百組輸入 ----

    proptest! {
        /// 不管收到什麼位元組、怎麼切,都只能回 Ok 或 Err,絕不 panic。
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512), cut in 0usize..512) {
            let mut p = SseParser::new();
            let (a, b) = bytes.split_at(cut.min(bytes.len()));
            let _ = p.feed(a);
            let _ = p.feed(b);
        }

        /// 同一串資料,不管被網路切成幾塊、從哪裡切,解出來的事件都一樣,而且就是原本那些事件。
        #[test]
        fn chunking_never_changes_the_events(
            events in proptest::collection::vec(
                (proptest::option::of("[a-z_]{1,12}"), proptest::collection::vec("[^\r\n]{0,20}", 1..4)),
                0..6,
            ),
            ending in prop_oneof![Just("\n"), Just("\r\n"), Just("\r")],
            cuts in proptest::collection::vec(any::<proptest::sample::Index>(), 0..8),
        ) {
            let mut wire = String::new();
            for (name, lines) in &events {
                if let Some(name) = name {
                    wire.push_str(&format!("event: {name}{ending}"));
                }
                for line in lines {
                    wire.push_str(&format!("data: {line}{ending}"));
                }
                wire.push_str(ending);
            }
            let bytes = wire.as_bytes();
            let expected: Vec<SseEvent> = events
                .iter()
                .map(|(name, lines)| SseEvent { event: name.clone(), data: lines.join("\n") })
                .collect();

            let mut points: Vec<usize> = cuts.iter().map(|i| i.index(bytes.len() + 1)).collect();
            points.sort_unstable();
            points.push(bytes.len());

            let mut p = SseParser::new();
            let mut got = Vec::new();
            let mut prev = 0;
            for point in points {
                got.extend(p.feed(&bytes[prev..point]).unwrap());
                prev = point;
            }
            prop_assert_eq!(got, expected);
        }
    }
}
