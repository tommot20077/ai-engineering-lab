//! 輸出前的暫時性失敗自動重試(design-part2.md §14、D-022)。

use std::sync::Arc;
use std::time::Duration;

use async_stream::try_stream;
use futures::StreamExt;
use futures::future::BoxFuture;

use crate::error::{LlmError, Result};
use crate::provider::{EventStream, LlmProvider, is_output};
use crate::types::{Request, Response};

/// 重試的參數。預設值見 D-022;gateway 讀設定檔後從 constructor 傳進來(D-016)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 總共最多嘗試幾次(包含第一次)。
    pub max_attempts: u32,
    /// 第一次失敗後,等待時間的上限;之後每次加倍。
    pub first_delay: Duration,
    /// 任何一次等待的最長上限。對方的 `retry-after` 超過它,就不等了。
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            first_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    /// 第 `failed` 次失敗之後,等待時間的上限:`first_delay × 2^(failed-1)`,最多 `max_delay`。
    fn backoff_cap(&self, failed: u32) -> Duration {
        // `saturating_*`:超過上限就停在最大值,不會溢位。Java 的整數溢位會悄悄繞回負數;
        // Rust 在 debug 模式溢位會 panic,所以「可能很大」的計算一律明確選擇飽和、檢查或環繞。
        let factor = 2u32.saturating_pow(failed.saturating_sub(1));
        self.first_delay.saturating_mul(factor).min(self.max_delay)
    }

    /// 第 `failed` 次嘗試失敗了,要不要再試?要的話回傳等多久;`None` = 放棄,把錯誤交出去。
    fn next_wait(&self, failed: u32, error: &LlmError) -> Option<Duration> {
        if !error.is_retryable() || failed >= self.max_attempts {
            return None;
        }
        match error {
            // 對方說了要等多久:照它的,不加 jitter。但太久就不等,交給上層決定(D-022)。
            LlmError::Http {
                retry_after: Some(wait),
                ..
            } => (*wait <= self.max_delay).then_some(*wait),
            // full jitter:0 到上限之間隨機,避免很多客戶端在同一瞬間一起重試。
            _ => Some(self.backoff_cap(failed).mul_f64(fastrand::f64())),
        }
    }
}

/// 輸出前失敗就重試的 decorator。
pub struct Retry<P> {
    // 為什麼是 `Arc<P>` 而不是 `P`:`stream()` 會立刻回傳,真正的重試發生在之後被讀取的時候,
    // 那時 `&self` 這個借用早就結束了。串流必須「自己持有」一份 inner,才能再呼叫它。
    // Java 對照:像回傳一個捕捉了 `this.inner` 的 lambda;Java 有 GC 所以不用想,Rust 要明確多拿一份。
    inner: Arc<P>,
    policy: RetryPolicy,
}

impl<P> Retry<P> {
    pub fn new(inner: P, policy: RetryPolicy) -> Self {
        Self {
            inner: Arc::new(inner),
            policy,
        }
    }
}

