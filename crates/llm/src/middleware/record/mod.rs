//! 每次呼叫留下一筆完整紀錄,階段 2 資料飛輪的原料(design-part2.md §15、D-020、D-021)。

// 寫檔要用 Unix 專用的權限 API;其他平台不編譯這個模組,library 其餘部分照常可用。
#[cfg(unix)]
mod jsonl;
mod model;

use std::sync::Arc;
use std::time::Duration;

use async_stream::try_stream;
use futures::StreamExt;
use futures::future::BoxFuture;
use jiff::Timestamp;
use tokio::time::Instant;
use uuid::Uuid;

use crate::accumulate::Accumulator;
use crate::error::{LlmError, Result};
use crate::event::StreamEvent;
use crate::provider::{EventStream, LlmProvider, is_output};
use crate::types::{Request, Response};

#[cfg(unix)]
pub use jsonl::{DEFAULT_QUEUE_CAPACITY, JsonlWriter};
pub use model::{CallRecord, Latency, Outcome, RecordedError, SCHEMA_VERSION};

/// 紀錄要送去哪裡。這是擴展點(儲存後端):目前有 JSONL 檔案,測試用記憶體。
pub trait RecordSink: Send + Sync {
    /// 交出一筆紀錄。**不可阻塞、不回傳錯誤**:寫不進去由 sink 自己計數並大聲報錯(D-021)。
    /// 記錄是附帶工作,不能拖慢或拖垮回答本身。
    fn submit(&self, record: CallRecord);
}

/// 記下每一次呼叫的 decorator。放在最外層(D-020):一次呼叫一筆,看得到別名與總延遲。
pub struct Record<P> {
    inner: P,
    sink: Arc<dyn RecordSink>,
}

impl<P> Record<P> {
    pub fn new(inner: P, sink: Arc<dyn RecordSink>) -> Self {
        Self { inner, sink }
    }
}

impl<P: LlmProvider> LlmProvider for Record<P> {
    fn stream(&self, request: Request) -> EventStream {
        let mut pending = Pending::new(Arc::clone(&self.sink), request.clone());
        let mut events = self.inner.stream(request);

        // `pending` 被「搬進」這個串流:串流活多久,它就活多久;串流被丟掉,它也跟著被丟掉,
        // 這時它的 `Drop` 會把還沒送出的紀錄記成 cancelled。
        Box::pin(try_stream! {
            while let Some(item) = events.next().await {
                match item {
                    // 邊抄邊傳:抄一份給紀錄,原封不動地往外送。不像 Retry 那樣扣住事件。
                    Ok(event) => {
                        pending.observe(&event);
                        yield event;
                    }
                    // 先記錄,再把錯誤往外送:`Err(..)?` 之後串流就結束了,沒有「之後」可以記。
                    Err(error) => {
                        pending.failed(&error);
                        Err(error)?;
                    }
                }
            }
            pending.ended();
        })
    }

    // 規則:交給裡面那一層的 `complete`,在外面記下結果。
    fn complete(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            // 呼叫者如果不等了(把這個 future 丟掉),`pending` 也會被丟掉 → 記成 cancelled。
            let mut pending = Pending::new(Arc::clone(&self.sink), request.clone());
            let result = self.inner.complete(request).await;
            match &result {
                Ok(response) => pending.close(Outcome::Ok, Some(response.clone()), None),
                // 記的是斷線的原因(cause),回應是斷線前收到的部分。
                Err(LlmError::Interrupted { partial, cause }) => pending.close(
                    Outcome::Interrupted,
                    Some(partial.as_ref().clone()),
                    Some(cause.as_ref()),
                ),
                Err(error) => pending.close(Outcome::Error, None, Some(error)),
            }
            result
        })
    }
}

/// 一筆還沒送出的紀錄。
///
/// 一定會送出剛好一次:正常走完由 `close` 送;中途被丟掉由 `Drop` 送(cancelled)。
// `draft: Option<Draft>`:送出時用 `take()` 把內容拿走、留下 `None`。
// 之後 `Drop` 看到 `None` 就知道已經送過了,不會重複送。
struct Pending {
    sink: Arc<dyn RecordSink>,
    draft: Option<Draft>,
}

struct Draft {
    id: Uuid,
    started_at: Timestamp,
    // tokio 的 `Instant`:平常就是系統時鐘;在暫停時間的測試裡,它跟著快轉,延遲才量得準。
    started: Instant,
    request: Request,
    // 用第一部分的 Accumulator 收一份副本,斷線或取消時才拿得到「部分回應」。
    acc: Accumulator,
    saw_start: bool,
    first_output: Option<Duration>,
    // 裡面那一層吐出了不合協定的事件。紀錄不能中斷串流,所以先記下來,收尾時當成錯誤。
    broken: Option<LlmError>,
}

