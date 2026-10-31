//! LLM gateway:把 `llm` 包成 HTTP 服務的外殼(design-part2.md、D-018)。
//!
//! 這個 lib 只放「可以測試的部分」;`main.rs` 是 composition root,負責讀環境、組裝、啟動。

pub mod api;
pub mod config;
pub mod error;
mod guard;
pub mod startup;

#[cfg(test)]
mod testing;
