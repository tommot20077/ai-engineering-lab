//! 模型別名:請求寫 `cheap`,送出前換成真正的 model ID(design-part2.md §13、D-023)。

use std::collections::BTreeMap;

use futures::future::{self, BoxFuture};
use futures::stream;

use crate::error::{LlmError, Result};
use crate::provider::{EventStream, LlmProvider};
use crate::types::{Request, Response};

/// 別名表:別名 → 真正的 model ID。
///
/// `Alias` 只從 constructor 拿到這張表,不知道它從哪裡來(設定檔、之後 011 的執行中替換)。
// 用 BTreeMap 而不是 HashMap:key 有固定順序,錯誤訊息裡列出的別名每次都一樣,測試也穩定。
#[derive(Debug, Clone)]
pub struct AliasTable {
    map: BTreeMap<String, String>,
    strict: bool,
}

impl AliasTable {
    /// 嚴格模式:不在表裡的名稱一律拒絕。gateway 用這個,別名表就是「菜單」(D-023)。
    pub fn strict(map: BTreeMap<String, String>) -> Self {
        Self { map, strict: true }
    }

    /// 寬鬆模式:不在表裡就當成真正的 model ID 原樣送出。給自己寫的實驗程式用。
    pub fn lenient(map: BTreeMap<String, String>) -> Self {
        Self { map, strict: false }
    }

    /// 查表,回傳要真正送出的 model ID。
    fn resolve(&self, requested: &str) -> Result<String> {
        // `match` 加守衛條件 `if`,等於 Java 21 switch 的 `when`。由上往下比對,第一個符合的分支勝出。
        match self.map.get(requested) {
            Some(real) => Ok(real.clone()),
            None if self.strict => Err(LlmError::InvalidRequest(format!(
                "unknown model alias '{requested}'; available: {}",
                self.available()
            ))),
            None => Ok(requested.to_owned()),
        }
    }