// `'static`:回傳的串流可能活得比這次呼叫還久,所以 P 裡面不能有「借來、等一下要還」的東西。
// `AnthropicClient` 和 `Arc<dyn LlmProvider>` 都符合。
impl<P: LlmProvider + 'static> LlmProvider for Retry<P> {
    fn stream(&self, request: Request) -> EventStream {
        // 串流自己持有一份 inner 和 policy:`Arc::clone` 只是計數 +1,不複製 provider。
        let inner = Arc::clone(&self.inner);
        let policy = self.policy;

        Box::pin(try_stream! {
            let mut attempt = 1;
            // 外圈:每一圈是一次嘗試。取名字 `'attempts`,裡面才能用 `continue 'attempts` 指定回到「外圈」。
            // Java 也有一樣的語法:`attempts: while (true) { ... continue attempts; }`。
            'attempts: loop {
                let mut events = inner.stream(request.clone());
                // 第一個輸出之前的事件先扣住:MessageStart 這類事件一旦傳出去就收不回來,
                // 重試後再送一次,下游的 Accumulator 會判定為協定錯誤。
                // 每一圈都重新建立,所以重試時上一次扣住的事件自然被丟掉。
                let mut held = Vec::new();
                let mut produced = false;

                while let Some(item) = events.next().await {
                    match item {
                        // 已經在輸出了:直接轉送。
                        Ok(event) if produced => yield event,
                        // 第一個有內容的事件:依原順序放行扣住的事件,再送它。
                        Ok(event) if is_output(&event) => {
                            // `drain(..)`:一個個拿出來,同時清空 held。
                            for held_event in held.drain(..) {
                                yield held_event;
                            }
                            produced = true;
                            yield event;
                        }
                        // 還沒有內容:先扣住。
                        Ok(event) => held.push(event),
                        // 還沒輸出就失敗:看看能不能重試。
                        Err(error) if !produced => {
                            let Some(wait) = policy.next_wait(attempt, &error) else {
                                Err(error)?;
                                // `Err(..)?` 會把錯誤交給下游並結束串流,這行永遠不會執行到;
                                // 但 compiler 看不出來,`let ... else` 的 else 必須明確離開,所以補一個 return。
                                return;
                            };
                            // 營運 log:不含內容,`LlmError` 的 Display 只有狀態碼(第一部分 §7)。
                            tracing::warn!(attempt, error = %error, wait_ms = wait.as_millis() as u64, "retrying LLM call");
                            // `tokio::time::sleep`:只暫停這個任務,執行緒先去處理別的請求。
                            // Java 對照:virtual thread 上的 `Thread.sleep`。
                            tokio::time::sleep(wait).await;
                            attempt += 1;
                            continue 'attempts;
                        }
                        // 已經輸出過才失敗:不能重來,錯誤原樣往下傳(下游的 collect 會變成 Interrupted)。
                        Err(error) => Err(error)?,
                    }
                }
                // 整段都沒有輸出(例如空的回答):扣住的事件也要放行,不能吞掉。
                for held_event in held.drain(..) {
                    yield held_event;
                }
                break;
            }
        })
    }

    // 規則:交給裡面那一層的 `complete`(它可能用自己的 `Limits`)。
    // 輸出到一半斷線的 `Interrupted` 不可重試(`is_retryable`),所以整個重來是安全的。
    fn complete(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            let mut attempt = 1;
            loop {
                match self.inner.complete(request.clone()).await {
                    Err(error) => {
                        let Some(wait) = self.policy.next_wait(attempt, &error) else {
                            return Err(error);
                        };
                        tracing::warn!(attempt, error = %error, wait_ms = wait.as_millis() as u64, "retrying LLM call");
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                    }
                    ok => return ok,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::stream;
    use tokio::time::Instant;

    use super::*;
    use crate::event::StreamEvent;
    use crate::middleware::LlmProviderExt;
    use crate::{Limits, ProviderMetadata, StopKind, StopReason, Usage, collect};

    /// 一次嘗試要吐出的內容。錯誤存成函式,因為 `LlmError` 沒有 `Clone`。
    type Script = Vec<std::result::Result<StreamEvent, fn() -> LlmError>>;

    /// 假的 provider:每被呼叫一次,就照順序拿出下一份劇本,並記下被呼叫的是哪個方法。
    struct Attempts {
        scripts: Mutex<VecDeque<Script>>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Attempts {
        fn new(scripts: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                calls: Mutex::new(vec![]),
            })
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn next_script(&self) -> EventStream {
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            let items: Vec<Result<StreamEvent>> = script
                .into_iter()
                .map(|item| item.map_err(|make_error| make_error()))
                .collect();
            Box::pin(stream::iter(items))
        }
    }

    impl LlmProvider for Attempts {
        fn stream(&self, _request: Request) -> EventStream {
            self.calls.lock().unwrap().push("stream");
            self.next_script()
        }

        fn complete(&self, _request: Request) -> BoxFuture<'_, Result<Response>> {
            self.calls.lock().unwrap().push("complete");
            Box::pin(collect(self.next_script(), Limits::default()))
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

    fn text_start() -> StreamEvent {
        StreamEvent::TextStart {
            index: 0,
            meta: ProviderMetadata::default(),
        }
    }

    fn delta(text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            index: 0,
            text: text.into(),
        }
    }

    fn text_end() -> StreamEvent {
        StreamEvent::TextEnd {
            index: 0,
            meta: ProviderMetadata::default(),
        }
    }

    fn finish() -> StreamEvent {
        StreamEvent::Finish {
            stop: StopReason {
                kind: StopKind::EndTurn,
                raw: "end_turn".into(),
            },
            usage: Usage::default(),
            meta: ProviderMetadata::default(),
        }
    }

    fn answer() -> Script {
        vec![
            Ok(start()),
            Ok(text_start()),
            Ok(delta("你好")),
            Ok(text_end()),
            Ok(finish()),
        ]
    }

    fn overloaded() -> LlmError {
        LlmError::http(529, None, "")
    }

    fn status(err: &LlmError) -> Option<u16> {
        match err {
            LlmError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// 把整個串流讀完:成功的事件收成一個 Vec,錯誤(最多一個,而且一定在最後)另外回傳。
    async fn drain(p: &impl LlmProvider) -> (Vec<StreamEvent>, Option<LlmError>) {
        let mut events = vec![];
        let mut error = None;
        let mut s = p.stream(request());
        while let Some(item) = s.next().await {
            match item {
                Ok(ev) => events.push(ev),
                Err(e) => error = Some(e),
            }
        }
        (events, error)
    }

    // `start_paused = true`:測試裡的時鐘是暫停的。程式 sleep 時,tokio 發現沒別的事可做,
    // 就直接把時間快轉過去。所以「等 10 秒」一瞬間就跑完,而且量到的經過時間是精確的。

    #[tokio::test(start_paused = true)]
    async fn retry_before_output_sends_the_start_events_only_once() {
        // 上一講的「情況 A」:開始事件出來之後才 529,重試後成功。
        let inner = Attempts::new(vec![
            vec![Ok(start()), Ok(text_start()), Err(overloaded)],
            answer(),
        ]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());
        let t = Instant::now();

        let (events, error) = drain(&p).await;

        assert!(error.is_none(), "{error:?}");
        // 下游只看到一次 MessageStart / TextStart,跟沒失敗過一樣。
        assert_eq!(
            events,
            [start(), text_start(), delta("你好"), text_end(), finish()]
        );
        assert_eq!(inner.calls(), ["stream", "stream"]);
        assert!(t.elapsed() <= Duration::from_secs(1), "第一次最多等 1 秒");
    }

    #[tokio::test(start_paused = true)]
    async fn error_after_output_is_passed_on_and_not_retried() {
        // 「情況 B」:使用者已經看到「你」才斷線,不能重來。
        let inner = Attempts::new(vec![
            vec![
                Ok(start()),
                Ok(text_start()),
                Ok(delta("你")),
                Err(|| LlmError::Transport("reset".into())),
            ],
            answer(),
        ]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());

        let (events, error) = drain(&p).await;

        assert_eq!(events, [start(), text_start(), delta("你")]);
        assert!(matches!(error, Some(LlmError::Transport(_))), "{error:?}");
        assert_eq!(inner.calls(), ["stream"], "只能呼叫一次");
    }

    #[tokio::test(start_paused = true)]
    async fn non_retryable_error_is_returned_at_once() {
        let inner = Attempts::new(vec![vec![Err(|| LlmError::http(401, None, ""))], answer()]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());
        let t = Instant::now();

        let (events, error) = drain(&p).await;

        assert!(events.is_empty());
        assert_eq!(error.as_ref().and_then(status), Some(401));
        assert_eq!(inner.calls(), ["stream"]);
        assert_eq!(t.elapsed(), Duration::ZERO, "不能重試就不該等");
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts_with_the_last_error() {
        let inner = Attempts::new(vec![
            vec![Err(overloaded)],
            vec![Err(overloaded)],
            vec![Err(|| LlmError::http(503, None, ""))],
            answer(),
        ]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());
        let t = Instant::now();

        let (_, error) = drain(&p).await;

        // 交出去的是「最後一次」的錯誤。
        assert_eq!(error.as_ref().and_then(status), Some(503));
        assert_eq!(inner.calls(), ["stream", "stream", "stream"]);
        assert!(t.elapsed() <= Duration::from_secs(3), "最多等 1 + 2 秒");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_from_the_provider_is_followed_exactly() {
        let inner = Attempts::new(vec![
            vec![Err(|| {
                LlmError::http(429, Some(Duration::from_secs(5)), "")
            })],
            answer(),
        ]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());
        let t = Instant::now();

        let (_, error) = drain(&p).await;

        assert!(error.is_none(), "{error:?}");
        assert_eq!(
            t.elapsed(),
            Duration::from_secs(5),
            "對方說 5 秒就是 5 秒,不加 jitter"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_beyond_max_delay_gives_up_without_waiting() {
        let inner = Attempts::new(vec![
            vec![Err(|| {
                LlmError::http(429, Some(Duration::from_secs(300)), "")
            })],
            answer(),
        ]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());
        let t = Instant::now();

        let (_, error) = drain(&p).await;

        assert_eq!(error.as_ref().and_then(status), Some(429));
        assert_eq!(inner.calls(), ["stream"]);
        assert_eq!(t.elapsed(), Duration::ZERO, "要等 5 分鐘就直接交給上層");
    }

    #[tokio::test(start_paused = true)]
    async fn held_events_are_released_when_the_answer_has_no_output() {
        // 少見但合法:整段回答沒有任何內容。扣住的事件最後要全部放行,不能吞掉。
        let inner = Attempts::new(vec![vec![Ok(start()), Ok(finish())]]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());

        let (events, error) = drain(&p).await;

        assert!(error.is_none(), "{error:?}");
        assert_eq!(events, [start(), finish()]);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_retries_through_the_inner_complete() {
        let inner = Attempts::new(vec![vec![Err(overloaded)], answer()]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());

        let resp = p.complete(request()).await.unwrap();

        assert_eq!(resp.stop.kind, StopKind::EndTurn);
        // 「complete」證明走的是裡面的 complete(它可能用自己的 Limits),不是收集 stream。
        assert_eq!(inner.calls(), ["complete", "complete"]);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_also_gives_up_after_max_attempts() {
        // `complete` 有自己的重試迴圈:次數沒有往上加的話,它會永遠重試下去。
        // (cargo-mutants:`attempt += 1` 換成 `-=` / `*=` 時沒有測試失敗;stream 那條路有測,這條沒有。)
        let inner = Attempts::new(vec![
            vec![Err(overloaded)],
            vec![Err(overloaded)],
            vec![Err(|| LlmError::http(503, None, ""))],
            answer(),
        ]);
        let p = Arc::clone(&inner).with_retry(RetryPolicy::default());

        let error = tokio::time::timeout(Duration::from_secs(60), p.complete(request()))
            .await
            .expect("應該在第 3 次之後放棄,而不是一直重試")
            .unwrap_err();

        assert_eq!(status(&error), Some(503));
        assert_eq!(inner.calls(), ["complete", "complete", "complete"]);
    }

    #[test]
    fn backoff_cap_doubles_and_stops_at_max_delay() {
        let policy = RetryPolicy::default();
        let caps: Vec<u64> = (1..=5).map(|n| policy.backoff_cap(n).as_secs()).collect();
        assert_eq!(caps, [1, 2, 4, 8, 10]);
        // 次數很大也不能溢位。
        assert_eq!(policy.backoff_cap(u32::MAX), Duration::from_secs(10));
    }

    #[test]
    fn jittered_wait_stays_within_the_cap() {
        let policy = RetryPolicy::default();
        for failed in 1..=2 {
            for _ in 0..200 {
                let wait = policy.next_wait(failed, &overloaded()).unwrap();
                assert!(wait <= policy.backoff_cap(failed), "{wait:?}");
            }
        }
    }

    #[test]
    fn jittered_wait_actually_varies() {
        // 上一個測試只保證「不超過上限」;永遠剛好等上限也會通過。這裡確認真的有隨機。
        // 200 次全部相同的機率趨近於 0,不會偶爾失敗。
        let policy = RetryPolicy::default();
        let waits: std::collections::BTreeSet<Duration> = (0..200)
            .map(|_| policy.next_wait(2, &overloaded()).unwrap())
            .collect();
        assert!(waits.len() > 1, "{waits:?}");
    }

    #[test]
    fn no_wait_once_attempts_are_used_up() {
        let policy = RetryPolicy::default();
        assert!(policy.next_wait(2, &overloaded()).is_some());
        assert!(policy.next_wait(3, &overloaded()).is_none());
    }
}
