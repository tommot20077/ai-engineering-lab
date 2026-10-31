//! 跟 provider 無關的介面(design.md §3、D-009)。
//!
//! 每個 provider 只要實作 `stream`;`complete` 是把 stream 收集起來的預設方法。

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::accumulate::Accumulator;
use crate::error::{LlmError, Result};
use crate::event::StreamEvent;
use crate::limits::Limits;
use crate::types::{Request, Response};

/// 一個 provider 回傳的事件流。
///
/// `BoxStream<'static, ...>`:放在 heap 上、型別被抹除的 stream,而且不借用任何東西。
// 為什麼要 `Box`?trait 裡的方法若回傳 `impl Stream`,每個 provider 的 stream 型別都不一樣,
// 就沒辦法寫 `Arc<dyn LlmProvider>`,在執行期挑 provider(gateway 會需要)。
// 裝箱之後型別統一了,代價是每次請求多一次 heap 配置,跟網路延遲比可以忽略(design.md §8)。
// Java 對照:Java 的介面方法回傳 `Iterator<T>`,本來就是透過參考、執行期才知道實際型別,
// Rust 要明確寫出 `Box<dyn ...>` 才有這種行為。
pub type EventStream = BoxStream<'static, Result<StreamEvent>>;

/// 一個 LLM provider。
// `Send + Sync`:這個 provider 可以被多個執行緒同時使用。gateway 會把它放進 `Arc` 給所有請求共用,
// compiler 會檢查實作是否真的安全。Java 沒有這種檢查,執行緒安全只能靠文件和自律。
pub trait LlmProvider: Send + Sync {
    /// 送出請求,回傳事件流。錯誤也是串流中的一個項目,出錯之後串流就結束。
    fn stream(&self, request: Request) -> EventStream;

    /// 送出請求,等串流結束,回傳完整的回應。
    // 這是預設方法(Java 的 `default` method),每個 provider 自動擁有,也可以覆寫。
    // 回傳 `BoxFuture` 而不是寫成 `async fn`:trait 裡的 `async fn` 不能用在 `dyn LlmProvider` 上。
    fn complete(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(collect(self.stream(request), Limits::default()))
    }
}

/// 「裝在 `Arc` 裡的 provider,本身也是 provider」(design-part2.md §12、D-019)。
///
/// 有了它,decorator 的模具 `Alias<P>` 才能填進 `Arc<dyn LlmProvider>`,
/// gateway 也才能依設定檔在執行期一層層組裝。
// 這叫 blanket impl:一次替「所有」`Arc<T>` 實作,只要 T 是 provider。
// Java 做不到這件事:不能事後讓 JDK 的類別(這裡的 `Arc`)去 implement 你自己的介面。
// `?Sized`:泛型的 T 預設必須在編譯期知道大小;`dyn LlmProvider` 的大小不固定(可能是任何實作),
// 加上 `?Sized` 才涵蓋它。少了它,這個 impl 偏偏對最需要的 `Arc<dyn LlmProvider>` 無效。
impl<T: LlmProvider + ?Sized> LlmProvider for std::sync::Arc<T> {
    fn stream(&self, request: Request) -> EventStream {
        // `self` 是 `&Arc<T>`,`*self` 是 `Arc<T>`,`**self` 是裡面的 T:打開盒子交給它。
        // 不能寫 `self.stream(request)`:那會呼叫這個函式自己,變成無窮遞迴。
        (**self).stream(request)
    }

    // 一定要轉發:裡面的 provider 可能覆寫了 `complete`(`AnthropicClient` 用自己的 `Limits`)。
    // 不寫的話,`Arc` 會悄悄改用 trait 的預設方法,上限變回預設值,編譯器不會提醒。
    fn complete(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        (**self).complete(request)
    }
}

/// 把事件流收集成完整的回應,並套用 design.md §6 的中途失敗規則:
/// 還沒輸出任何內容就失敗 → 原本的錯誤(可能可以重試);已經輸出過才失敗 → `Interrupted`(不可重試)。
// `async fn`:呼叫它不會馬上執行,而是回傳一個 Future,要 `.await` 才會跑。
// Java 對照:很像回傳 `CompletableFuture`,但 Rust 的 Future 是「懶的」,沒人 await 就什麼都不做。
pub async fn collect(mut events: EventStream, limits: Limits) -> Result<Response> {
    let mut acc = Accumulator::with_max_bytes(limits.max_response_bytes);
    let mut produced = false;
    // `while let`:只要 `next().await` 還拿得到 `Some`,就繼續迴圈。
    while let Some(item) = events.next().await {
        match item {
            Ok(event) => {
                produced |= is_output(&event);
                acc.push(event)?;
            }
            Err(cause) if produced => return Err(interrupted(acc, cause)),
            Err(cause) => return Err(cause),
        }
    }
    if produced && !acc.is_finished() {
        let cause = LlmError::protocol("stream ended before finish");
        return Err(interrupted(acc, cause));
    }
    acc.finish()
}

/// 這個事件是不是「已經給使用者看到的內容」。
// `pub(crate)`:Retry 也用同一個定義決定「還能不能重試」,規則只寫在一個地方。
pub(crate) fn is_output(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta { .. }
            | StreamEvent::ThinkingDelta { .. }
            | StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolArgsDelta { .. }
    )
}

