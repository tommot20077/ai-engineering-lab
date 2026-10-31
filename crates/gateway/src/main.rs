//! gateway 的 composition root:讀環境 → 檢查 → 組裝 → 啟動 HTTP server → 優雅關機。
//!
//! ```text
//! cp gateway.example.toml gateway.toml      # 改成你要的設定
//! cargo run -p gateway -- [--config <path>]  # 預設讀 ./gateway.toml
//! ```
//!
//! 整個程式只有這個檔案讀環境變數與設定檔(D-016);讀完就轉成型別,交給 lib 裡可以測試的部分。

use std::future::IntoFuture;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use config_loader::ConfigError;
use gateway::api::{AppState, router};
use gateway::config::{RawConfig, Settings};
use gateway::startup::{Secrets, assemble};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "gateway.toml";
/// 收到停止信號後,最多等進行中的請求多久(design-part2.md §18)。
const GRACE: Duration = Duration::from_secs(30);
const USAGE: &str = "usage: gateway [--config <path>]   (default: ./gateway.toml)";

// `anyhow::Result`:binary 不需要讓別人 match 錯誤種類,只需要把錯誤和它的原因鏈印給人看。
// main 回傳 Err 時,anyhow 會印出 `Error: ...` 加上每一層 `Caused by: ...`,並以非 0 結束。
// Java 對照:`main(...) throws Exception`,但印出來的是整條 cause 鏈。
//
// `#[tokio::main]`:把 async main 包成一般的 main,並建立多執行緒的 runtime。
#[tokio::main]
async fn main() -> Result<()> {
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

    let listener = TcpListener::bind(gateway.settings.bind)
        .await
        .with_context(|| format!("cannot listen on {}", gateway.settings.bind))?;
    print_summary(&file.dir.join(file_name(&config_path)), &gateway.settings);
    eprintln!(
        "listening on http://{} (Ctrl+C to stop)",
        gateway.settings.bind
    );

    // AppState 複製的是各個 Arc;`gateway` 本身留著,關機時要用它確實寫完紀錄。
    serve(listener, router(AppState::from(&gateway))).await?;

    // 寫完紀錄:JsonlWriter 的 Drop 會 join 寫檔執行緒(會阻塞),
    // 不能在 tokio 的工作執行緒上做,所以交給專門跑阻塞工作的執行緒(design-part2.md §18)。
    // 一定要明確做:main 結束時,其他執行緒上的物件不會執行 Drop,佇列裡的紀錄會直接遺失。
    tokio::task::spawn_blocking(move || gateway.shutdown())
        .await
        .context("records writer did not shut down cleanly")?;
    eprintln!("gateway: stopped");
    Ok(())
}

/// 開始服務;收到 Ctrl+C / SIGTERM 時停止接受新連線,等進行中的請求做完,最多等 `GRACE`。
async fn serve(listener: TcpListener, app: Router) -> Result<()> {
    // `watch` channel:一個人寫、很多人讀的「目前狀態」。這裡只有一個狀態:要不要停。
    // Java 對照:一個 `volatile boolean` 加上等待它變成 true 的機制。
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        // 送不出去 = 已經沒人在等了(server 自己結束了),不用處理。
        let _ = stop_tx.send(true);
    });

    let server = axum::serve(listener, app)
        .with_graceful_shutdown(stopped(stop_rx.clone()))
        // `with_graceful_shutdown` 回傳的是「可以變成 future 的東西」(IntoFuture);
        // `select!` 要的是 future 本身,所以明確轉一次。
        .into_future();

    // 兩件事同時等,先完成的勝出:
    // - server 自己結束(進行中的請求都做完了)→ 正常;
    // - 停止信號之後又過了 GRACE → 不等了。
    tokio::select! {
        result = server => result.context("server error"),
        () = async { stopped(stop_rx).await; tokio::time::sleep(GRACE).await } => {
            tracing::warn!("requests still running after {} s; stopping anyway", GRACE.as_secs());
            Ok(())
        }
    }
}

/// 等到「要停了」。
async fn stopped(mut stop: watch::Receiver<bool>) {
    // 等錯誤 = 送的一方不見了,也當成要停。
    let _ = stop.wait_for(|stop| *stop).await;
}

/// 等 Ctrl+C(SIGINT)或 SIGTERM(`kill`、Docker / systemd 停止服務時送的)。
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot listen for Ctrl+C");
            // 監聽失敗就永遠不觸發,讓另一個信號決定。
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!(
        "shutdown signal received; finishing in-flight requests (up to {} s)",
        GRACE.as_secs()
    );
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