impl Pending {
    fn new(sink: Arc<dyn RecordSink>, request: Request) -> Self {
        Self {
            sink,
            draft: Some(Draft {
                id: Uuid::now_v7(),
                started_at: Timestamp::now(),
                started: Instant::now(),
                request,
                acc: Accumulator::new(),
                saw_start: false,
                first_output: None,
                broken: None,
            }),
        }
    }

    /// 抄下一個事件。收到 `Finish` 就立刻送出:有些呼叫者讀到 Finish 就停了,不算取消。
    fn observe(&mut self, event: &StreamEvent) {
        // `as_mut()`:借出 Option 裡面的東西來改,而不是把它拿走。
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        if matches!(event, StreamEvent::MessageStart { .. }) {
            draft.saw_start = true;
        }
        if draft.first_output.is_none() && is_output(event) {
            draft.first_output = Some(draft.started.elapsed());
        }
        if draft.broken.is_none()
            && let Err(e) = draft.acc.push(event.clone())
        {
            draft.broken = Some(e);
        }
        if draft.broken.is_none() && draft.acc.is_finished() {
            self.close_stream(None);
        }
    }

    /// 串流送來一個錯誤(之後就結束了)。
    fn failed(&mut self, error: &LlmError) {
        self.close_stream(Some(error));
    }

    /// 串流結束了,但沒收到 Finish 也沒收到錯誤:當成協定錯誤,跟 `collect` 的規則一樣。
    fn ended(&mut self) {
        let error = LlmError::protocol("stream ended before finish");
        self.close_stream(Some(&error));
    }

    /// 串流的收尾:依照「有沒有輸出過」決定是 error 還是 interrupted。
    fn close_stream(&mut self, error: Option<&LlmError>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        // 不合協定的事件優先:它比之後的任何錯誤都更早發生。下游的 `collect` 在那一刻就會拒絕,
        // 所以記成 error,也不附回應(違規之後收到的內容不可靠)。
        if let Some(broken) = draft.broken.take() {
            self.close(Outcome::Error, None, Some(&broken));
            return;
        }
        let produced = draft.first_output.is_some();
        match error {
            None => {
                // 走到這裡代表 Accumulator 已經收到 Finish。
                // `mem::take` 把 Accumulator 拿出來(留下一個新的空的),因為 `finish(self)` 會消耗它。
                let acc = std::mem::take(&mut draft.acc);
                match acc.finish() {
                    Ok(response) => self.close(Outcome::Ok, Some(response), None),
                    Err(e) => self.close(Outcome::Error, None, Some(&e)),
                }
            }
            Some(error) if produced => {
                let partial = std::mem::take(&mut draft.acc).into_partial();
                self.close(Outcome::Interrupted, Some(partial), Some(error));
            }
            Some(error) => self.close(Outcome::Error, None, Some(error)),
        }
    }

    /// 組成紀錄並送出。只有第一次呼叫有效。
    fn close(&mut self, outcome: Outcome, response: Option<Response>, error: Option<&LlmError>) {
        // `take()`:把 draft 拿走,留下 None。第二次呼叫(或之後的 Drop)就什麼都不做。
        let Some(draft) = self.draft.take() else {
            return;
        };
        let record = CallRecord {
            schema_version: SCHEMA_VERSION,
            id: draft.id,
            started_at: draft.started_at,
            requested_model: draft.request.model.clone(),
            served_model: response.as_ref().map(|r| r.model.clone()),
            usage: response.as_ref().map(|r| r.usage).unwrap_or_default(),
            request: draft.request,
            response,
            outcome,
            error: error.map(RecordedError::from),
            latency_ms: Latency {
                first_output: draft.first_output.map(millis),
                total: millis(draft.started.elapsed()),
            },
        };
        self.sink.submit(record);
    }
}

// `Drop`:這個值被丟掉的那一刻,Rust 自動呼叫它。Java 對照:像是 try-finally,但掛在物件上,
// 不管是正常結束、提早 return、還是呼叫者把串流整個丟掉,都一定會執行。
impl Drop for Pending {
    fn drop(&mut self) {
        // 還沒送出就被丟掉 = 呼叫者不要了。有收到開頭,就把目前的部分一起記下。
        let partial = match self.draft.as_mut() {
            Some(draft) if draft.saw_start && draft.broken.is_none() => {
                Some(std::mem::take(&mut draft.acc).into_partial())
            }
            _ => None,
        };
        self.close(Outcome::Cancelled, partial, None);
    }
}

