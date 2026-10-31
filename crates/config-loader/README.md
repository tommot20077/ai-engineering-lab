# config-loader

這個 lab 裡所有 binary（gateway，以及之後的 eval、個人助理）共用的設定讀取工具。
目的是讓每個程式處理路徑、設定檔和秘密的方式都一樣。

**只給 binary 用。** library（例如 `llm`）不依賴它，只從 constructor 接收設定值（D-016）。

## 規則

| 項目 | 規則 |
|---|---|
| `~` | 只展開開頭的 `~/`（以及單獨的 `~`）。`~bob/...` 會被拒絕 |
| `$VAR` | **不展開**，就是普通的文字 |
| 相對路徑 | 以**設定檔所在的目錄**為準，不以執行指令的目錄為準（和 Cargo 一樣） |
| 預設資料目錄 | macOS：`~/Library/Application Support/ai-engineering-lab/<app>`<br>其他：`$XDG_DATA_HOME/ai-engineering-lab/<app>`，沒有設定就用 `~/.local/share/...` |
| 設定檔 | TOML。錯誤訊息包含檔名、行號和欄號；找不到檔案時回傳 `ConfigError::NotFound` |
| 秘密 | 從環境變數讀取，直接包成 `SecretString`。沒有設定或只有空白都算錯誤；錯誤訊息只會出現變數名稱 |

## 用法

```rust
use config_loader::{ConfigFile, load_toml, secret_env};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // 打錯的欄位要報錯，不能被默默忽略
struct Config {
    records_dir: String,
}

let config: ConfigFile<Config> = load_toml("gateway.toml".as_ref())?;
let records = config.resolve(&config.value.records_dir)?; // "./records" → <設定檔目錄>/records
let token = secret_env("GATEWAY_TOKEN")?;                   // SecretString
```

## 測試

```text
cargo test -p config-loader
```

Rust 2024 把 `std::env::set_var` 改成了 `unsafe`，而這個 workspace 禁止使用 `unsafe`。
所以每個函式都拆成兩層：外層讀取真正的環境變數，內層是值從參數傳入的純函式。測試只測內層。
