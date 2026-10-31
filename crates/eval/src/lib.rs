//! Eval harness(002,設計見 `docs/modules/002-eval-harness/design.md`)。
//!
//! 用數字回答「這樣改,回答變好還是變差?」:對資料集跑模型(`run`)、打分(`score`)、比較兩次執行(`compare`)。
//! Eval 不是測試(D-007):測試是確定性的 pass/fail;eval 是對資料集打分、看趨勢。

mod case;
mod dataset;

pub use case::{Case, ChatInput, Check, Input};
pub use dataset::{Dataset, DatasetError};