/// `Duration` → 毫秒。超過 u64 範圍(五億年)就飽和,不會溢位。
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::stream;

    use super::*;
    use crate::middleware::LlmProviderExt;
    use crate::{Content, Limits, ProviderMetadata, StopKind, StopReason, Usage, collect};

    /// 測試用的 sink:把紀錄存在記憶體裡。
    #[derive(Default)]
    struct MemorySink(Mutex<Vec<CallRecord>>);

    impl MemorySink {
        fn records(&self) -> Vec<CallRecord> {
            self.0.lock().unwrap().clone()
        }

        /// 大部分測試都要確認「剛好一筆」:不能漏記,也不能重複記。
        fn only(&self) -> CallRecord {
            let records = self.records();
            assert_eq!(records.len(), 1, "應該剛好一筆:{records:?}");
            records.into_iter().next().unwrap()
        }
    }

    impl RecordSink for MemorySink {
        fn submit(&self, record: CallRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    /// 一個步驟 = 先等幾毫秒,再吐出一個項目。用來製造可預測的延遲。
    type Step = (u64, std::result::Result<StreamEvent, fn() -> LlmError>);

    /// 假的 provider:每次呼叫都照同一份步驟表吐出事件,並記下被呼叫的方法。
    struct Timed {
        steps: Vec<Step>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Timed {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps,
                calls: Mutex::new(vec![]),
            }
        }

        fn events(&self) -> EventStream {
            let steps = self.steps.clone();
            Box::pin(stream::iter(steps).then(|(ms, item)| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                item.map_err(|make_error| make_error())
            }))
        }
    }

    impl LlmProvider for Timed {
        fn stream(&self, _request: Request) -> EventStream {
            self.calls.lock().unwrap().push("stream");
            self.events()
        }

        fn complete(&self, _request: Request) -> BoxFuture<'_, Result<Response>> {
            self.calls.lock().unwrap().push("complete");
            Box::pin(collect(self.events(), Limits::default()))
        }
    }

    fn request() -> Request {
        Request {
            model: "cheap".into(),
            system: Some("你是助理".into()),
            messages: vec![],
            tools: vec![],
            max_tokens: 64,
            provider_options: ProviderMetadata::default(),
        }
    }

    fn start() -> StreamEvent {
        StreamEvent::MessageStart {
            id: Some("msg_1".into()),
            model: "claude-haiku-5-5".into(),
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
            usage: Usage {
                input_tokens: 12,
                output_tokens: 3,
                ..Usage::default()
            },
            meta: ProviderMetadata::default(),
        }
    }

    /// 「你好」:開始 100ms、第一個字 +200ms、結束 +300ms。
    fn answer() -> Vec<Step> {
        vec![
            (100, Ok(start())),
            (0, Ok(text_start())),
            (200, Ok(delta("你好"))),
            (0, Ok(text_end())),
            (300, Ok(finish())),
        ]
    }

    fn text(response: &Option<Response>) -> Vec<String> {
        response
            .iter()
            .flat_map(|r| &r.content)
            .filter_map(|c| match c {
                Content::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn setup(steps: Vec<Step>) -> (Arc<Timed>, Arc<MemorySink>, impl LlmProvider) {
        let inner = Arc::new(Timed::new(steps));
        let sink = Arc::new(MemorySink::default());
        let p = Arc::clone(&inner).with_record(Arc::clone(&sink) as Arc<dyn RecordSink>);
        (inner, sink, p)
    }

    #[tokio::test(start_paused = true)]
    async fn successful_stream_is_recorded_once_and_passes_through_unchanged() {
        let (_, sink, p) = setup(answer());

        let events: Vec<StreamEvent> = p.stream(request()).map(|e| e.unwrap()).collect().await;

        // Record 只抄不改:下游收到的跟裡面吐出的一模一樣。
        assert_eq!(
            events,
            [start(), text_start(), delta("你好"), text_end(), finish()]
        );
        let r = sink.only();
        assert_eq!(r.schema_version, SCHEMA_VERSION);
        assert_eq!(r.id.get_version_num(), 7, "UUID v7");
        assert_eq!(r.outcome, Outcome::Ok);
        assert_eq!(r.requested_model, "cheap");
        assert_eq!(r.served_model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(r.request, request());
        assert_eq!(text(&r.response), ["你好"]);
        assert_eq!(r.usage.output_tokens, 3);
        assert_eq!(r.error, None);
        assert_eq!(
            r.latency_ms,
            Latency {
                first_output: Some(300),
                total: 600
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn error_before_output_is_recorded_as_error_without_response() {
        let (_, sink, p) = setup(vec![
            (0, Ok(start())),
            (50, Err(|| LlmError::http(529, None, ""))),
        ]);

        let _: Vec<_> = p.stream(request()).collect().await;

        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Error);
        assert_eq!(r.response, None);
        assert_eq!(r.served_model, None);
        let error = r.error.unwrap();
        assert_eq!((error.kind.as_str(), error.status), ("http", Some(529)));
        assert_eq!(
            r.latency_ms,
            Latency {
                first_output: None,
                total: 50
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn error_after_output_is_recorded_as_interrupted_with_the_partial_answer() {
        let (_, sink, p) = setup(vec![
            (0, Ok(start())),
            (0, Ok(text_start())),
            (0, Ok(delta("你"))),
            (0, Err(|| LlmError::Transport("reset".into()))),
        ]);

        let _: Vec<_> = p.stream(request()).collect().await;

        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Interrupted);
        assert_eq!(text(&r.response), ["你"]);
        assert_eq!(r.error.unwrap().kind, "transport");
    }

    #[tokio::test(start_paused = true)]
    async fn stream_that_ends_without_finish_is_recorded_as_interrupted() {
        let (_, sink, p) = setup(vec![
            (0, Ok(start())),
            (0, Ok(text_start())),
            (0, Ok(delta("你"))),
        ]);

        let _: Vec<_> = p.stream(request()).collect().await;

        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Interrupted);
        assert_eq!(
            r.error.unwrap().message,
            "malformed response from provider: stream ended before finish"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn protocol_violation_is_recorded_but_does_not_alter_the_stream() {
        // 一開始就有一個不合協定的 TextStart(在 MessageStart 之前),之後是一段完全正常的回答。
        // 「之後正常」很重要:如果 Record 忽略了違規,這筆就會被記成 ok,測試才抓得到。
        let mut steps = vec![(0, Ok(text_start()))];
        steps.extend(answer());
        let (_, sink, p) = setup(steps);

        let events: Vec<StreamEvent> = p.stream(request()).map(|e| e.unwrap()).collect().await;

        // Record 只是旁觀者:照樣轉送,不中斷串流。
        assert_eq!(events.len(), 6);
        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Error);
        assert_eq!(r.response, None);
        let error = r.error.unwrap();
        assert_eq!(error.kind, "protocol");
        assert!(
            error.message.contains("before message_start"),
            "{}",
            error.message
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_stream_midway_is_recorded_as_cancelled() {
        let (_, sink, p) = setup(answer());

        let mut s = p.stream(request());
        for _ in 0..3 {
            s.next().await.unwrap().unwrap(); // start、text_start、「你好」
        }
        drop(s); // 使用者關掉視窗

        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Cancelled);
        assert_eq!(text(&r.response), ["你好"]);
        assert_eq!(r.error, None);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_right_after_finish_still_counts_as_ok() {
        // 有些呼叫者收到 Finish 就不再讀了(不會等到串流回傳 None)。這不算取消。
        let (_, sink, p) = setup(answer());

        let mut s = p.stream(request());
        while let Some(event) = s.next().await {
            if matches!(event.unwrap(), StreamEvent::Finish { .. }) {
                break;
            }
        }
        drop(s);

        assert_eq!(sink.only().outcome, Outcome::Ok);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_dropped_before_it_is_read_is_cancelled_without_response() {
        let (_, sink, p) = setup(answer());

        drop(p.stream(request()));

        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Cancelled);
        assert_eq!(r.response, None);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_is_recorded_through_the_inner_complete() {
        let (inner, sink, p) = setup(answer());

        let resp = p.complete(request()).await.unwrap();

        assert_eq!(resp.usage.output_tokens, 3);
        assert_eq!(*inner.calls.lock().unwrap(), ["complete"]);
        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Ok);
        assert_eq!(text(&r.response), ["你好"]);
        // complete() 看不到逐字的事件,所以沒有「第一個字」的時間。
        assert_eq!(
            r.latency_ms,
            Latency {
                first_output: None,
                total: 600
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interrupted_complete_keeps_the_partial_answer() {
        let (_, sink, p) = setup(vec![
            (0, Ok(start())),
            (0, Ok(text_start())),
            (0, Ok(delta("你"))),
            (0, Err(|| LlmError::Transport("reset".into()))),
        ]);

        let err = p.complete(request()).await.unwrap_err();

        assert!(matches!(err, LlmError::Interrupted { .. }), "{err:?}");
        let r = sink.only();
        assert_eq!(r.outcome, Outcome::Interrupted);
        assert_eq!(text(&r.response), ["你"]);
        // 記的是斷線的「原因」,不是外面那層 Interrupted。
        assert_eq!(r.error.unwrap().kind, "transport");
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_complete_is_recorded_as_cancelled() {
        let (_, sink, p) = setup(answer());

        // 只等 10ms,答案要 600ms:時間到了 future 就被丟掉,等於呼叫者不要了。
        let result = tokio::time::timeout(Duration::from_millis(10), p.complete(request())).await;

        assert!(result.is_err(), "應該逾時");
        assert_eq!(sink.only().outcome, Outcome::Cancelled);
    }
}