fn interrupted(acc: Accumulator, cause: LlmError) -> LlmError {
    LlmError::Interrupted {
        partial: Box::new(acc.into_partial()),
        cause: Box::new(cause),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::stream;

    use super::*;
    use crate::{ProviderMetadata, StopKind, StopReason, Usage};

    /// 假的 provider:把事先準備好的事件原樣吐出來。
    // 錯誤存成「產生錯誤的函式」(`fn() -> LlmError`):`LlmError` 沒有 `Clone`,
    // 而每次呼叫 `stream` 都要重新產生一份。
    struct Scripted(Vec<std::result::Result<StreamEvent, fn() -> LlmError>>);

    impl LlmProvider for Scripted {
        fn stream(&self, _request: Request) -> EventStream {
            let items: Vec<Result<StreamEvent>> = self
                .0
                .iter()
                .map(|item| match item {
                    Ok(event) => Ok(event.clone()),
                    Err(make_error) => Err(make_error()),
                })
                .collect();
            Box::pin(stream::iter(items))
        }
    }

    fn request() -> Request {
        Request {
            model: "m".into(),
            system: None,
            messages: vec![],
            tools: vec![],
            max_tokens: 1,
            provider_options: ProviderMetadata::default(),
        }
    }

    fn start() -> StreamEvent {
        StreamEvent::MessageStart {
            id: None,
            model: "m".into(),
        }
    }

    #[test]
    fn provider_is_usable_as_a_trait_object() {
        // 能編譯就代表 trait 是 dyn-compatible:gateway 才能在執行期挑 provider。
        let _: Arc<dyn LlmProvider> = Arc::new(Scripted(vec![]));
    }

    /// 覆寫了 `complete` 的 provider,就像 `AnthropicClient`(它用自己的 `Limits`)。
    /// `stream` 是空的:如果有人沒呼叫到這個 `complete`、改走預設方法收集 stream,就會失敗。
    struct OwnComplete;

    impl LlmProvider for OwnComplete {
        fn stream(&self, _request: Request) -> EventStream {
            Box::pin(stream::empty())
        }

        fn complete(&self, _request: Request) -> BoxFuture<'_, Result<Response>> {
            Box::pin(async {
                Ok(Response {
                    id: None,
                    model: "from-own-complete".into(),
                    content: vec![],
                    stop: StopReason {
                        kind: StopKind::EndTurn,
                        raw: "end_turn".into(),
                    },
                    usage: Usage::default(),
                    meta: ProviderMetadata::default(),
                })
            })
        }
    }

    // 透過泛型呼叫,才會真的用到 `Arc<T>` 的 impl。
    // 直接寫 `arc.complete(...)` 時,Rust 可能自動解開 Arc 呼叫裡面的方法,測不到我們的 impl。
    async fn complete_via<P: LlmProvider>(provider: &P) -> Result<Response> {
        provider.complete(request()).await
    }

    async fn first_event_via<P: LlmProvider>(provider: &P) -> Option<Result<StreamEvent>> {
        provider.stream(request()).next().await
    }

    #[tokio::test]
    async fn arc_forwards_stream_to_the_inner_provider() {
        let p: Arc<dyn LlmProvider> = Arc::new(Scripted(vec![Ok(start())]));
        let first = first_event_via(&p).await.unwrap().unwrap();
        assert_eq!(first, start());
    }

    #[tokio::test]
    async fn arc_forwards_complete_instead_of_using_the_default() {
        // 盒子(`Arc<dyn>`)和具體型別(`Arc<OwnComplete>`)都要轉發到裡面那個覆寫過的 `complete`。
        let boxed: Arc<dyn LlmProvider> = Arc::new(OwnComplete);
        assert_eq!(
            complete_via(&boxed).await.unwrap().model,
            "from-own-complete"
        );
        let concrete = Arc::new(OwnComplete);
        assert_eq!(
            complete_via(&concrete).await.unwrap().model,
            "from-own-complete"
        );
    }

    #[tokio::test]
    async fn default_complete_collects_the_stream() {
        let p = Scripted(vec![
            Ok(start()),
            Ok(StreamEvent::Finish {
                stop: StopReason {
                    kind: StopKind::EndTurn,
                    raw: "end_turn".into(),
                },
                usage: Usage::default(),
                meta: ProviderMetadata::default(),
            }),
        ]);
        let resp = p.complete(request()).await.unwrap();
        assert_eq!(resp.stop.kind, StopKind::EndTurn);
    }

    #[tokio::test]
    async fn error_before_output_is_passed_through_unchanged() {
        let p = Scripted(vec![
            Ok(start()),
            Err(|| LlmError::Transport("reset".into())),
        ]);
        let err = p.complete(request()).await.unwrap_err();
        assert!(matches!(err, LlmError::Transport(_)), "{err:?}");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn error_after_output_becomes_interrupted_with_its_cause() {
        let p = Scripted(vec![
            Ok(start()),
            Ok(StreamEvent::TextStart {
                index: 0,
                meta: ProviderMetadata::default(),
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "par".into(),
            }),
            Err(|| LlmError::Transport("reset".into())),
        ]);
        let err = p.complete(request()).await.unwrap_err();
        let LlmError::Interrupted { partial, cause } = err else {
            panic!("{err:?}")
        };
        assert!(matches!(*cause, LlmError::Transport(_)));
        assert_eq!(
            partial.content,
            vec![crate::Content::Text {
                text: "par".into(),
                meta: ProviderMetadata::default()
            }]
        );
    }

    #[tokio::test]
    async fn start_events_alone_do_not_count_as_output() {
        let p = Scripted(vec![
            Ok(start()),
            Ok(StreamEvent::TextStart {
                index: 0,
                meta: ProviderMetadata::default(),
            }),
            Err(|| LlmError::Timeout),
        ]);
        let err = p.complete(request()).await.unwrap_err();
        assert!(matches!(err, LlmError::Timeout), "{err:?}");
    }
}
