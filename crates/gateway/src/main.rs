//! gateway 的 composition root:讀環境 → 檢查 → 組裝 →(第 16 步起)啟動 HTTP server。
//!
//! ```text
//! cp gateway.example.toml gateway.toml      # 改成你要的設定
//! cargo run -p gateway -- [--config <path>]  # 預設讀 ./gateway.toml
//! ```
//!
//! 整個程式只有這個檔案讀環境變數與設定檔(D-016);讀完就轉成型別,交給 lib 裡可以測試的部分。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use config_loader::ConfigError;
use gateway::config::{RawConfig, Settings};
use gateway::startup::{Secrets, assemble};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "gateway.toml";
const USAGE: &str = "usage: gateway [--config <path>]   (default: ./gateway.toml)";

// `anyhow::Result`:binary 不需要讓別人 match 錯誤種類,只需要把錯誤和它的原因鏈印給人看。
// main 回傳 Err 時,anyhow 會印出 `Error: ...` 加上每一層 `Caused by: ...`,並以非 0 結束。
// Java 對照:`main(...) throws Exception`,但印出來的是整條 cause 鏈。
fn main() -> Result<()> {
    init_tracing();
    // 找不到 .env 沒關係:真的環境變數一樣有效(例如部署時由系統設定)。
    dotenvy::dotenv().ok();

    let config_path = config_path(std::env::args().skip(1))?;
    let file = match config_loader::load_toml::<RawConfig>(&config_path) {
        Ok(file) => file,
        Err(ConfigError::NotFound(path)) => bail!(
            "config file {} not found; create it with `cp gateway.example.toml gateway.toml`",
            path.display()
        ),
        Err(e) => return Err(e.into()),
    };
    // closure 裡也能用 `?`:它自己回傳 Result。
    let settings = Settings::from_config(&file, || {
        Ok(config_loader::default_data_dir("gateway")?.join("records"))
    })?;
    let secrets = read_secrets()?;
    let gateway = assemble(settings, secrets).context("cannot start the gateway")?;

    print_summary(&file.dir.join(file_name(&config_path)), &gateway.settings);
    eprintln!("(the HTTP server arrives in step 16; this run only checks and assembles)");
    gateway.shutdown();
    Ok(())
}

/// 營運 log 印到 stderr。用 `RUST_LOG` 控制要印哪些,預設 info。
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// 解析命令列。只有一個選項,不值得為它加一個 CLI 框架。
fn config_path(mut args: impl Iterator<Item = String>) -> Result<PathBuf> {
    let mut path = PathBuf::from(DEFAULT_CONFIG);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = args.next().context("--config needs a path")?.into(),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument `{other}`\n{USAGE}"),
        }
    }
    Ok(path)
}

/// 兩個秘密都檢查完再報錯:缺兩個就一次說兩個。
fn read_secrets() -> Result<Secrets> {
    let api_key = config_loader::secret_env("ANTHROPIC_API_KEY");
    let token = config_loader::secret_env("GATEWAY_TOKEN");
    match (api_key, token) {
        (Ok(anthropic_api_key), Ok(gateway_token)) => Ok(Secrets {
            anthropic_api_key,
            gateway_token,
        }),
        (api_key, token) => {
            // `.err()`:Result → Option,只留下錯誤;`flatten` 把 None 濾掉。錯誤訊息裡只有變數名稱,沒有值。
            let missing: Vec<String> = [api_key.err(), token.err()]
                .into_iter()
                .flatten()
                .map(|e| format!("  - {e}"))
                .collect();
            bail!(
                "missing secrets (put them in .env, see .env.example):\n{}",
                missing.join("\n")
            )
        }
    }
}

fn file_name(path: &std::path::Path) -> &std::ffi::OsStr {
    path.file_name().unwrap_or(path.as_os_str())
}

/// 啟動時印出重點,特別是紀錄寫到哪裡:預設放在專案外,要讓人找得到(D-021)。
fn print_summary(config: &std::path::Path, s: &Settings) {
    let reach = if s.bind.ip().is_loopback() {
        "local only"
    } else {
        "REACHABLE FROM OTHER MACHINES"
    };
    let aliases: Vec<String> = s
        .aliases
        .iter()
        .map(|(a, m)| format!("{a} → {m}"))
        .collect();
    let records = match &s.record_dir {
        Some(dir) => dir.display().to_string(),
        None => "disabled".into(),
    };
    eprintln!("gateway: startup checks passed");
    eprintln!("  config   {}", config.display());
    eprintln!("  bind     {} ({reach})", s.bind);
    eprintln!("  aliases  {}", aliases.join(", "));
    eprintln!("  records  {records}");
    eprintln!(
        "  retry    up to {} attempts, waits {}..{} ms",
        s.retry.max_attempts,
        s.retry.first_delay.as_millis(),
        s.retry.max_delay.as_millis()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<PathBuf> {
        config_path(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn config_path_defaults_to_gateway_toml() {
        assert_eq!(parse(&[]).unwrap(), PathBuf::from("gateway.toml"));
        assert_eq!(
            parse(&["--config", "conf/gw.toml"]).unwrap(),
            PathBuf::from("conf/gw.toml")
        );
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--confg", "x"]).is_err());
    }
}
