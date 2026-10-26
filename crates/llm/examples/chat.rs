//! 跟 Claude 串流對話的最小範例:回答一邊產生、一邊印出來。
//!
//! ```text
//! cp .env.example .env      # 填入 ANTHROPIC_API_KEY
//! cargo run -p llm --example chat -- "用一句話介紹 Rust"
//! ```

use std::io::Write;

use futures::StreamExt;
use llm::{
    AnthropicClient, AnthropicConfig, Content, LlmProvider, Message, ProviderMetadata, Request,
    Role, SecretString, StreamEvent,
};

const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
const DEFAULT_PROMPT: &str = "用一句話介紹 Rust 程式語言。";

// `#[tokio::main]`:把 `async fn main` 包成一般的 `main`,並啟動 tokio 的 async 執行環境。
// Java 對照:很像 main 一開始先建好一個 ExecutorService,再把工作丟進去跑。
//
// 回傳 `Result<(), Box<dyn Error>>`:任何實作了 `Error` 的錯誤都可以用 `?` 往外丟,
// main 回傳 `Err` 時程式會印出錯誤並以非 0 結束。小程式這樣寫最省事;
// 正式的 binary 通常改用 `anyhow`,錯誤訊息會更完整。
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 讀 .env 是 binary 的事,library 不做(D-016)。找不到 .env 也沒關係,真的環境變數一樣有效。
    dotenvy::dotenv().ok();

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY is not set (copy .env.example to .env)")?;
    let mut config = AnthropicConfig::new(SecretString::from(api_key));
    if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
        config.base_url = url;
    }
    let client = AnthropicClient::new(config)?;

    // 命令列參數(跳過第一個,那是程式本身的路徑)接起來當問題。
    let prompt: Vec<String> = std::env::args().skip(1).collect();
    let prompt = if prompt.is_empty() {
        DEFAULT_PROMPT.to_owned()
    } else {
        prompt.join(" ")
    };

    let request = Request {
        model: std::env::var("LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned()),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: prompt,
                meta: ProviderMetadata::default(),
            }],
        }],
        tools: vec![],
        max_tokens: 4096,
        provider_options: ProviderMetadata::default(),
    };

    let mut events = client.stream(request);
    let mut stdout = std::io::stdout();
    while let Some(event) = events.next().await {
        match event? {
            StreamEvent::TextDelta { text, .. } => {
                write!(stdout, "{text}")?;
                // stdout 預設有緩衝,不 flush 的話要等一整行才看得到,就沒有「串流」的感覺了。
                stdout.flush()?;
            }
            // 用量印到 stderr:stdout 只放回答本身,方便用 `>` 導到檔案。
            StreamEvent::Finish { stop, usage, .. } => eprintln!(
                "\n\n[stop: {} · input {} tokens · output {} tokens]",
                stop.raw, usage.input_tokens, usage.output_tokens
            ),
            // 這個範例只關心文字;思考、工具呼叫等其他事件略過。
            _ => {}
        }
    }
    Ok(())
}