    /// 列出可用的別名,給錯誤訊息用。
    fn available(&self) -> String {
        if self.map.is_empty() {
            return "(none)".to_owned();
        }
        // `keys()` 依 BTreeMap 的順序(字母順)走過所有 key;`map(String::as_str)` 把 `&String` 變成 `&str`;
        // `collect::<Vec<_>>()` 收成陣列,再用 `join` 接成一個字串。
        // Java 對照:`map.keySet().stream().collect(Collectors.joining(", "))`。
        self.map
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 把請求裡的別名換成真正的 model ID,再交給裡面那一層。
// `<P>` 是「還沒填的空格」:用的時候才決定裡面裝什麼。
// 測試填具體型別,gateway 填 `Arc<dyn LlmProvider>`(靠 provider.rs 的 blanket impl)。
// Java 對照:`class Alias<P extends LlmProvider> implements LlmProvider`,
// 但 Java 編譯後會把 P 擦掉;Rust 會替每一種填法各產生一份程式(monomorphization)。
#[derive(Debug, Clone)]
pub struct Alias<P> {
    inner: P,
    table: AliasTable,
}

impl<P> Alias<P> {
    pub fn new(inner: P, table: AliasTable) -> Self {
        Self { inner, table }
    }
}

// 「只要 P 是 provider,`Alias<P>` 也是 provider」。
// 條件 `P: LlmProvider` 不能省:下面要呼叫 `self.inner.stream(...)`,compiler 得確定 P 有這個方法。
impl<P: LlmProvider> LlmProvider for Alias<P> {
    fn stream(&self, mut request: Request) -> EventStream {
        match self.table.resolve(&request.model) {
            Ok(model) => {
                request.model = model;
                self.inner.stream(request)
            }
            // 不碰網路:直接回傳「只有一個錯誤」的串流。錯誤是串流的一個項目(D-010)。
            Err(e) => Box::pin(stream::once(future::ready(Err(e)))),
        }
    }

    // decorator 的規則:兩個方法都要實作,各自交給裡面那一層的「同名」方法。
    // 如果只寫 `stream`,`complete` 會走 trait 的預設方法,裡面那一層覆寫的 `complete`
    // (例如 `AnthropicClient` 用自己的 `Limits`)就被跳過了。
    fn complete(&self, mut request: Request) -> BoxFuture<'_, Result<Response>> {
        match self.table.resolve(&request.model) {
            Ok(model) => {
                request.model = model;
                self.inner.complete(request)
            }
            Err(e) => Box::pin(future::ready(Err(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::StreamExt;

    use super::*;
    use crate::middleware::LlmProviderExt;
    use crate::{ProviderMetadata, StopKind, StopReason, Usage};

    /// 假的 provider:記下每次被呼叫的方法與收到的 model,不回傳任何內容。
    // `Mutex`:`stream(&self)` 只拿到唯讀的借用,要在裡面修改紀錄就得用鎖。
    // Java 對照:像是 `Collections.synchronizedList`。
    #[derive(Default)]
    struct Capture {
        seen: Mutex<Vec<String>>,
    }

    impl Capture {
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl LlmProvider for Capture {
        fn stream(&self, request: Request) -> EventStream {
            self.seen
                .lock()
                .unwrap()
                .push(format!("stream:{}", request.model));
            Box::pin(stream::empty())
        }

        fn complete(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
            self.seen
                .lock()
                .unwrap()
                .push(format!("complete:{}", request.model));
            Box::pin(future::ready(Ok(Response {
                id: None,
                model: request.model,
                content: vec![],
                stop: StopReason {
                    kind: StopKind::EndTurn,
                    raw: "end_turn".into(),
                },
                usage: Usage::default(),
                meta: ProviderMetadata::default(),
            })))
        }
    }

    fn table() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("default".to_owned(), "claude-sonnet-5-5".to_owned()),
            ("cheap".to_owned(), "claude-haiku-5-5".to_owned()),
        ])
    }

    fn request(model: &str) -> Request {
        Request {
            model: model.into(),
            system: None,
            messages: vec![],
            tools: vec![],
            max_tokens: 1,
            provider_options: ProviderMetadata::default(),
        }
    }

    #[tokio::test]
    async fn known_alias_is_replaced_and_each_method_reaches_its_counterpart() {
        // 自己留一份 `Arc`,包進 Alias 之後還能檢查假 provider 收到什麼。
        // `Alias<Arc<Capture>>` 能成立,靠的就是 provider.rs 的 blanket impl。
        let inner = Arc::new(Capture::default());
        let p = Arc::clone(&inner).with_alias(AliasTable::strict(table()));

        let _ = p.stream(request("cheap"));
        let resp = p.complete(request("default")).await.unwrap();

        assert_eq!(resp.model, "claude-sonnet-5-5");
        // 「complete:」證明 Alias 的 complete 交給了裡面的 complete,而不是走預設方法收集 stream。
        assert_eq!(
            inner.seen(),
            ["stream:claude-haiku-5-5", "complete:claude-sonnet-5-5"]
        );
    }

    #[tokio::test]
    async fn strict_mode_rejects_an_unknown_name_without_calling_inner() {
        let inner = Arc::new(Capture::default());
        let p = Arc::clone(&inner).with_alias(AliasTable::strict(table()));

        let first = p.stream(request("chaep")).next().await.unwrap();
        let err = p.complete(request("chaep")).await.unwrap_err();

        for e in [first.unwrap_err(), err] {
            let LlmError::InvalidRequest(message) = e else {
                panic!("{e:?}")
            };
            // 打錯字的人要能立刻看到有哪些可以用。
            assert_eq!(
                message,
                "unknown model alias 'chaep'; available: cheap, default"
            );
        }
        assert!(inner.seen().is_empty(), "不應該碰到裡面那一層");
    }

    #[tokio::test]
    async fn lenient_mode_passes_an_unknown_name_through_unchanged() {
        let inner = Arc::new(Capture::default());
        let p = Arc::clone(&inner).with_alias(AliasTable::lenient(table()));

        let _ = p.stream(request("claude-opus-5-5"));
        let _ = p.stream(request("cheap"));

        assert_eq!(
            inner.seen(),
            ["stream:claude-opus-5-5", "stream:claude-haiku-5-5"]
        );
    }

    #[test]
    fn empty_strict_table_says_so() {
        let err = AliasTable::strict(BTreeMap::new())
            .resolve("cheap")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid request: unknown model alias 'cheap'; available: (none)"
        );
    }
}
