//! 依設定組裝 provider(design-part2.md §16、D-020)。
//!
//! 包裝順序:`Record [ Retry [ Alias [ AnthropicClient ] ] ]`。

use std::path::PathBuf;
use std::sync::Arc;

use llm::{
    AliasTable, AnthropicClient, AnthropicConfig, DEFAULT_QUEUE_CAPACITY, JsonlWriter, LlmError,
    LlmProvider, LlmProviderExt, RecordSink, SecretString,
};

use crate::config::Settings;

/// 從 `.env` / 環境變數讀到的秘密。
pub struct Secrets {
    pub anthropic_api_key: SecretString,
    pub gateway_token: SecretString,
}

/// 組裝好、可以開始服務的 gateway。
pub struct Gateway {
    pub settings: Settings,
    /// 整條 decorator 鏈。`Arc<dyn>`:依設定決定包不包 Record,型別要在執行期統一(D-019)。
    pub provider: Arc<dyn LlmProvider>,
    /// 另外留一份 writer:`/v1/status` 要讀遺失數,關機時要確實寫完。
    pub records: Option<Arc<JsonlWriter>>,
    /// `Arc`:每個請求的 AppState 都要讀它,共用同一份,不複製秘密。
    pub token: Arc<SecretString>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("cannot create the Anthropic client: {0}")]
    Client(#[from] LlmError),

    #[error("cannot use the records directory {}: {source}", dir.display())]
    Records {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 依設定組裝整條 provider 鏈。紀錄目錄在這裡建立與檢查:有問題就啟動失敗(D-021)。
pub fn assemble(settings: Settings, secrets: Secrets) -> Result<Gateway, StartupError> {
    // `..AnthropicConfig::new(key)`:struct update 語法。先用 `new` 取得全部的預設值,
    // 只覆寫 `base_url`。Java 對照:builder 的 `toBuilder().baseUrl(...).build()`。
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: settings.anthropic_base_url.clone(),
        ..AnthropicConfig::new(secrets.anthropic_api_key)
    })?;

    // 由內往外包(D-020)。這一段組合在編譯期就固定了,所以用模具(泛型)寫,
    // 型別是 `Retry<Alias<AnthropicClient>>`;到了「要不要包 Record」這個執行期才知道的分岔,
    // 才裝進盒子 `Arc<dyn LlmProvider>`,讓 if 的兩個分支型別一致。
    let core: Arc<dyn LlmProvider> = Arc::new(
        client
            .with_alias(AliasTable::strict(settings.aliases.clone()))
            .with_retry(settings.retry),
    );

    let (provider, records) = match &settings.record_dir {
        Some(dir) => {
            let writer = JsonlWriter::start(dir, DEFAULT_QUEUE_CAPACITY).map_err(|source| {
                StartupError::Records {
                    dir: dir.clone(),
                    source,
                }
            })?;
            let writer = Arc::new(writer);
            // 同一個 writer 有兩個身分:Record 把它當 `RecordSink` 用,gateway 自己留一份具體型別。
            // `Arc<JsonlWriter>` → `Arc<dyn RecordSink>`:compiler 自動轉換(unsizing coercion)。
            let sink: Arc<dyn RecordSink> = writer.clone();
            // `core.with_record(...)`:`core` 是 `Arc<dyn LlmProvider>`,能填進模具,靠的是第 10 步的 blanket impl。
            let provider: Arc<dyn LlmProvider> = Arc::new(core.with_record(sink));
            (provider, Some(writer))
        }
        None => (core, None),
    };

    Ok(Gateway {
        settings,
        provider,
        records,
        token: Arc::new(secrets.gateway_token),
    })
}

impl Gateway {
    /// 關機:丟掉 provider 鏈,再等紀錄寫完。
    ///
    /// 會阻塞(等寫檔執行緒),不能在 tokio 工作執行緒上直接呼叫;第 17 步會包進 `spawn_blocking`(design-part2.md §18)。
    pub fn shutdown(self) {
        // provider 鏈裡的 Record 也持有一份 writer:先丟掉它,writer 才會只剩我們這一份。
        drop(self.provider);
        let Some(writer) = self.records else {
            return;
        };
        // `Arc::try_unwrap`:如果只剩我們這一份,就把裡面的值拿出來;否則把 Arc 原樣還回來。
        match Arc::try_unwrap(writer) {
            Ok(writer) => writer.shutdown(),
            // 還有人(例如沒結束的請求)拿著它:沒辦法保證寫完,至少大聲說出來。
            Err(writer) => tracing::error!(
                holders = Arc::strong_count(&writer),
                "records writer is still in use at shutdown; queued records may be lost"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use futures::StreamExt;
    use llm::{
        CallRecord, Content, Message, Outcome, ProviderMetadata, Request, RetryPolicy, Role,
    };

    use super::*;

    fn settings(record_dir: Option<PathBuf>) -> Settings {
        Settings {
            bind: "127.0.0.1:8787".parse().unwrap(),
            allowed_hosts: vec![],
            aliases: BTreeMap::from([("cheap".into(), "claude-haiku-5-5".into())]),
            // 不會真的連線:這裡只測組裝,未知別名在送出前就被擋下。
            anthropic_base_url: "http://127.0.0.1:9".into(),
            record_dir,
            retry: RetryPolicy::default(),
            max_body_bytes: 1024,
            max_concurrent: 1,
        }
    }

    fn secrets() -> Secrets {
        Secrets {
            anthropic_api_key: SecretString::from("sk-ant-test"),
            gateway_token: SecretString::from("token"),
        }
    }

    // 一定要是「合法」的請求:messages 是空的話,AnthropicClient 的 validate() 也會回 InvalidRequest,
    // 測試就分不出是誰擋下的(第一次寫的時候就是這樣用錯的理由通過)。
    fn request(model: &str) -> Request {
        Request {
            model: model.into(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".into(),
                    meta: ProviderMetadata::default(),
                }],
            }],
            tools: vec![],
            max_tokens: 16,
            provider_options: ProviderMetadata::default(),
        }
    }

    #[tokio::test]
    async fn the_chain_is_record_outside_retry_outside_strict_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("records");
        let gateway = assemble(settings(Some(dir.clone())), secrets()).unwrap();

        // 打錯的別名:嚴格的 Alias 在送出前就擋下(證明 Alias 在鏈上、而且是嚴格模式)。
        let first = gateway
            .provider
            .stream(request("chaep"))
            .next()
            .await
            .unwrap();
        let Err(LlmError::InvalidRequest(message)) = first else {
            panic!("{first:?}")
        };
        assert!(
            message.starts_with("unknown model alias 'chaep'"),
            "{message}"
        );
        gateway.shutdown();

        // Record 在最外層:看到的是「換名字之前」的請求,而且這次失敗也被記下了。
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        let record: CallRecord = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(record.requested_model, "chaep");
        assert_eq!(record.outcome, Outcome::Error);
        assert_eq!(record.error.unwrap().kind, "invalid_request");
    }

    #[test]
    fn records_can_be_turned_off() {
        let gateway = assemble(settings(None), secrets()).unwrap();
        assert!(gateway.records.is_none());
        gateway.shutdown();
    }

    #[test]
    fn a_records_directory_others_can_read_stops_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("records");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = assemble(settings(Some(dir.clone())), secrets())
            .err()
            .expect("應該拒絕啟動");

        assert!(
            matches!(&err, StartupError::Records { dir: d, .. } if *d == dir),
            "{err}"
        );
        assert!(err.to_string().contains("chmod 700"), "{err}");
    }
}
