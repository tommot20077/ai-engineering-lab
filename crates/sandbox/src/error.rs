//! 沙箱的錯誤。程式本身的失敗(編譯錯誤、逾時)不是錯誤,在 `Report` 裡。

/// 沙箱本身不能用。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// 設定不合法(image 沒有用 digest 鎖定、上限是 0)。建立 `ContainerSandbox` 時就擋下(fail-loud)。
    #[error("invalid sandbox configuration: {0}")]
    InvalidConfig(String),

    /// 原始碼超過 `Limits::code_bytes`。
    #[error("the source code is larger than {limit} bytes")]
    TooLarge { limit: usize },

    /// 容器指令啟動不了(沒有安裝、不在 PATH 裡)。
    #[error("could not run `{program}` (is it installed?): {reason}")]
    Unavailable { program: String, reason: String },

    /// image 不在本機。我們不自動下載(`--pull never`):1 GB 以上的下載不該在工具呼叫裡默默發生。
    #[error("the sandbox image is not available locally; run `{program} pull {image}` first")]
    ImageMissing { program: String, image: String },

    /// 容器起不來、或沙箱自己的步驟失敗(daemon 沒開、寫不進原始碼)。`detail` 是容器指令的 stderr 開頭,已經整理成可以印的。
    #[error("the sandbox failed: {detail}")]
    Container { detail: String },
}
