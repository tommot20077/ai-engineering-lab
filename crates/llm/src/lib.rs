//! 與 provider 無關、以 streaming 為核心的 LLM client。
//!
//! 所有上層(gateway、agent、eval)都透過這個 crate 呼叫 LLM,不直接碰任何廠商的 API。
//! 設計見 `docs/modules/001-llm-gateway/design.md`。
//!
//! Rust 筆記:
//! - `//!` 註解的是「外面那一層」,這裡就是整個 crate;`///` 註解的是「下一個項目」。
//!   兩者都會被 `cargo doc --open` 產生成文件。
//! - crate 是 Rust 的編譯單位,最接近 Go 的 module。`lib.rs` 是一棵 module 樹的根,
//!   之後用 `mod xxx;` 一個個掛上去;沒掛上這棵樹的檔案根本不會被編譯。
//!   (Go 是同目錄的檔案自動屬於同一個 package,Rust 要明確宣告。)

// `mod` 本身是私有的:外面看不到 `llm::types` 這個路徑。
// 再用 `pub use` 把要公開的型別拉到 crate 最上層,外面寫 `llm::Request` 就好。
// 好處:檔案怎麼拆是內部細節,之後搬動檔案不會影響使用者。
// Go 對照:Go 的 package 路徑就是目錄,搬目錄就是 breaking change;Rust 把兩者分開了。
mod error;
mod stop;
mod types;

pub use error::{LlmError, Result};
pub use stop::{StopKind, StopReason};
pub use types::{Content, Message, ProviderMetadata, Request, Response, Role, ToolDef, Usage};
