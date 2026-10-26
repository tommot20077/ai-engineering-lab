//! Anthropic Messages API 的 adapter。
//!
//! 對外只公開 `AnthropicClient` 和它的設定;Anthropic 的 JSON 格式(`wire`)與翻譯規則(`translate`)
//! 都是這個 module 的內部細節。

mod client;
mod translate;
mod wire;

pub use client::{AnthropicClient, AnthropicConfig, DEFAULT_BASE_URL};
