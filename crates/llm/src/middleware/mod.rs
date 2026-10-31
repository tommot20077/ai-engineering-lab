//! Decorator:每一層都實作 `LlmProvider`,裡面裝著下一層(design-part2.md §12、D-018)。
//!
//! 包裝順序(D-020):`Record [ Retry [ Alias [ client ] ] ]`。
//!
//! 規則:每個 decorator 都要實作 `stream` **和** `complete`,各自交給裡面那一層的同名方法。
//! 只寫 `stream` 的話,`complete` 會走 trait 的預設方法,跳過裡面那一層覆寫的 `complete`。

mod alias;
mod record;
mod retry;

use std::sync::Arc;

pub use alias::{Alias, AliasTable};
pub use record::{CallRecord, Latency, Outcome, Record, RecordSink, RecordedError, SCHEMA_VERSION};
#[cfg(unix)]
pub use record::{DEFAULT_QUEUE_CAPACITY, JsonlWriter};
pub use retry::{Retry, RetryPolicy};

/// 組裝用的語法:`client.with_alias(table)`,讀起來像 provider 自己的方法。
///
/// 底層仍然是一層包一層的 decorator,每段邏輯只寫一次。
// 這是 extension trait:替「別人的型別」加方法。下面那行 blanket impl 讓所有 provider 自動擁有它。
// Java 沒有對應的語法;最接近的是 Kotlin 的 extension function。
// `Sized`:`with_alias(self)` 要把整個 provider「按值」搬進 `Alias`,大小必須在編譯期確定。
pub trait LlmProviderExt: crate::LlmProvider + Sized {
    fn with_alias(self, table: AliasTable) -> Alias<Self> {
        Alias::new(self, table)
    }

    fn with_retry(self, policy: RetryPolicy) -> Retry<Self> {
        Retry::new(self, policy)
    }

    fn with_record(self, sink: Arc<dyn RecordSink>) -> Record<Self> {
        Record::new(self, sink)
    }
}

impl<P: crate::LlmProvider> LlmProviderExt for P {}
